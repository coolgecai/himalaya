use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::time::Instant;

use serde_json::{Map, Value};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use telemetry::SessionTracer;

use crate::compact::{
    compact_session, estimate_session_tokens, CompactionConfig, CompactionResult,
};
use crate::config::{DecisioningConfig, RuntimeFeatureConfig};
use crate::hooks::{HookAbortSignal, HookProgressReporter, HookRunResult, HookRunner};
use crate::permissions::{
    PermissionContext, PermissionOutcome, PermissionOverride, PermissionPolicy, PermissionPrompter,
};
use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
use crate::usage::{TokenUsage, UsageTracker};
use crate::{
    infer_tool_capabilities, tool_from_profile, DecisioningEngine, DecisioningEvent,
    DecisioningEventKind, DecisioningSnapshot, ReasoningContext, RiskAssessment, SafetyOutcome,
    SafetyPolicy, StepOutcome, Subtask, Task, Tool, ToolHistoryEntry, ToolSelector,
};

const DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD: u32 = 100_000;
const AUTO_COMPACTION_THRESHOLD_ENV_VAR: &str = "Himalaya_CODE_AUTO_COMPACT_INPUT_TOKENS";

/// Fully assembled request payload sent to the upstream model client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    pub system_prompt: Vec<String>,
    pub messages: Vec<ConversationMessage>,
}

/// Streamed events emitted while processing a single assistant turn.
#[derive(Debug, Clone, PartialEq)]
pub enum AssistantEvent {
    TextDelta(String),
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    ReasoningStep(ReasoningStep),
    Usage(TokenUsage),
    PromptCache(PromptCacheEvent),
    MessageStop,
}

/// Reasoning step in the chain of thought process.
#[derive(Debug, Clone, PartialEq)]
#[derive(Serialize, Deserialize)]
pub enum ReasoningStep {
    Analysis {
        content: String,
        confidence: Option<f32>,
    },
    Planning {
        plan: String,
        steps: Vec<String>,
    },
    Reflection {
        critique: String,
        adjustment: Option<String>,
    },
    Decision {
        choice: String,
        reasoning: String,
    },
}

/// Problem analysis produced during the Analyze phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProblemAnalysis {
    pub summary: String,
    pub details: Option<String>,
    pub confidence: Option<f32>,
}

/// Execution plan suggested by the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub plan: String,
    pub steps: Vec<String>,
}

/// Self-critique produced during reflection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelfCritique {
    pub critique: String,
    pub suggested_adjustment: Option<String>,
}

/// Strategy adjustment suggested to adapt future behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyAdjustment {
    pub description: String,
}

/// Alternative approach suggestion recorded alongside ChainOfThought.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlternativeApproach {
    pub description: String,
    pub confidence: Option<f32>,
}

/// A lightweight Chain-of-Thought container for collected reasoning steps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChainOfThought {
    pub steps: Vec<ReasoningStep>,
    pub confidence: f32,
    pub alternatives: Vec<AlternativeApproach>,
}

impl ChainOfThought {
    #[must_use]
    pub fn new() -> Self {
        Self {
            steps: Vec::new(),
            confidence: 0.0,
            alternatives: Vec::new(),
        }
    }

    pub fn add_step(&mut self, step: ReasoningStep) {
        self.steps.push(step);
        self.recompute_confidence();
    }

    fn recompute_confidence(&mut self) {
        // Simple heuristic: average available confidences from Analysis steps
        let mut sum = 0.0f32;
        let mut count = 0usize;
        for s in &self.steps {
            if let ReasoningStep::Analysis { confidence, .. } = s {
                if let Some(c) = confidence {
                    sum += *c;
                    count += 1;
                }
            }
        }
        if count > 0 {
            self.confidence = (sum / count as f32).clamp(0.0, 1.0);
        } else {
            // default neutral confidence
            self.confidence = 0.5;
        }
    }
}

/// Simple persistent long-term memory for the runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub topic: String,
    pub note: String,
    pub confidence: f32,
    pub ts_ms: u64,
}

#[derive(Debug)]
pub struct LongTermMemory {
    pub path: PathBuf,
    pub entries: Vec<MemoryEntry>,
}

impl LongTermMemory {
    #[must_use]
    pub fn load_for_workspace(workspace_root: Option<&std::path::Path>) -> Self {
        let path = workspace_root
            .map(|root| root.join(".Himalaya").join("long_term_memory.json"))
            .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".Himalaya").join("knowledge.json")))
            .unwrap_or_else(|| PathBuf::from(".Himalaya/knowledge.json"));

        if let Ok(contents) = fs::read_to_string(&path) {
            if let Ok(entries) = serde_json::from_str::<Vec<MemoryEntry>>(&contents) {
                return Self { path, entries };
            }
        }

        Self {
            path,
            entries: Vec::new(),
        }
    }

    pub fn add_entry(&mut self, topic: impl Into<String>, note: impl Into<String>, confidence: f32) {
        let entry = MemoryEntry {
            topic: topic.into(),
            note: note.into(),
            confidence: confidence.clamp(0.0, 1.0),
            ts_ms: current_time_millis(),
        };
        self.entries.push(entry);
        let _ = self.save();
    }

    #[must_use]
    pub fn ranked_topics(&self, limit: usize) -> Vec<String> {
        let mut ranked_entries = self.entries.iter().collect::<Vec<_>>();
        ranked_entries.sort_by(|left, right| {
            right
                .confidence
                .partial_cmp(&left.confidence)
                .unwrap_or(Ordering::Equal)
                .then_with(|| right.ts_ms.cmp(&left.ts_ms))
                .then_with(|| left.topic.cmp(&right.topic))
        });

        let mut seen = BTreeSet::new();
        let mut topics = Vec::new();
        for entry in ranked_entries {
            let topic = entry.topic.trim();
            if topic.is_empty() || !seen.insert(topic.to_string()) {
                continue;
            }

            topics.push(topic.to_string());
            if topics.len() >= limit {
                break;
            }
        }

        topics
    }

    pub fn save(&self) -> Result<(), std::io::Error> {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let serialized = serde_json::to_string_pretty(&self.entries).unwrap_or_else(|_| "[]".to_string());
        fs::write(&self.path, serialized)
    }
}

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn reasoning_step_summary(step: &ReasoningStep) -> String {
    match step {
        ReasoningStep::Analysis { content, .. } => content.clone(),
        ReasoningStep::Planning { plan, steps } => {
            let mut summary = plan.clone();
            if !steps.is_empty() {
                summary.push(' ');
                summary.push_str(&steps.join(" "));
            }
            summary
        }
        ReasoningStep::Reflection { critique, adjustment } => adjustment
            .as_ref()
            .map_or_else(|| critique.clone(), |value| format!("{critique} {value}")),
        ReasoningStep::Decision { choice, reasoning } => format!("{choice} {reasoning}"),
    }
}

fn extract_memory_topics(text: &str, limit: usize) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "about", "after", "again", "also", "analysis", "and", "another", "because",
        "before", "being", "between", "could", "decision", "during", "first", "from",
        "have", "into", "need", "next", "only", "plan", "reason", "reasoning",
        "reflection", "should", "steps", "that", "their", "there", "this", "tool",
        "turn", "used", "using", "with", "within", "would", "your",
    ];

    let mut topics = BTreeSet::new();
    for token in text.split(|ch: char| !ch.is_ascii_alphanumeric()) {
        let token = token.trim().to_ascii_lowercase();
        if token.len() < 4 || STOP_WORDS.contains(&token.as_str()) {
            continue;
        }
        if token.chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }

        topics.insert(token);
        if topics.len() >= limit {
            break;
        }
    }

    topics.into_iter().collect()
}

fn collect_reflection_topics(chain: &ChainOfThought) -> Vec<String> {
    let mut topics = BTreeSet::new();

    for step in &chain.steps {
        for topic in extract_memory_topics(&reasoning_step_summary(step), 4) {
            topics.insert(topic);
        }
    }

    for alternative in &chain.alternatives {
        for topic in extract_memory_topics(&alternative.description, 2) {
            topics.insert(topic);
        }
    }

    topics.into_iter().take(6).collect()
}

fn record_reflection_memory(
    memory: &mut LongTermMemory,
    chain: Option<&ChainOfThought>,
    summary: &TurnSummary,
) {
    let mut failed_tools = BTreeSet::new();
    let mut failure_examples = Vec::new();

    for message in &summary.tool_results {
        if let Some(ContentBlock::ToolResult {
            tool_name,
            output,
            is_error,
            ..
        }) = message.blocks.first()
        {
            if *is_error {
                failed_tools.insert(tool_name.clone());
                if failure_examples.len() < 3 {
                    failure_examples.push(format!(
                        "{tool_name}: {}",
                        output.chars().take(160).collect::<String>()
                    ));
                }
            }
        }
    }

    if !failed_tools.is_empty() {
        let note = format!(
            "Observed {} failed tool(s): {}",
            failed_tools.len(),
            failure_examples.join(" | ")
        );
        memory.add_entry("tool_failure", note, 0.95);

        for tool_name in failed_tools {
            memory.add_entry(
                tool_name.clone(),
                format!("Tool failed during turn: {tool_name}"),
                0.90,
            );

            for capability in infer_tool_capabilities(&tool_name, None).into_iter().take(3) {
                memory.add_entry(
                    capability.clone(),
                    format!(
                        "Failure pattern observed while exercising capability {capability} via {tool_name}"
                    ),
                    0.85,
                );
            }
        }
    }

    if let Some(cot) = chain {
        let reasoning_topics = collect_reflection_topics(cot);
        if cot.confidence < 0.4 {
            let note = format!(
                "Low decision confidence detected ({:.2}). Steps: {}",
                cot.confidence,
                cot.steps
                    .iter()
                    .map(reasoning_step_summary)
                    .map(|snippet| snippet.chars().take(80).collect::<String>())
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
            memory.add_entry("low_confidence_decision", note, cot.confidence);
        }

        if !reasoning_topics.is_empty() {
            let note = format!("Reasoning topics: {}", reasoning_topics.join(", "));
            memory.add_entry("reflection_summary", note, cot.confidence.max(0.5));

            for topic in reasoning_topics {
                memory.add_entry(
                    topic.clone(),
                    format!("Reasoning topic observed during reflection: {topic}"),
                    cot.confidence.max(0.5),
                );
            }
        }

        if !cot.alternatives.is_empty() {
            let note = cot
                .alternatives
                .iter()
                .map(|alternative| alternative.description.chars().take(100).collect::<String>())
                .collect::<Vec<_>>()
                .join(" | ");
            memory.add_entry("alternative_approach", note, cot.confidence.max(0.4));
        }
    }

    let turn_note = format!(
        "Turn completed: {} assistant messages, {} tool results, {} iterations",
        summary.assistant_messages.len(),
        summary.tool_results.len(),
        summary.iterations
    );
    memory.add_entry("turn_summary", turn_note, 0.5);
}

/// Prompt-cache telemetry captured from the provider response stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCacheEvent {
    pub unexpected: bool,
    pub reason: String,
    pub previous_cache_read_input_tokens: u32,
    pub current_cache_read_input_tokens: u32,
    pub token_drop: u32,
}

/// Minimal streaming API contract required by [`ConversationRuntime`].
pub trait ApiClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError>;
}

/// Trait implemented by tool dispatchers that execute model-requested tools.
pub trait ToolExecutor {
    fn available_tools(&self) -> Vec<Tool> {
        Vec::new()
    }

    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError>;
}

/// Error returned when a tool invocation fails locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    message: String,
}

impl ToolError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ToolError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolError {}

/// Error returned when a conversation turn cannot be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    message: String,
}

impl RuntimeError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for RuntimeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}

/// Summary of one completed runtime turn, including tool results and usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub assistant_messages: Vec<ConversationMessage>,
    pub tool_results: Vec<ConversationMessage>,
    pub prompt_cache_events: Vec<PromptCacheEvent>,
    pub iterations: usize,
    pub usage: TokenUsage,
    pub auto_compaction: Option<AutoCompactionEvent>,
}

/// Details about automatic session compaction applied during a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoCompactionEvent {
    pub removed_message_count: usize,
}

pub trait DecisioningEventReporter: Send + Sync {
    fn emit_decisioning_event(&self, event: &DecisioningEvent);
}

struct DecisioningTurnPlan {
    engine: DecisioningEngine,
    task: Task,
    snapshot: DecisioningSnapshot,
    selected_positions: BTreeMap<String, usize>,
}

/// Coordinates the model loop, tool execution, hooks, and session updates.
pub struct ConversationRuntime<C, T> {
    session: Session,
    api_client: C,
    tool_executor: T,
    permission_policy: PermissionPolicy,
    system_prompt: Vec<String>,
    max_iterations: usize,
    usage_tracker: UsageTracker,
    hook_runner: HookRunner,
    decisioning_config: DecisioningConfig,
    decisioning_event_reporter: Option<Arc<dyn DecisioningEventReporter>>,
    auto_compaction_input_tokens_threshold: u32,
    hook_abort_signal: HookAbortSignal,
    hook_progress_reporter: Option<Box<dyn HookProgressReporter>>,
    session_tracer: Option<SessionTracer>,
}

impl<C, T> ConversationRuntime<C, T>
where
    C: ApiClient,
    T: ToolExecutor,
{
    #[must_use]
    pub fn new(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: Vec<String>,
    ) -> Self {
        Self::new_with_features(
            session,
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
            &RuntimeFeatureConfig::default(),
        )
    }

    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new_with_features(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: Vec<String>,
        feature_config: &RuntimeFeatureConfig,
    ) -> Self {
        let usage_tracker = UsageTracker::from_session(&session);
        Self {
            session,
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
            max_iterations: usize::MAX,
            usage_tracker,
            hook_runner: HookRunner::from_feature_config(feature_config),
            decisioning_config: feature_config.decisioning().clone(),
            decisioning_event_reporter: None,
            auto_compaction_input_tokens_threshold: auto_compaction_threshold_from_env(),
            hook_abort_signal: HookAbortSignal::default(),
            hook_progress_reporter: None,
            session_tracer: None,
        }
    }

    #[must_use]
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    #[must_use]
    pub fn with_auto_compaction_input_tokens_threshold(mut self, threshold: u32) -> Self {
        self.auto_compaction_input_tokens_threshold = threshold;
        self
    }

    #[must_use]
    pub fn with_hook_abort_signal(mut self, hook_abort_signal: HookAbortSignal) -> Self {
        self.hook_abort_signal = hook_abort_signal;
        self
    }

    #[must_use]
    pub fn with_hook_progress_reporter(
        mut self,
        hook_progress_reporter: Box<dyn HookProgressReporter>,
    ) -> Self {
        self.hook_progress_reporter = Some(hook_progress_reporter);
        self
    }

    #[must_use]
    pub fn with_session_tracer(mut self, session_tracer: SessionTracer) -> Self {
        self.session_tracer = Some(session_tracer);
        self
    }

    #[must_use]
    pub fn with_decisioning_event_reporter(
        mut self,
        reporter: impl DecisioningEventReporter + 'static,
    ) -> Self {
        self.decisioning_event_reporter = Some(Arc::new(reporter));
        self
    }

    fn run_pre_tool_use_hook(&mut self, tool_name: &str, input: &str) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_pre_tool_use_with_context(
                tool_name,
                input,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_pre_tool_use_with_context(
                tool_name,
                input,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    fn run_post_tool_use_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
        is_error: bool,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    fn run_post_tool_use_failure_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    /// Inject a user message containing the given content blocks into the
    /// session before the next [`run_turn`] call.
    ///
    /// This is used to attach file or image content to a turn without
    /// changing the `run_turn` signature. The blocks are merged into a single
    /// user message so providers that reject consecutive user messages work
    /// correctly.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the session cannot persist the message.
    pub fn inject_user_blocks(&mut self, blocks: Vec<ContentBlock>) -> Result<(), RuntimeError> {
        if blocks.is_empty() {
            return Ok(());
        }
        let message = ConversationMessage {
            role: MessageRole::User,
            blocks,
            usage: None,
        };
        self.session
            .push_message(message)
            .map_err(|e| RuntimeError::new(e.to_string()))
    }

    #[allow(clippy::too_many_lines)]
    pub fn run_turn(
        &mut self,
        user_input: impl Into<String>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnSummary, RuntimeError> {
        let user_input = user_input.into();
        self.record_turn_started(&user_input);
        self.session
            .push_user_text(user_input.clone())
            .map_err(|error| RuntimeError::new(error.to_string()))?;

        let mut assistant_messages = Vec::new();
        let mut tool_results = Vec::new();
        let mut prompt_cache_events = Vec::new();
        let mut iterations = 0;
        let mut chain_of_thought: Option<ChainOfThought> = None;

        loop {
            iterations += 1;
            if iterations > self.max_iterations {
                let error = RuntimeError::new(
                    "conversation loop exceeded the maximum number of iterations",
                );
                self.record_turn_failed(iterations, &error);
                return Err(error);
            }

            let request = ApiRequest {
                system_prompt: self.system_prompt.clone(),
                messages: self.session.messages.clone(),
            };
            let events = match self.api_client.stream(request) {
                Ok(events) => events,
                Err(error) => {
                    self.record_turn_failed(iterations, &error);
                    return Err(error);
                }
            };
            let (assistant_message, usage, turn_prompt_cache_events, cot_part) =
                match build_assistant_message(events) {
                    Ok(result) => result,
                    Err(error) => {
                        self.record_turn_failed(iterations, &error);
                        return Err(error);
                    }
                };
            if let Some(usage) = usage {
                self.usage_tracker.record(usage);
            }
            prompt_cache_events.extend(turn_prompt_cache_events);

            // Merge any chain-of-thought fragment produced this iteration
            if let Some(part) = cot_part {
                if let Some(ref mut outer) = chain_of_thought {
                    outer.steps.extend(part.steps);
                    outer.alternatives.extend(part.alternatives);
                    outer.recompute_confidence();
                } else {
                    chain_of_thought = Some(part);
                }
            }
            let pending_tool_uses = assistant_message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            self.record_assistant_iteration(
                iterations,
                &assistant_message,
                pending_tool_uses.len(),
            );

            self.session
                .push_message(assistant_message.clone())
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            assistant_messages.push(assistant_message);

            if pending_tool_uses.is_empty() {
                break;
            }

            let decisioning_plan = self.build_decisioning_turn_plan(
                &user_input,
                chain_of_thought.as_ref(),
                &pending_tool_uses,
            );

            let mut ordered_pending_tool_uses = pending_tool_uses
                .into_iter()
                .enumerate()
                .collect::<Vec<_>>();
            if let Some(plan) = decisioning_plan.as_ref() {
                ordered_pending_tool_uses.sort_by(|(left_index, left_use), (right_index, right_use)| {
                    let left_rank = plan
                        .selected_positions
                        .get(&left_use.1)
                        .copied()
                        .unwrap_or(usize::MAX);
                    let right_rank = plan
                        .selected_positions
                        .get(&right_use.1)
                        .copied()
                        .unwrap_or(usize::MAX);
                    left_rank
                        .cmp(&right_rank)
                        .then_with(|| left_index.cmp(right_index))
                });
            }

            let mut step_outcomes = Vec::new();

            for (_, (tool_use_id, tool_name, input)) in ordered_pending_tool_uses {
                let tool_started_at = Instant::now();
                let pre_hook_result = self.run_pre_tool_use_hook(&tool_name, &input);
                let effective_input = pre_hook_result
                    .updated_input()
                    .map_or_else(|| input.clone(), ToOwned::to_owned);
                let permission_context = PermissionContext::new(
                    pre_hook_result.permission_override(),
                    pre_hook_result.permission_reason().map(ToOwned::to_owned),
                );

                let decisioning_outcome = decisioning_plan
                    .as_ref()
                    .and_then(|plan| self.assess_tool_decisioning(plan, &tool_name, &effective_input));

                let permission_outcome = if pre_hook_result.is_cancelled() {
                    PermissionOutcome::Deny {
                        reason: format_hook_message(
                            &pre_hook_result,
                            &format!("PreToolUse hook cancelled tool `{tool_name}`"),
                        ),
                    }
                } else if pre_hook_result.is_failed() {
                    PermissionOutcome::Deny {
                        reason: format_hook_message(
                            &pre_hook_result,
                            &format!("PreToolUse hook failed for tool `{tool_name}`"),
                        ),
                    }
                } else if pre_hook_result.is_denied() {
                    PermissionOutcome::Deny {
                        reason: format_hook_message(
                            &pre_hook_result,
                            &format!("PreToolUse hook denied tool `{tool_name}`"),
                        ),
                    }
                } else if let Some((override_decision, reason)) = decisioning_outcome {
                    match override_decision {
                        PermissionOverride::Deny => PermissionOutcome::Deny { reason },
                        PermissionOverride::Ask => {
                            if let Some(prompt) = prompter.as_mut() {
                                let decisioning_context = PermissionContext::new(
                                    Some(PermissionOverride::Ask),
                                    Some(reason.clone()),
                                );
                                self.permission_policy.authorize_with_context(
                                    &tool_name,
                                    &effective_input,
                                    &decisioning_context,
                                    Some(*prompt),
                                )
                            } else {
                                PermissionOutcome::Deny { reason }
                            }
                        }
                        PermissionOverride::Allow => {
                            unreachable!("decisioning never emits Allow overrides")
                        }
                    }
                } else if let Some(prompt) = prompter.as_mut() {
                    self.permission_policy.authorize_with_context(
                        &tool_name,
                        &effective_input,
                        &permission_context,
                        Some(*prompt),
                    )
                } else {
                    self.permission_policy.authorize_with_context(
                        &tool_name,
                        &effective_input,
                        &permission_context,
                        None,
                    )
                };

                let plan_step_id = decisioning_plan
                    .as_ref()
                    .and_then(|plan| {
                        plan.snapshot.plan.steps.iter().find(|step| {
                            step.candidate_tools.iter().any(|candidate| candidate == &tool_name)
                        })
                    })
                    .map(|step| step.id.clone())
                    .unwrap_or_else(|| tool_use_id.clone());
                let permission_allowed = matches!(&permission_outcome, PermissionOutcome::Allow);
                let result_message = match permission_outcome {
                    PermissionOutcome::Allow => {
                        self.record_tool_started(iterations, &tool_name);
                        let (mut output, mut is_error) =
                            match self.tool_executor.execute(&tool_name, &effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            };
                        output = merge_hook_feedback(pre_hook_result.messages(), output, false);

                        let post_hook_result = if is_error {
                            self.run_post_tool_use_failure_hook(
                                &tool_name,
                                &effective_input,
                                &output,
                            )
                        } else {
                            self.run_post_tool_use_hook(
                                &tool_name,
                                &effective_input,
                                &output,
                                false,
                            )
                        };
                        if post_hook_result.is_denied()
                            || post_hook_result.is_failed()
                            || post_hook_result.is_cancelled()
                        {
                            is_error = true;
                        }
                        output = merge_hook_feedback(
                            post_hook_result.messages(),
                            output,
                            post_hook_result.is_denied()
                                || post_hook_result.is_failed()
                                || post_hook_result.is_cancelled(),
                        );

                        ConversationMessage::tool_result(
                            tool_use_id.clone(),
                            tool_name.clone(),
                            output,
                            is_error,
                        )
                    }
                    PermissionOutcome::Deny { reason } => ConversationMessage::tool_result(
                        tool_use_id.clone(),
                        tool_name.clone(),
                        merge_hook_feedback(pre_hook_result.messages(), reason, true),
                        true,
                    ),
                };
                let step_latency_ms = tool_started_at.elapsed().as_millis().min(u128::from(u32::MAX))
                    as u32;
                step_outcomes.push(StepOutcome {
                    step_id: plan_step_id,
                    succeeded: permission_allowed
                        && !matches!(result_message.blocks.first(), Some(ContentBlock::ToolResult { is_error: true, .. })),
                    latency_ms: step_latency_ms,
                    notes: vec![format!("tool={tool_name}",)],
                });
                self.session
                    .push_message(result_message.clone())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                self.record_tool_finished(iterations, &result_message);
                tool_results.push(result_message);
            }

            if let Some(plan) = decisioning_plan.as_ref() {
                if step_outcomes.iter().any(|outcome| !outcome.succeeded) {
                    let adjustment = plan.engine.planner.adjust_plan(&plan.snapshot.plan, &step_outcomes);
                    if self.decisioning_config.emit_events() {
                        self.emit_decisioning_adjustment_event(plan, &adjustment);
                    }
                }
            }
        }

        let auto_compaction = self.maybe_auto_compact();

        let summary = TurnSummary {
            assistant_messages,
            tool_results,
            prompt_cache_events,
            iterations,
            usage: self.usage_tracker.cumulative_usage(),
            auto_compaction,
        };
        // Run lightweight reflection and learning hooks before completing the turn.
        let _ = self.reflect_on_outcome(chain_of_thought, &summary);
        self.record_turn_completed(&summary);

        Ok(summary)
    }

    #[must_use]
    pub fn compact(&self, config: CompactionConfig) -> CompactionResult {
        compact_session(&self.session, config)
    }

    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        estimate_session_tokens(&self.session)
    }

    #[must_use]
    pub fn usage(&self) -> &UsageTracker {
        &self.usage_tracker
    }

    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn api_client_mut(&mut self) -> &mut C {
        &mut self.api_client
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    #[must_use]
    pub fn fork_session(&self, branch_name: Option<String>) -> Session {
        self.session.fork(branch_name)
    }

    #[must_use]
    pub fn into_session(self) -> Session {
        self.session
    }

    fn maybe_auto_compact(&mut self) -> Option<AutoCompactionEvent> {
        if self.usage_tracker.cumulative_usage().input_tokens
            < self.auto_compaction_input_tokens_threshold
        {
            return None;
        }

        let result = compact_session(
            &self.session,
            CompactionConfig {
                max_estimated_tokens: 0,
                ..CompactionConfig::default()
            },
        );

        if result.removed_message_count == 0 {
            return None;
        }

        self.session = result.compacted_session;
        Some(AutoCompactionEvent {
            removed_message_count: result.removed_message_count,
        })
    }

    fn record_turn_started(&self, user_input: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert(
            "user_input".to_string(),
            Value::String(user_input.to_string()),
        );
        session_tracer.record("turn_started", attributes);
    }

    fn record_assistant_iteration(
        &self,
        iteration: usize,
        assistant_message: &ConversationMessage,
        pending_tool_use_count: usize,
    ) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert(
            "assistant_blocks".to_string(),
            Value::from(assistant_message.blocks.len() as u64),
        );
        attributes.insert(
            "pending_tool_use_count".to_string(),
            Value::from(pending_tool_use_count as u64),
        );
        session_tracer.record("assistant_iteration_completed", attributes);
    }

    fn record_tool_started(&self, iteration: usize, tool_name: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert(
            "tool_name".to_string(),
            Value::String(tool_name.to_string()),
        );
        session_tracer.record("tool_execution_started", attributes);
    }

    fn record_tool_finished(&self, iteration: usize, result_message: &ConversationMessage) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let Some(ContentBlock::ToolResult {
            tool_name,
            is_error,
            ..
        }) = result_message.blocks.first()
        else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("tool_name".to_string(), Value::String(tool_name.clone()));
        attributes.insert("is_error".to_string(), Value::Bool(*is_error));
        session_tracer.record("tool_execution_finished", attributes);
    }

    fn record_turn_completed(&self, summary: &TurnSummary) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert(
            "iterations".to_string(),
            Value::from(summary.iterations as u64),
        );
        attributes.insert(
            "assistant_messages".to_string(),
            Value::from(summary.assistant_messages.len() as u64),
        );
        attributes.insert(
            "tool_results".to_string(),
            Value::from(summary.tool_results.len() as u64),
        );
        attributes.insert(
            "prompt_cache_events".to_string(),
            Value::from(summary.prompt_cache_events.len() as u64),
        );
        session_tracer.record("turn_completed", attributes);
    }

    fn record_turn_failed(&self, iteration: usize, error: &RuntimeError) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("error".to_string(), Value::String(error.to_string()));
        session_tracer.record("turn_failed", attributes);
    }

    fn build_reasoning_context(&self, chain_of_thought: Option<&ChainOfThought>) -> ReasoningContext {
        ReasoningContext {
            chain_of_thought: chain_of_thought.cloned(),
            workspace_root: self.session.workspace_root().map(PathBuf::from),
            memory_topics: self.collect_memory_topics(),
            recent_tool_history: self.collect_recent_tool_history(),
            active_constraints: self.collect_active_constraints(),
            max_parallelism: self.decisioning_config.max_parallelism(),
        }
    }

    fn collect_memory_topics(&self) -> Vec<String> {
        let memory = LongTermMemory::load_for_workspace(self.session.workspace_root());
        memory.ranked_topics(8)
    }

    fn collect_recent_tool_history(&self) -> Vec<ToolHistoryEntry> {
        self.session
            .messages
            .iter()
            .rev()
            .filter_map(|message| {
                if message.role != MessageRole::Tool {
                    return None;
                }

                let Some(ContentBlock::ToolResult {
                    tool_name,
                    is_error,
                    output,
                    ..
                }) = message.blocks.first()
                else {
                    return None;
                };

                Some(ToolHistoryEntry {
                    tool_name: tool_name.clone(),
                    succeeded: !*is_error,
                    latency_ms: 0,
                    note: Some(output.chars().take(120).collect()),
                })
            })
            .take(8)
            .collect()
    }

    fn collect_active_constraints(&self) -> Vec<String> {
        let mut constraints = vec![format!(
            "permission-mode:{}",
            self.permission_policy.active_mode().as_str()
        )];

        if let Some(workspace_root) = self.session.workspace_root() {
            constraints.push(format!("workspace-root:{}", workspace_root.display()));
        }

        constraints
    }

    fn build_decisioning_tools(&self, pending_tool_uses: &[(String, String, String)]) -> Vec<Tool> {
        let recent_history = self.collect_recent_tool_history();
        let mut tools = BTreeMap::new();

        for tool in self.tool_executor.available_tools() {
            tools.entry(tool.name.clone()).or_insert(tool);
        }

        for (_, tool_name, _) in pending_tool_uses {
            tools
                .entry(tool_name.clone())
                .or_insert_with(|| tool_from_profile(tool_name, None, None));
        }

        for tool in tools.values_mut() {
            let matching_history = recent_history
                .iter()
                .filter(|entry| entry.tool_name == tool.name)
                .collect::<Vec<_>>();
            let successful_count = matching_history.iter().filter(|entry| entry.succeeded).count() as f32;
            let total_count = matching_history.len() as f32;
            if total_count > 0.0 {
                let historical_success_rate = (successful_count / total_count).clamp(0.05, 0.99);
                let historical_latency_ms = (matching_history
                    .iter()
                    .map(|entry| entry.latency_ms as u64)
                    .sum::<u64>()
                    / total_count as u64) as u32;
                tool.avg_success_rate = ((tool.avg_success_rate * 0.6)
                    + (historical_success_rate * 0.4))
                    .clamp(0.05, 0.99);
                tool.avg_latency_ms = ((tool.avg_latency_ms as f32 * 0.6)
                    + (historical_latency_ms as f32 * 0.4)) as u32;
            }
        }

        tools.into_values().collect()
    }

    fn build_decisioning_task(
        &self,
        user_input: &str,
        pending_tool_uses: &[(String, String, String)],
        reasoning_context: &ReasoningContext,
    ) -> Task {
        let mut required_capabilities = pending_tool_uses
            .iter()
            .flat_map(|(_, tool_name, _)| infer_tool_capabilities(tool_name, None))
            .collect::<Vec<_>>();
        required_capabilities.sort();
        required_capabilities.dedup();

        Task::new(
            format!("turn-{}", self.session.session_id),
            user_input.to_string(),
            pending_tool_uses.len().clamp(1, 5) as u8,
            required_capabilities,
            reasoning_context.active_constraints.clone(),
        )
    }

    fn build_decisioning_engine(
        &self,
        available_tools: Vec<Tool>,
        reasoning_context: ReasoningContext,
    ) -> DecisioningEngine {
        let safety = SafetyPolicy::from_config(self.decisioning_config.safety_policy());

        DecisioningEngine::new(
            ToolSelector::new(available_tools, reasoning_context),
            crate::TaskPlanner::new(self.decisioning_config.max_parallelism()),
            safety,
        )
    }

    fn build_decisioning_turn_plan(
        &self,
        user_input: &str,
        chain_of_thought: Option<&ChainOfThought>,
        pending_tool_uses: &[(String, String, String)],
    ) -> Option<DecisioningTurnPlan> {
        if !self.decisioning_config.enabled() || pending_tool_uses.is_empty() {
            return None;
        }

        let reasoning_context = self.build_reasoning_context(chain_of_thought);
        let decisioning_task = self.build_decisioning_task(user_input, pending_tool_uses, &reasoning_context);
        let decisioning_engine = self.build_decisioning_engine(
            self.build_decisioning_tools(pending_tool_uses),
            reasoning_context,
        );
        let snapshot = decisioning_engine.analyze(&decisioning_task);
        if self.decisioning_config.emit_events() {
            self.record_decisioning_snapshot(&snapshot);
            self.emit_decisioning_events(&snapshot);
        }

        let selected_positions = snapshot
            .selected_tools
            .iter()
            .enumerate()
            .map(|(index, tool)| (tool.name.clone(), index))
            .collect::<BTreeMap<_, _>>();

        Some(DecisioningTurnPlan {
            engine: decisioning_engine,
            task: decisioning_task,
            snapshot,
            selected_positions,
        })
    }

    fn record_decisioning_snapshot(&self, snapshot: &DecisioningSnapshot) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("task_id".to_string(), Value::String(snapshot.task.id.clone()));
        attributes.insert(
            "selected_tool_count".to_string(),
            Value::from(snapshot.selected_tools.len() as u64),
        );
        attributes.insert(
            "plan_step_count".to_string(),
            Value::from(snapshot.plan.steps.len() as u64),
        );
        attributes.insert(
            "risk_score".to_string(),
            Value::from(f64::from(snapshot.risk.score)),
        );
        attributes.insert(
            "risk_outcome".to_string(),
            Value::String(format!("{:?}", snapshot.risk.outcome)),
        );
        if let Ok(serialized) = serde_json::to_value(snapshot) {
            attributes.insert("snapshot".to_string(), serialized);
        }

        session_tracer.record("decisioning_snapshot", attributes);
    }

    fn emit_decisioning_events(&self, snapshot: &DecisioningSnapshot) {
        let Some(reporter) = &self.decisioning_event_reporter else {
            return;
        };

        for event in &snapshot.events {
            reporter.emit_decisioning_event(event);
        }
    }

    fn emit_decisioning_adjustment_event(
        &self,
        plan: &DecisioningTurnPlan,
        adjustment: &crate::PlanAdjustment,
    ) {
        let Some(reporter) = &self.decisioning_event_reporter else {
            return;
        };

        let mut selected_tools = adjustment
            .revised_plan
            .steps
            .iter()
            .flat_map(|step| step.candidate_tools.iter().cloned())
            .collect::<Vec<_>>();
        selected_tools.sort();
        selected_tools.dedup();

        reporter.emit_decisioning_event(&DecisioningEvent {
            kind: DecisioningEventKind::PlanAdjustment,
            title: "Plan adjustment".to_string(),
            summary: adjustment.reason.clone(),
            task_id: adjustment.original_task_id.clone(),
            confidence: Some(adjustment.revised_plan.confidence),
            risk_score: Some(plan.snapshot.risk.score),
            risk_level: None,
            selected_tools,
            parallelizable: Some(matches!(
                adjustment.revised_plan.execution_mode,
                crate::ExecutionMode::Parallel { .. }
            )),
            action: Some(SafetyOutcome::Review),
            tool_scores: None,
            plan_tree: Some(crate::build_plan_tree(
                &plan.task,
                &adjustment.revised_plan,
                &plan.snapshot.selected_tools,
            )),
            details: adjustment
                .changed_step_ids
                .iter()
                .map(|step_id| format!("Adjusted step: {step_id}"))
                .collect(),
        });
    }

    fn assess_tool_decisioning(
        &self,
        plan: &DecisioningTurnPlan,
        tool_name: &str,
        effective_input: &str,
    ) -> Option<(PermissionOverride, String)> {
        if !plan.selected_positions.contains_key(tool_name) {
            let selected_tools = plan
                .snapshot
                .selected_tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>();
            let reason = if selected_tools.is_empty() {
                format!("Decisioning engine did not select {tool_name} for this turn.")
            } else {
                format!(
                    "Decisioning engine selected {} for this turn; skipping {tool_name}.",
                    selected_tools.join(", ")
                )
            };
            return Some((PermissionOverride::Deny, reason));
        }

        let risk = self.assess_tool_risk(plan, tool_name, effective_input);
        match risk.outcome {
            SafetyOutcome::Allow => None,
            SafetyOutcome::Review => Some((
                PermissionOverride::Ask,
                self.format_decisioning_risk_reason(tool_name, &risk, &plan.snapshot),
            )),
            SafetyOutcome::Deny => Some((
                PermissionOverride::Deny,
                self.format_decisioning_risk_reason(tool_name, &risk, &plan.snapshot),
            )),
        }
    }

    fn assess_tool_risk(
        &self,
        plan: &DecisioningTurnPlan,
        tool_name: &str,
        effective_input: &str,
    ) -> RiskAssessment {
        let mut constraints = plan.task.constraints.clone();
        constraints.push(effective_input.to_string());

        let task = Task::new(
            format!("{}::{tool_name}", plan.task.id),
            plan.task.description.clone(),
            plan.task.complexity,
            plan.task.required_capabilities.clone(),
            constraints,
        );
        let capabilities = infer_tool_capabilities(tool_name, None);
        let primary_capability = capabilities
            .first()
            .cloned()
            .unwrap_or_else(|| tool_name.to_ascii_lowercase());
        let subtask = Subtask {
            id: format!("{}-{}", task.id, primary_capability),
            title: format!("Execute {tool_name}"),
            required_capabilities: capabilities,
            candidate_tools: vec![tool_name.to_string()],
            parallelizable: false,
            estimated_effort: 1,
            notes: vec![format!(
                "Tool input: {}",
                effective_input.chars().take(160).collect::<String>()
            )],
        };

        plan.engine
            .safety
            .assess(&task, &subtask, &plan.engine.selector.reasoning_context)
    }

    fn format_decisioning_risk_reason(
        &self,
        tool_name: &str,
        risk: &RiskAssessment,
        snapshot: &DecisioningSnapshot,
    ) -> String {
        let details = if risk.reasons.is_empty() {
            "no explicit reasons were provided".to_string()
        } else {
            risk.reasons.join("; ")
        };

        format!(
            "Decisioning {:?} {tool_name} (risk {:.2}, plan mode {}): {details}",
            risk.outcome,
            risk.score,
            snapshot.plan.execution_mode.label()
        )
    }

    /// Perform lightweight reflection on the completed turn and persist
    /// notable facts to the long-term memory store.
    fn reflect_on_outcome(
        &mut self,
        chain: Option<ChainOfThought>,
        summary: &TurnSummary,
    ) -> Result<(), RuntimeError> {
        // Load or create a workspace-scoped memory store.
        let workspace = self.session.workspace_root();
        let mut memory = LongTermMemory::load_for_workspace(workspace);

        record_reflection_memory(&mut memory, chain.as_ref(), summary);

        // best-effort save (already attempted in add_entry)
        let _ = memory.save();
        Ok(())
    }
}

/// Reads the automatic compaction threshold from the environment.
#[must_use]
pub fn auto_compaction_threshold_from_env() -> u32 {
    parse_auto_compaction_threshold(
        std::env::var(AUTO_COMPACTION_THRESHOLD_ENV_VAR)
            .ok()
            .as_deref(),
    )
}

#[must_use]
fn parse_auto_compaction_threshold(value: Option<&str>) -> u32 {
    value
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|threshold| *threshold > 0)
        .unwrap_or(DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD)
}

fn build_assistant_message(
    events: Vec<AssistantEvent>,
) -> Result<
    (
        ConversationMessage,
        Option<TokenUsage>,
        Vec<PromptCacheEvent>,
        Option<ChainOfThought>,
    ),
    RuntimeError,
> {
    let mut text = String::new();
    let mut blocks = Vec::new();
    let mut prompt_cache_events = Vec::new();
    let mut chain: Option<ChainOfThought> = None;
    let mut finished = false;
    let mut usage = None;

    for event in events {
        match event {
            AssistantEvent::TextDelta(delta) => text.push_str(&delta),
            AssistantEvent::ToolUse { id, name, input } => {
                flush_text_block(&mut text, &mut blocks);
                blocks.push(ContentBlock::ToolUse { id, name, input });
            }
            AssistantEvent::ReasoningStep(step) => {
                // Flush any pending text so reasoning steps separate text blocks
                flush_text_block(&mut text, &mut blocks);
                // Collect reasoning steps into an optional ChainOfThought so
                // the runtime can reflect and persist learning.
                if chain.is_none() {
                    chain = Some(ChainOfThought::new());
                }
                if let Some(ref mut c) = chain {
                    c.add_step(step);
                }
                // reasoning steps are not added to message blocks
            }
            AssistantEvent::Usage(value) => usage = Some(value),
            AssistantEvent::PromptCache(event) => prompt_cache_events.push(event),
            AssistantEvent::MessageStop => {
                finished = true;
            }
        }
    }

    flush_text_block(&mut text, &mut blocks);

    if !finished {
        return Err(RuntimeError::new(
            "assistant stream ended without a message stop event",
        ));
    }
    if blocks.is_empty() {
        return Err(RuntimeError::new("assistant stream produced no content"));
    }

    Ok((
        ConversationMessage::assistant_with_usage(blocks, usage),
        usage,
        prompt_cache_events,
        chain,
    ))
}

fn flush_text_block(text: &mut String, blocks: &mut Vec<ContentBlock>) {
    if !text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: std::mem::take(text),
        });
    }
}

fn format_hook_message(result: &HookRunResult, fallback: &str) -> String {
    if result.messages().is_empty() {
        fallback.to_string()
    } else {
        result.messages().join("\n")
    }
}

fn merge_hook_feedback(messages: &[String], output: String, is_error: bool) -> String {
    if messages.is_empty() {
        return output;
    }

    let mut sections = Vec::new();
    if !output.trim().is_empty() {
        sections.push(output);
    }
    let label = if is_error {
        "Hook feedback (error)"
    } else {
        "Hook feedback"
    };
    sections.push(format!("{label}:\n{}", messages.join("\n")));
    sections.join("\n\n")
}

type ToolHandler = Box<dyn FnMut(&str) -> Result<String, ToolError>>;

/// Simple in-memory tool executor for tests and lightweight integrations.
#[derive(Default)]
pub struct StaticToolExecutor {
    handlers: BTreeMap<String, ToolHandler>,
}

impl StaticToolExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn register(
        mut self,
        tool_name: impl Into<String>,
        handler: impl FnMut(&str) -> Result<String, ToolError> + 'static,
    ) -> Self {
        self.handlers.insert(tool_name.into(), Box::new(handler));
        self
    }
}

impl ToolExecutor for StaticToolExecutor {
    fn available_tools(&self) -> Vec<Tool> {
        self.handlers
            .keys()
            .map(|tool_name| tool_from_profile(tool_name, None, None))
            .collect()
    }

    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        self.handlers
            .get_mut(tool_name)
            .ok_or_else(|| ToolError::new(format!("unknown tool: {tool_name}")))?(input)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_assistant_message, parse_auto_compaction_threshold, ApiClient, ApiRequest,
        AssistantEvent, AutoCompactionEvent, AlternativeApproach, ChainOfThought,
        ConversationRuntime, DecisioningEvent, DecisioningEventReporter, LongTermMemory,
        MemoryEntry, PromptCacheEvent, ReasoningStep, RuntimeError, StaticToolExecutor,
        ToolExecutor, TurnSummary, DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD,
    };
    use crate::compact::CompactionConfig;
    use crate::config::{DecisioningConfig, RuntimeFeatureConfig, RuntimeHookConfig};
    use crate::permissions::{
        PermissionMode, PermissionPolicy, PermissionPromptDecision, PermissionPrompter,
        PermissionRequest,
    };
    use crate::prompt::{ProjectContext, SystemPromptBuilder};
    use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
    use crate::usage::TokenUsage;
    use crate::ToolError;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use telemetry::{MemoryTelemetrySink, SessionTracer, TelemetryEvent};

    struct ScriptedApiClient {
        call_count: usize,
    }

    impl ApiClient for ScriptedApiClient {
        fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.call_count += 1;
            match self.call_count {
                1 => {
                    assert!(request
                        .messages
                        .iter()
                        .any(|message| message.role == MessageRole::User));
                    Ok(vec![
                        AssistantEvent::TextDelta("Let me calculate that.".to_string()),
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: "2,2".to_string(),
                        },
                        AssistantEvent::Usage(TokenUsage {
                            input_tokens: 20,
                            output_tokens: 6,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 2,
                        }),
                        AssistantEvent::MessageStop,
                    ])
                }
                2 => {
                    let last_message = request
                        .messages
                        .last()
                        .expect("tool result should be present");
                    assert_eq!(last_message.role, MessageRole::Tool);
                    Ok(vec![
                        AssistantEvent::TextDelta("The answer is 4.".to_string()),
                        AssistantEvent::Usage(TokenUsage {
                            input_tokens: 24,
                            output_tokens: 4,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 3,
                        }),
                        AssistantEvent::PromptCache(PromptCacheEvent {
                            unexpected: true,
                            reason:
                                "cache read tokens dropped while prompt fingerprint remained stable"
                                    .to_string(),
                            previous_cache_read_input_tokens: 6_000,
                            current_cache_read_input_tokens: 1_000,
                            token_drop: 5_000,
                        }),
                        AssistantEvent::MessageStop,
                    ])
                }
                _ => unreachable!("extra API call"),
            }
        }
    }

    struct PromptAllowOnce;

    impl PermissionPrompter for PromptAllowOnce {
        fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
            assert_eq!(request.tool_name, "add");
            PermissionPromptDecision::Allow
        }
    }

    #[test]
    fn runs_user_to_tool_to_result_loop_end_to_end_and_tracks_usage() {
        let api_client = ScriptedApiClient { call_count: 0 };
        let tool_executor = StaticToolExecutor::new().register("add", |input| {
            let total = input
                .split(',')
                .map(|part| part.parse::<i32>().expect("input must be valid integer"))
                .sum::<i32>();
            Ok(total.to_string())
        });
        let permission_policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite);
        let system_prompt = SystemPromptBuilder::new()
            .with_project_context(ProjectContext {
                cwd: PathBuf::from("/tmp/project"),
                current_date: "2026-03-31".to_string(),
                git_status: None,
                git_diff: None,
                git_context: None,
                instruction_files: Vec::new(),
            })
            .with_os("linux", "6.8")
            .build();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
        );

        let summary = runtime
            .run_turn("what is 2 + 2?", Some(&mut PromptAllowOnce))
            .expect("conversation loop should succeed");

        assert_eq!(summary.iterations, 2);
        assert_eq!(summary.assistant_messages.len(), 2);
        assert_eq!(summary.tool_results.len(), 1);
        assert_eq!(summary.prompt_cache_events.len(), 1);
        assert_eq!(runtime.session().messages.len(), 4);
        assert_eq!(summary.usage.output_tokens, 10);
        assert_eq!(summary.auto_compaction, None);
        assert!(matches!(
            runtime.session().messages[1].blocks[1],
            ContentBlock::ToolUse { .. }
        ));
        assert!(matches!(
            runtime.session().messages[2].blocks[0],
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn records_runtime_session_trace_events() {
        let sink = Arc::new(MemoryTelemetrySink::default());
        let tracer = SessionTracer::new("session-runtime", sink.clone());
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["system".to_string()],
        )
        .with_session_tracer(tracer);

        runtime
            .run_turn("what is 2 + 2?", Some(&mut PromptAllowOnce))
            .expect("conversation loop should succeed");

        let events = sink.events();
        let trace_names = events
            .iter()
            .filter_map(|event| match event {
                TelemetryEvent::SessionTrace(trace) => Some(trace.name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(trace_names.contains(&"turn_started"));
        assert!(trace_names.contains(&"assistant_iteration_completed"));
        assert!(trace_names.contains(&"tool_execution_started"));
        assert!(trace_names.contains(&"tool_execution_finished"));
        assert!(trace_names.contains(&"turn_completed"));
    }

    #[test]
    fn long_term_memory_ranks_high_value_topics() {
        let memory = LongTermMemory {
            path: PathBuf::from("/tmp/unused-memory.json"),
            entries: vec![
                MemoryEntry {
                    topic: "older-topic".to_string(),
                    note: "older entry".to_string(),
                    confidence: 0.30,
                    ts_ms: 10,
                },
                MemoryEntry {
                    topic: "fresh-topic".to_string(),
                    note: "higher confidence".to_string(),
                    confidence: 0.90,
                    ts_ms: 20,
                },
                MemoryEntry {
                    topic: "fresh-topic".to_string(),
                    note: "duplicate with lower confidence".to_string(),
                    confidence: 0.70,
                    ts_ms: 30,
                },
                MemoryEntry {
                    topic: "mid-topic".to_string(),
                    note: "mid confidence".to_string(),
                    confidence: 0.60,
                    ts_ms: 40,
                },
            ],
        };

        assert_eq!(
            memory.ranked_topics(3),
            vec![
                "fresh-topic".to_string(),
                "mid-topic".to_string(),
                "older-topic".to_string(),
            ]
        );
    }

    #[test]
    fn reflection_records_failed_tools_and_reasoning_topics() {
        let workspace_root = std::env::temp_dir().join(format!(
            "himalaya-reflection-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after UNIX_EPOCH")
                .as_millis()
        ));
        let mut memory = LongTermMemory {
            path: workspace_root.join(".Himalaya").join("long_term_memory.json"),
            entries: Vec::new(),
        };

        let mut chain = ChainOfThought::new();
        chain.add_step(ReasoningStep::Analysis {
            content: "Need shell access to inspect workspace write behavior".to_string(),
            confidence: Some(0.30),
        });
        chain.alternatives.push(AlternativeApproach {
            description: "Use read-only inspection first".to_string(),
            confidence: Some(0.60),
        });

        let summary = TurnSummary {
            assistant_messages: Vec::new(),
            tool_results: vec![ConversationMessage::tool_result(
                "tool-1",
                "shell",
                "permission denied",
                true,
            )],
            prompt_cache_events: Vec::new(),
            iterations: 1,
            usage: TokenUsage::default(),
            auto_compaction: None,
        };

        super::record_reflection_memory(&mut memory, Some(&chain), &summary);

        assert!(memory.entries.iter().any(|entry| entry.topic == "tool_failure"));
        assert!(memory.entries.iter().any(|entry| entry.topic == "shell"));
        assert!(memory
            .entries
            .iter()
            .any(|entry| entry.topic == "reflection_summary"));
        assert!(memory
            .entries
            .iter()
            .any(|entry| entry.topic == "low_confidence_decision"));
        assert!(memory.entries.iter().any(|entry| entry.topic == "turn_summary"));
    }

    #[test]
    fn decisioning_reorders_and_limits_tool_execution() {
        struct MultiToolApiClient;

        struct RecordingDecisioningReporter {
            events: Arc<Mutex<Vec<DecisioningEvent>>>,
        }

        impl RecordingDecisioningReporter {
            fn new() -> Self {
                Self {
                    events: Arc::new(Mutex::new(Vec::new())),
                }
            }
        }

        impl DecisioningEventReporter for RecordingDecisioningReporter {
            fn emit_decisioning_event(&self, event: &DecisioningEvent) {
                self.events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(event.clone());
            }
        }

        impl ApiClient for MultiToolApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    let tool_results = request
                        .messages
                        .iter()
                        .filter(|message| message.role == MessageRole::Tool)
                        .count();
                    assert_eq!(tool_results, 2);
                    return Ok(vec![
                        AssistantEvent::TextDelta("done".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }

                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-z".to_string(),
                        name: "z_tool".to_string(),
                        input: "plain input".to_string(),
                    },
                    AssistantEvent::ToolUse {
                        id: "tool-a".to_string(),
                        name: "a_tool".to_string(),
                        input: "plain input".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let sink = Arc::new(MemoryTelemetrySink::default());
        let tracer = SessionTracer::new("decisioning-selection", sink.clone());
        let reporter = RecordingDecisioningReporter::new();
        let reporter_events = reporter.events.clone();
        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(true)
                .with_max_parallelism(1),
        );

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            MultiToolApiClient,
            StaticToolExecutor::new()
                .register("a_tool", |_input| Ok("selected".to_string()))
                .register("z_tool", |_input| {
                    panic!("z_tool should not execute when decisioning limits the turn")
                }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        )
        .with_decisioning_event_reporter(reporter)
        .with_session_tracer(tracer);

        let summary = runtime
            .run_turn("choose the best tool", None)
            .expect("decisioning turn should succeed");

        assert_eq!(summary.tool_results.len(), 2);
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult {
                tool_name,
                is_error: false,
                output,
                ..
            } if tool_name == "a_tool" && output == "selected"
        ));
        assert!(matches!(
            &summary.tool_results[1].blocks[0],
            ContentBlock::ToolResult {
                tool_name,
                is_error: true,
                output,
                ..
            } if tool_name == "z_tool" && output.contains("Decisioning engine selected a_tool")
        ));

        let events = sink.events();
        let trace_names = events
            .iter()
            .filter_map(|event| match event {
                TelemetryEvent::SessionTrace(trace) => Some(trace.name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(trace_names.contains(&"decisioning_snapshot"));

        let emitted_events = reporter_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(!emitted_events.is_empty());
        assert!(matches!(
            emitted_events[0].kind,
            crate::DecisioningEventKind::ToolSelection
        ));
    }

    #[test]
    fn decisioning_blocks_sensitive_tool_inputs_before_execution() {
        struct SensitiveToolApiClient;

        impl ApiClient for SensitiveToolApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("blocked".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }

                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-sensitive".to_string(),
                        name: "safe_tool".to_string(),
                        input: "delete secret token credential network shell overwrite remove".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(false)
                .with_max_parallelism(2),
        );

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SensitiveToolApiClient,
            StaticToolExecutor::new().register("safe_tool", |_input| {
                panic!("safe_tool should be blocked by decisioning before execution")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        );

        let summary = runtime
            .run_turn("handle sensitive operation", None)
            .expect("conversation should continue after decisioning block");

        assert_eq!(summary.tool_results.len(), 1);
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult {
                tool_name,
                is_error: true,
                output,
                ..
            } if tool_name == "safe_tool" && output.contains("Decisioning")
        ));
    }

    #[test]
    fn records_denied_tool_results_when_prompt_rejects() {
        struct RejectPrompter;
        impl PermissionPrompter for RejectPrompter {
            fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
                PermissionPromptDecision::Deny {
                    reason: "not now".to_string(),
                }
            }
        }

        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("I could not use the tool.".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: "secret".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("use the tool", Some(&mut RejectPrompter))
            .expect("conversation should continue after denied tool");

        assert_eq!(summary.tool_results.len(), 1);
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult { is_error: true, output, .. } if output == "not now"
        ));
    }

    #[test]
    fn denies_tool_use_when_pre_tool_hook_blocks() {
        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("blocked".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: r#"{"path":"secret.txt"}"#.to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("tool should not execute when hook denies")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'blocked by hook'; exit 2")],
                Vec::new(),
                Vec::new(),
            )),
        );

        let summary = runtime
            .run_turn("use the tool", None)
            .expect("conversation should continue after hook denial");

        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "hook denial should produce an error result: {output}"
        );
        assert!(
            output.contains("denied tool") || output.contains("blocked by hook"),
            "unexpected hook denial output: {output:?}"
        );
    }

    #[test]
    fn denies_tool_use_when_pre_tool_hook_fails() {
        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("failed".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: r#"{"path":"secret.txt"}"#.to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        // given
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("tool should not execute when hook fails")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'broken hook'; exit 1")],
                Vec::new(),
                Vec::new(),
            )),
        );

        // when
        let summary = runtime
            .run_turn("use the tool", None)
            .expect("conversation should continue after hook failure");

        // then
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "hook failure should produce an error result: {output}"
        );
        assert!(
            output.contains("exited with status 1") || output.contains("broken hook"),
            "unexpected hook failure output: {output:?}"
        );
    }

    #[test]
    fn appends_post_tool_hook_feedback_to_tool_result() {
        struct TwoCallApiClient {
            calls: usize,
        }

        impl ApiClient for TwoCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: r#"{"lhs":2,"rhs":2}"#.to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::Tool));
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TwoCallApiClient { calls: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'pre hook ran'")],
                vec![shell_snippet("printf 'post hook ran'")],
                Vec::new(),
            )),
        );

        let summary = runtime
            .run_turn("use add", None)
            .expect("tool loop succeeds");

        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            !*is_error,
            "post hook should preserve non-error result: {output:?}"
        );
        assert!(
            output.contains('4'),
            "tool output missing value: {output:?}"
        );
        assert!(
            output.contains("pre hook ran"),
            "tool output missing pre hook feedback: {output:?}"
        );
        assert!(
            output.contains("post hook ran"),
            "tool output missing post hook feedback: {output:?}"
        );
    }

    #[test]
    fn appends_post_tool_use_failure_hook_feedback_to_tool_result() {
        struct TwoCallApiClient {
            calls: usize,
        }

        impl ApiClient for TwoCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "fail".to_string(),
                            input: r#"{"path":"README.md"}"#.to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::Tool));
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        // given
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TwoCallApiClient { calls: 0 },
            StaticToolExecutor::new()
                .register("fail", |_input| Err(ToolError::new("tool exploded"))),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                Vec::new(),
                vec![shell_snippet("printf 'post hook should not run'")],
                vec![shell_snippet("printf 'failure hook ran'")],
            )),
        );

        // when
        let summary = runtime
            .run_turn("use fail", None)
            .expect("tool loop succeeds");

        // then
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "failure hook path should preserve error result: {output:?}"
        );
        assert!(
            output.contains("tool exploded"),
            "tool output missing failure reason: {output:?}"
        );
        assert!(
            output.contains("failure hook ran"),
            "tool output missing failure hook feedback: {output:?}"
        );
        assert!(
            !output.contains("post hook should not run"),
            "normal post hook should not run on tool failure: {output:?}"
        );
    }

    #[test]
    fn reconstructs_usage_tracker_from_restored_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session
            .messages
            .push(crate::session::ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "earlier".to_string(),
                }],
                Some(TokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_creation_input_tokens: 2,
                    cache_read_input_tokens: 1,
                }),
            ));

        let runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        assert_eq!(runtime.usage().turns(), 1);
        assert_eq!(runtime.usage().cumulative_usage().total_tokens(), 21);
    }

    #[test]
    fn compacts_session_after_turns() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        runtime.run_turn("a", None).expect("turn a");
        runtime.run_turn("b", None).expect("turn b");
        runtime.run_turn("c", None).expect("turn c");

        let result = runtime.compact(CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        });
        assert!(result.summary.contains("Conversation summary"));
        assert_eq!(
            result.compacted_session.messages[0].role,
            MessageRole::System
        );
        assert_eq!(
            result.compacted_session.session_id,
            runtime.session().session_id
        );
        assert!(result.compacted_session.compaction.is_some());
    }

    #[test]
    fn persists_conversation_turn_messages_to_jsonl_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let path = temp_session_path("persisted-turn");
        let session = Session::new().with_persistence_path(path.clone());
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime
            .run_turn("persist this turn", None)
            .expect("turn should succeed");

        let restored = Session::load_from_path(&path).expect("persisted session should reload");
        fs::remove_file(&path).expect("temp session file should be removable");

        assert_eq!(restored.messages.len(), 2);
        assert_eq!(restored.messages[0].role, MessageRole::User);
        assert_eq!(restored.messages[1].role, MessageRole::Assistant);
        assert_eq!(restored.session_id, runtime.session().session_id);
    }

    #[test]
    fn forks_runtime_session_without_mutating_original() {
        let mut session = Session::new();
        session
            .push_user_text("branch me")
            .expect("message should append");

        let runtime = ConversationRuntime::new(
            session.clone(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let forked = runtime.fork_session(Some("alt-path".to_string()));

        assert_eq!(forked.messages, session.messages);
        assert_ne!(forked.session_id, session.session_id);
        assert_eq!(
            forked
                .fork
                .as_ref()
                .map(|fork| (fork.parent_session_id.as_str(), fork.branch_name.as_deref())),
            Some((session.session_id.as_str(), Some("alt-path")))
        );
        assert!(runtime.session().fork.is_none());
    }

    fn temp_session_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-conversation-{label}-{nanos}.json"))
    }

    #[cfg(windows)]
    fn shell_snippet(script: &str) -> String {
        script.replace('\'', "\"")
    }

    #[cfg(not(windows))]
    fn shell_snippet(script: &str) -> String {
        script.to_string()
    }

    #[test]
    fn auto_compacts_when_cumulative_input_threshold_is_crossed() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::Usage(TokenUsage {
                        input_tokens: 120_000,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::user_text("one"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two".to_string(),
            }]),
            crate::session::ConversationMessage::user_text("three"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four".to_string(),
            }]),
        ];

        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");

        assert_eq!(
            summary.auto_compaction,
            Some(AutoCompactionEvent {
                removed_message_count: 2,
            })
        );
        assert_eq!(runtime.session().messages[0].role, MessageRole::System);
    }

    #[test]
    fn skips_auto_compaction_below_threshold() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::Usage(TokenUsage {
                        input_tokens: 99_999,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");
        assert_eq!(summary.auto_compaction, None);
        assert_eq!(runtime.session().messages.len(), 2);
    }

    #[test]
    fn auto_compaction_threshold_defaults_and_parses_values() {
        assert_eq!(
            parse_auto_compaction_threshold(None),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
        assert_eq!(parse_auto_compaction_threshold(Some("4321")), 4321);
        assert_eq!(
            parse_auto_compaction_threshold(Some("0")),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
        assert_eq!(
            parse_auto_compaction_threshold(Some("not-a-number")),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
    }

    #[test]
    fn build_assistant_message_requires_message_stop_event() {
        // given
        let events = vec![AssistantEvent::TextDelta("hello".to_string())];

        // when
        let error = build_assistant_message(events)
            .expect_err("assistant messages should require a stop event");

        // then
        assert!(error
            .to_string()
            .contains("assistant stream ended without a message stop event"));
    }

    #[test]
    fn reasoning_step_demo_creates_visualizable_events() {
        // This test demonstrates how reasoning steps would be generated
        // In a real implementation, these would be emitted during AI processing

        let demo_reasoning_events = vec![
            AssistantEvent::ReasoningStep(crate::conversation::ReasoningStep::Analysis {
                content: "The user is asking me to analyze a codebase and provide insights.".to_string(),
                confidence: Some(0.92),
            }),
            AssistantEvent::ReasoningStep(crate::conversation::ReasoningStep::Planning {
                plan: "I need to examine the project structure, understand the technology stack, and provide actionable recommendations.".to_string(),
                steps: vec![
                    "Analyze project structure and dependencies".to_string(),
                    "Review code quality and patterns".to_string(),
                    "Identify potential improvements".to_string(),
                    "Provide specific recommendations".to_string(),
                ],
            }),
            AssistantEvent::ReasoningStep(crate::conversation::ReasoningStep::Decision {
                choice: "Use comprehensive analysis approach".to_string(),
                reasoning: "The user wants deep insights, so I'll perform thorough analysis rather than surface-level review.".to_string(),
            }),
            AssistantEvent::TextDelta("Based on my analysis of your codebase, here are the key insights:".to_string()),
            AssistantEvent::ReasoningStep(crate::conversation::ReasoningStep::Reflection {
                critique: "My initial analysis was comprehensive but could be more focused on immediate actionable items.".to_string(),
                adjustment: Some("Prioritize recommendations by impact and ease of implementation".to_string()),
            }),
            AssistantEvent::TextDelta("The most impactful improvements would be...".to_string()),
            AssistantEvent::MessageStop,
        ];

        // Verify the events can be processed without errors
        let result = build_assistant_message(demo_reasoning_events);
        assert!(result.is_ok(), "Reasoning steps should not break message building");

        let (message, _, _, chain_opt) = result.unwrap();
        assert_eq!(message.blocks.len(), 2); // Two text blocks
        assert!(chain_opt.is_some(), "Chain of thought should be collected");
    }

    #[test]
    fn build_assistant_message_requires_content() {
        // given
        let events = vec![AssistantEvent::MessageStop];

        // when
        let error =
            build_assistant_message(events).expect_err("assistant messages should require content");

        // then
        assert!(error
            .to_string()
            .contains("assistant stream produced no content"));
    }

    #[test]
    fn static_tool_executor_rejects_unknown_tools() {
        // given
        let mut executor = StaticToolExecutor::new();

        // when
        let error = executor
            .execute("missing", "{}")
            .expect_err("unregistered tools should fail");

        // then
        assert_eq!(error.to_string(), "unknown tool: missing");
    }

    #[test]
    fn run_turn_errors_when_max_iterations_is_exceeded() {
        struct LoopingApi;

        impl ApiClient for LoopingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "echo".to_string(),
                        input: "payload".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        // given
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            LoopingApi,
            StaticToolExecutor::new().register("echo", |input| Ok(input.to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_max_iterations(1);

        // when
        let error = runtime
            .run_turn("loop", None)
            .expect_err("conversation loop should stop after the configured limit");

        // then
        assert!(error
            .to_string()
            .contains("conversation loop exceeded the maximum number of iterations"));
    }

    #[test]
    fn run_turn_propagates_api_errors() {
        struct FailingApi;

        impl ApiClient for FailingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Err(RuntimeError::new("upstream failed"))
            }
        }

        // given
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            FailingApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        // when
        let error = runtime
            .run_turn("hello", None)
            .expect_err("API failures should propagate");

        // then
        assert_eq!(error.to_string(), "upstream failed");
    }
}
