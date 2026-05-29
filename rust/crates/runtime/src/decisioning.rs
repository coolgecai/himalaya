use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::DecisioningSafetyPolicyConfig;
use crate::conversation::ChainOfThought;

/// A concrete tool candidate considered by the decisioning skeleton.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub capabilities: Vec<String>,
    pub avg_success_rate: f32,
    pub avg_latency_ms: u32,
    pub cost: f32,
    pub parallelizable: bool,
}

impl Tool {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        capabilities: Vec<String>,
        avg_success_rate: f32,
        avg_latency_ms: u32,
        cost: f32,
        parallelizable: bool,
    ) -> Self {
        Self {
            name: name.into(),
            capabilities,
            avg_success_rate,
            avg_latency_ms,
            cost,
            parallelizable,
        }
    }
}

fn normalize_tool_identifier(input: &str) -> String {
    let normalized = input
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();

    if normalized.is_empty() {
        input.to_ascii_lowercase()
    } else {
        normalized
    }
}

fn infer_capability_tokens(tool_name: &str, description: Option<&str>) -> Vec<String> {
    let mut capabilities = BTreeSet::new();
    let normalized_name = normalize_tool_identifier(tool_name);
    if !normalized_name.is_empty() {
        capabilities.insert(normalized_name);
    }

    let mut profile = tool_name.to_ascii_lowercase();
    if let Some(description) = description {
        profile.push(' ');
        profile.push_str(&description.to_ascii_lowercase());
    }

    for token in profile
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
    {
        capabilities.insert(token.to_string());
    }

    for (needles, family) in [
        (&["read"][..], "read"),
        (&["write", "writefile", "write_file"][..], "write"),
        (&["edit", "editfile", "edit_file", "patch"][..], "edit"),
        (&["search", "grep", "glob", "find", "scan"][..], "search"),
        (&["file", "filesystem", "path"][..], "file"),
        (&["bash", "shell", "powershell", "command"][..], "shell"),
        (&["git", "diff", "commit", "branch"][..], "git"),
        (&["mcp", "server", "resource"][..], "mcp"),
        (&["agent", "subagent", "task"][..], "agent"),
        (&["memory", "remember", "topic"][..], "memory"),
        (&["model", "llm", "reasoning"][..], "model"),
        (&["permission", "auth", "approval"][..], "permission"),
        (&["test", "spec", "benchmark"][..], "test"),
    ] {
        if needles.iter().any(|needle| profile.contains(needle)) {
            capabilities.insert(family.to_string());
        }
    }

    capabilities.into_iter().collect()
}

#[must_use]
pub fn infer_tool_capabilities(tool_name: &str, description: Option<&str>) -> Vec<String> {
    infer_capability_tokens(tool_name, description)
}

#[must_use]
pub fn tool_from_profile(
    tool_name: &str,
    description: Option<&str>,
    input_schema: Option<&Value>,
) -> Tool {
    let profile = format!(
        "{} {} {}",
        tool_name.to_ascii_lowercase(),
        description.unwrap_or_default().to_ascii_lowercase(),
        input_schema
            .map_or_else(String::new, Value::to_string)
            .to_ascii_lowercase()
    );

    let mut avg_success_rate = 0.72_f32;
    if profile.contains("fast") || profile.contains("preferred") || profile.contains("reliable") {
        avg_success_rate += 0.12;
    }
    if profile.contains("slow") || profile.contains("legacy") || profile.contains("experimental") {
        avg_success_rate -= 0.10;
    }
    if profile.contains("search") || profile.contains("read") || profile.contains("list") {
        avg_success_rate += 0.04;
    }
    if profile.contains("write") || profile.contains("edit") || profile.contains("patch") {
        avg_success_rate -= 0.02;
    }
    if profile.contains("bash") || profile.contains("shell") || profile.contains("powershell") {
        avg_success_rate -= 0.05;
    }
    avg_success_rate = avg_success_rate.clamp(0.30, 0.98);

    let mut avg_latency_ms = 120_u32;
    if profile.contains("fast") || profile.contains("search") || profile.contains("read") {
        avg_latency_ms = 60;
    }
    if profile.contains("write") || profile.contains("edit") {
        avg_latency_ms = avg_latency_ms.max(100);
    }
    if profile.contains("bash") || profile.contains("shell") || profile.contains("powershell") {
        avg_latency_ms = 240;
    }
    if profile.contains("mcp") || profile.contains("agent") {
        avg_latency_ms = 300;
    }
    if profile.contains("slow") {
        avg_latency_ms = avg_latency_ms.saturating_add(140);
    }

    let mut cost = 0.10_f32;
    if profile.contains("search") || profile.contains("read") {
        cost = 0.05;
    }
    if profile.contains("write") || profile.contains("edit") {
        cost = 0.12;
    }
    if profile.contains("bash") || profile.contains("shell") || profile.contains("mcp") {
        cost = 0.30;
    }
    if profile.contains("agent") {
        cost = 0.35;
    }

    let parallelizable = !(profile.contains("bash")
        || profile.contains("shell")
        || profile.contains("powershell")
        || profile.contains("write")
        || profile.contains("edit")
        || profile.contains("agent"));

    Tool::new(
        tool_name.to_string(),
        infer_capability_tokens(tool_name, description),
        avg_success_rate,
        avg_latency_ms,
        cost,
        parallelizable,
    )
}

/// Context used while selecting tools and building plans.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReasoningContext {
    pub chain_of_thought: Option<ChainOfThought>,
    pub workspace_root: Option<PathBuf>,
    pub memory_topics: Vec<String>,
    pub recent_tool_history: Vec<ToolHistoryEntry>,
    pub active_constraints: Vec<String>,
    pub max_parallelism: usize,
}

impl ReasoningContext {
    #[must_use]
    pub fn effective_parallelism(&self) -> usize {
        self.max_parallelism.max(1)
    }
}

/// Historical signal for one tool invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolHistoryEntry {
    pub tool_name: String,
    pub succeeded: bool,
    pub latency_ms: u32,
    pub note: Option<String>,
}

/// Input task used by the decisioning skeleton.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub description: String,
    pub complexity: u8,
    pub required_capabilities: Vec<String>,
    pub constraints: Vec<String>,
}

impl Task {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        description: impl Into<String>,
        complexity: u8,
        required_capabilities: Vec<String>,
        constraints: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            complexity,
            required_capabilities,
            constraints,
        }
    }
}

/// One decomposed step produced by the planner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subtask {
    pub id: String,
    pub title: String,
    pub required_capabilities: Vec<String>,
    pub candidate_tools: Vec<String>,
    pub parallelizable: bool,
    pub estimated_effort: u8,
    pub notes: Vec<String>,
}

/// Serial or parallel execution choice for a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExecutionMode {
    Serial,
    Parallel { max_concurrency: usize },
}

impl ExecutionMode {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Serial => "serial",
            Self::Parallel { .. } => "parallel",
        }
    }
}

/// A decomposed and schedulable plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskPlan {
    pub task_id: String,
    pub steps: Vec<Subtask>,
    pub execution_mode: ExecutionMode,
    pub confidence: f32,
    pub notes: Vec<String>,
}

/// Outcome produced by a step execution in the dynamic adjustment loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepOutcome {
    pub step_id: String,
    pub succeeded: bool,
    pub latency_ms: u32,
    pub notes: Vec<String>,
}

/// A revised plan generated after a failure or new constraint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanAdjustment {
    pub original_task_id: String,
    pub revised_plan: TaskPlan,
    pub reason: String,
    pub changed_step_ids: Vec<String>,
}

/// Safety decision used by the constraint checker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SafetyOutcome {
    Allow,
    Review,
    Deny,
}

/// Risk scoring result for a task step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskAssessment {
    pub score: f32,
    pub outcome: SafetyOutcome,
    pub reasons: Vec<String>,
}

/// High-level risk classification derived from the safety outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

/// Score assigned to a tool candidate during selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolScore {
    pub name: String,
    pub score: f32,
    pub success_rate: f32,
    pub latency_ms: u32,
    pub cost: f32,
    pub parallelizable: bool,
    pub capabilities: Vec<String>,
    pub selected: bool,
}

/// Node in the rendered decisioning plan tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanNodeKind {
    Task,
    Step,
}

/// Tree node used by the UI to visualize the decomposed plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanTreeNode {
    pub kind: PlanNodeKind,
    pub id: String,
    pub title: String,
    pub parallelizable: bool,
    pub estimated_effort: u8,
    pub candidate_tools: Vec<String>,
    pub notes: Vec<String>,
    pub children: Vec<PlanTreeNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanDagEdgeKind {
    Contains,
    DependsOn,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanDagNode {
    pub kind: PlanNodeKind,
    pub id: String,
    pub title: String,
    pub parallelizable: bool,
    pub estimated_effort: u8,
    pub candidate_tools: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanDagEdge {
    pub from: String,
    pub to: String,
    pub kind: PlanDagEdgeKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanDag {
    pub task_id: String,
    pub root_id: String,
    pub nodes: Vec<PlanDagNode>,
    pub edges: Vec<PlanDagEdge>,
}

/// Serialized event that can be streamed to VS Code later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisioningEventKind {
    ToolSelection,
    TaskDecomposition,
    ParallelismDecision,
    SafetyAssessment,
    PlanAdjustment,
}

/// Streamable decisioning payload for the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisioningEvent {
    pub kind: DecisioningEventKind,
    pub title: String,
    pub summary: String,
    pub task_id: String,
    pub confidence: Option<f32>,
    pub risk_score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_level: Option<RiskLevel>,
    pub selected_tools: Vec<String>,
    pub parallelizable: Option<bool>,
    pub action: Option<SafetyOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_scores: Option<Vec<ToolScore>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_tree: Option<PlanTreeNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_dag: Option<PlanDag>,
    pub details: Vec<String>,
}

/// A snapshot that bundles the selector, plan, and safety result together.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisioningSnapshot {
    pub task: Task,
    pub selected_tools: Vec<Tool>,
    pub plan: TaskPlan,
    pub risk: RiskAssessment,
    pub events: Vec<DecisioningEvent>,
}

/// Tool selector that scores tools by capability, history, and cost.
#[derive(Debug, Clone)]
pub struct ToolSelector {
    pub available_tools: Vec<Tool>,
    pub reasoning_context: ReasoningContext,
}

impl ToolSelector {
    #[must_use]
    pub fn new(available_tools: Vec<Tool>, reasoning_context: ReasoningContext) -> Self {
        Self {
            available_tools,
            reasoning_context,
        }
    }

    #[must_use]
    pub fn select_optimal_tools(&self, task: &Task) -> Vec<Tool> {
        let mut scored = self
            .available_tools
            .iter()
            .cloned()
            .map(|tool| (self.score_tool(&tool, task), tool))
            .collect::<Vec<_>>();

        scored.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.1.name.cmp(&right.1.name))
        });

        let limit = self.reasoning_context.effective_parallelism().clamp(1, 4);
        scored
            .into_iter()
            .take(limit)
            .map(|(_, tool)| tool)
            .collect()
    }

    fn score_tool(&self, tool: &Tool, task: &Task) -> f32 {
        let capability_hits = tool
            .capabilities
            .iter()
            .filter(|capability| {
                task.required_capabilities
                    .iter()
                    .any(|required| required == *capability)
            })
            .count() as f32;
        let history_bonus = self
            .reasoning_context
            .recent_tool_history
            .iter()
            .filter(|entry| entry.tool_name == tool.name && entry.succeeded)
            .count() as f32
            * 0.6;
        let memory_bonus = self
            .reasoning_context
            .memory_topics
            .iter()
            .filter(|topic| {
                tool.name.contains(topic.as_str())
                    || tool
                        .capabilities
                        .iter()
                        .any(|capability| capability.contains(topic.as_str()))
            })
            .count() as f32
            * 0.2;
        let latency_penalty = tool.avg_latency_ms as f32 / 1000.0;
        let cost_penalty = tool.cost.max(0.0);
        let parallel_bonus = if tool.parallelizable && task.complexity > 2 {
            0.25
        } else {
            0.0
        };
        let confidence_hint = self
            .reasoning_context
            .chain_of_thought
            .as_ref()
            .map(|chain| chain.confidence)
            .unwrap_or(0.5);

        capability_hits * 3.0
            + tool.avg_success_rate * 4.0
            + history_bonus
            + memory_bonus
            + parallel_bonus
            + confidence_hint * 0.2
            - latency_penalty
            - cost_penalty
    }
}

/// Task planner that creates a decomposed execution plan.
#[derive(Debug, Clone)]
pub struct TaskPlanner {
    pub max_parallelism: usize,
}

impl TaskPlanner {
    #[must_use]
    pub fn new(max_parallelism: usize) -> Self {
        Self {
            max_parallelism: max_parallelism.max(1),
        }
    }

    #[must_use]
    pub fn decompose_task(&self, task: &Task, selector: &ToolSelector) -> TaskPlan {
        let selected_tools = selector.select_optimal_tools(task);
        let selected_tool_names = selected_tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();

        let mut steps = vec![Subtask {
            id: format!("{}-analyze", task.id),
            title: format!("Analyze {}", task.description),
            required_capabilities: task.required_capabilities.clone(),
            candidate_tools: selected_tool_names.clone(),
            parallelizable: false,
            estimated_effort: task.complexity.saturating_add(1),
            notes: vec!["Start with a shared understanding of the task.".to_string()],
        }];

        if task.required_capabilities.len() > 1 {
            for capability in &task.required_capabilities {
                steps.push(Subtask {
                    id: format!("{}-{}", task.id, sanitize_step_id(capability)),
                    title: format!("Handle {capability} work"),
                    required_capabilities: vec![capability.clone()],
                    candidate_tools: selected_tools
                        .iter()
                        .filter(|tool| tool.capabilities.iter().any(|item| item == capability))
                        .map(|tool| tool.name.clone())
                        .collect(),
                    parallelizable: true,
                    estimated_effort: task.complexity.max(1),
                    notes: vec!["Capability-level partition for parallel execution.".to_string()],
                });
            }
        } else {
            steps.push(Subtask {
                id: format!("{}-execute", task.id),
                title: format!("Execute {}", task.description),
                required_capabilities: task.required_capabilities.clone(),
                candidate_tools: selected_tool_names.clone(),
                parallelizable: false,
                estimated_effort: task.complexity.max(1),
                notes: vec!["Single-track execution path.".to_string()],
            });
        }

        if task.complexity >= 3 {
            steps.push(Subtask {
                id: format!("{}-verify", task.id),
                title: format!("Verify {}", task.description),
                required_capabilities: vec!["verification".to_string()],
                candidate_tools: selected_tool_names.clone(),
                parallelizable: false,
                estimated_effort: 1,
                notes: vec!["Validate the result before presenting it to the user.".to_string()],
            });
        }

        let execution_mode = self.choose_execution_mode(&steps);
        let confidence = self.estimate_confidence(task, &selected_tools, &steps);

        TaskPlan {
            task_id: task.id.clone(),
            steps,
            execution_mode,
            confidence,
            notes: vec!["Phase-2 planner skeleton: capability-aware decomposition.".to_string()],
        }
    }

    #[must_use]
    pub fn choose_execution_mode(&self, steps: &[Subtask]) -> ExecutionMode {
        let parallelizable_steps = steps.iter().filter(|step| step.parallelizable).count();
        if parallelizable_steps > 1 && self.max_parallelism > 1 {
            ExecutionMode::Parallel {
                max_concurrency: self.max_parallelism,
            }
        } else {
            ExecutionMode::Serial
        }
    }

    #[must_use]
    pub fn adjust_plan(&self, plan: &TaskPlan, outcomes: &[StepOutcome]) -> PlanAdjustment {
        let mut revised_plan = plan.clone();
        let mut changed_step_ids = Vec::new();

        if outcomes.iter().any(|outcome| !outcome.succeeded) {
            revised_plan.execution_mode = ExecutionMode::Serial;
            revised_plan
                .notes
                .push("Replanned after a failed step; fall back to serial execution.".to_string());

            for outcome in outcomes.iter().filter(|outcome| !outcome.succeeded) {
                changed_step_ids.push(outcome.step_id.clone());
                revised_plan.steps.push(Subtask {
                    id: format!("{}-retry", outcome.step_id),
                    title: format!("Retry {}", outcome.step_id),
                    required_capabilities: vec!["recovery".to_string()],
                    candidate_tools: Vec::new(),
                    parallelizable: false,
                    estimated_effort: 1,
                    notes: vec!["Recovery step generated from a failed outcome.".to_string()],
                });
            }
        }

        let reason = if changed_step_ids.is_empty() {
            "No adjustment was needed.".to_string()
        } else {
            format!("Adjusted {} failed step(s).", changed_step_ids.len())
        };

        PlanAdjustment {
            original_task_id: plan.task_id.clone(),
            revised_plan,
            reason,
            changed_step_ids,
        }
    }

    fn estimate_confidence(&self, task: &Task, selected_tools: &[Tool], steps: &[Subtask]) -> f32 {
        let capability_coverage = if task.required_capabilities.is_empty() {
            0.5
        } else {
            let matched = selected_tools
                .iter()
                .flat_map(|tool| tool.capabilities.iter())
                .filter(|capability| {
                    task.required_capabilities
                        .iter()
                        .any(|required| required == *capability)
                })
                .count() as f32;
            (matched / task.required_capabilities.len() as f32).clamp(0.0, 1.0)
        };
        let structural_bonus = if steps.len() > 2 { 0.1 } else { 0.0 };
        (capability_coverage * 0.7 + structural_bonus).clamp(0.0, 1.0)
    }
}

/// Safety policy that applies a simple risk model.
#[derive(Debug, Clone)]
pub struct SafetyPolicy {
    pub base_score: f32,
    pub review_threshold: f32,
    pub deny_threshold: f32,
    pub protected_terms: Vec<String>,
    pub protected_term_penalty: f32,
    pub constraint_overlap_penalty: f32,
    pub parallel_step_penalty: f32,
    pub high_complexity_threshold: u8,
    pub high_complexity_penalty: f32,
    pub low_confidence_threshold: f32,
    pub low_confidence_penalty: f32,
}

impl Default for SafetyPolicy {
    fn default() -> Self {
        Self {
            base_score: 0.05,
            review_threshold: 0.35,
            deny_threshold: 0.70,
            protected_terms: vec![
                "delete".to_string(),
                "remove".to_string(),
                "overwrite".to_string(),
                "secret".to_string(),
                "token".to_string(),
                "credential".to_string(),
                "network".to_string(),
                "shell".to_string(),
            ],
            protected_term_penalty: 0.18,
            constraint_overlap_penalty: 0.10,
            parallel_step_penalty: 0.04,
            high_complexity_threshold: 4,
            high_complexity_penalty: 0.08,
            low_confidence_threshold: 0.40,
            low_confidence_penalty: 0.08,
        }
    }
}

fn permission_mode_risk_penalty(mode: &str) -> Option<(f32, &'static str)> {
    match mode {
        "read-only" => Some((0.0, "Read-only mode keeps the action constrained.")),
        "workspace-write" => Some((0.03, "Workspace-write mode can modify local files.")),
        "prompt" => Some((0.05, "Prompt mode may require extra approval.")),
        "danger-full-access" => Some((0.12, "Danger-full-access mode broadens execution scope.")),
        "allow" => Some((0.15, "Allow mode bypasses normal permission barriers.")),
        _ => Some((0.04, "Unrecognized permission mode requires review.")),
    }
}

fn capability_risk_signal(capability: &str) -> Option<(f32, &'static str)> {
    match capability {
        "shell" | "bash" | "powershell" | "command" => {
            Some((0.14, "High-impact execution capability."))
        }
        "write" | "edit" | "delete" => Some((0.09, "Mutates workspace state.")),
        "mcp" | "agent" => Some((0.08, "Delegates work to another runtime or agent.")),
        "permission" | "auth" => Some((0.06, "Touches authorization-sensitive flow.")),
        "model" => Some((0.05, "Routes through model-selection or reasoning tooling.")),
        _ => None,
    }
}

impl SafetyPolicy {
    #[must_use]
    pub fn from_config(config: &DecisioningSafetyPolicyConfig) -> Self {
        Self {
            base_score: f32::from(config.base_score_percent()) / 100.0,
            review_threshold: f32::from(config.review_threshold_percent()) / 100.0,
            deny_threshold: f32::from(
                config
                    .deny_threshold_percent()
                    .max(config.review_threshold_percent()),
            ) / 100.0,
            protected_terms: config.protected_terms().to_vec(),
            protected_term_penalty: f32::from(config.protected_term_penalty_percent()) / 100.0,
            constraint_overlap_penalty: f32::from(config.constraint_overlap_penalty_percent())
                / 100.0,
            parallel_step_penalty: f32::from(config.parallel_step_penalty_percent()) / 100.0,
            high_complexity_threshold: config.high_complexity_threshold(),
            high_complexity_penalty: f32::from(config.high_complexity_penalty_percent()) / 100.0,
            low_confidence_threshold: f32::from(config.low_confidence_threshold_percent()) / 100.0,
            low_confidence_penalty: f32::from(config.low_confidence_penalty_percent()) / 100.0,
        }
    }

    #[must_use]
    pub fn assess(
        &self,
        task: &Task,
        step: &Subtask,
        context: &ReasoningContext,
    ) -> RiskAssessment {
        let mut score = self.base_score;
        let mut reasons = Vec::new();
        let corpus = format!(
            "{} {} {}",
            task.description,
            step.title,
            task.constraints.join(" ")
        )
        .to_lowercase();

        let mut capability_signals = BTreeSet::new();
        for capability in &step.required_capabilities {
            capability_signals.insert(capability.to_lowercase());
        }
        for tool_name in &step.candidate_tools {
            for capability in infer_tool_capabilities(tool_name, None) {
                capability_signals.insert(capability);
            }
        }

        for term in &self.protected_terms {
            if corpus.contains(term) {
                score += self.protected_term_penalty;
                reasons.push(format!("Matched protected term: {term}"));
            }
        }

        for constraint in &context.active_constraints {
            let normalized = constraint.trim().to_lowercase();
            if let Some(mode) = normalized.strip_prefix("permission-mode:") {
                if let Some((penalty, explanation)) = permission_mode_risk_penalty(mode.trim()) {
                    score += penalty;
                    if penalty > 0.0 || mode.trim() != "read-only" {
                        reasons.push(format!("Active permission mode {mode}: {explanation}"));
                    }
                }
                continue;
            }

            if let Some(root) = normalized.strip_prefix("workspace-root:") {
                reasons.push(format!("Workspace boundary anchored at {root}."));
                continue;
            }

            if corpus.contains(&normalized) {
                score += self.constraint_overlap_penalty;
                reasons.push(format!("Constraint overlap: {constraint}"));
            }
        }

        for capability in capability_signals {
            if let Some((penalty, explanation)) = capability_risk_signal(&capability) {
                score += penalty;
                reasons.push(format!("Capability signal {capability}: {explanation}"));
            }

            if context
                .active_constraints
                .iter()
                .any(|constraint| constraint.to_lowercase().contains(&capability))
            {
                score += self.constraint_overlap_penalty;
                reasons.push(format!("Constraint overlap: capability {capability}"));
            }
        }

        if step.parallelizable {
            score += self.parallel_step_penalty;
            reasons.push("Parallel step requires extra boundary checks.".to_string());
        }

        if task.complexity >= self.high_complexity_threshold {
            score += self.high_complexity_penalty;
            reasons.push("High-complexity task raises uncertainty.".to_string());
        }

        if context
            .chain_of_thought
            .as_ref()
            .map(|chain| chain.confidence)
            .unwrap_or(0.5)
            < self.low_confidence_threshold
        {
            score += self.low_confidence_penalty;
            reasons.push("Low reasoning confidence increases review need.".to_string());
        }

        score = score.clamp(0.0, 1.0);
        let outcome = if score >= self.deny_threshold {
            SafetyOutcome::Deny
        } else if score >= self.review_threshold {
            SafetyOutcome::Review
        } else {
            SafetyOutcome::Allow
        };

        RiskAssessment {
            score,
            outcome,
            reasons,
        }
    }

    #[must_use]
    pub fn check_boundary(&self, assessment: &RiskAssessment) -> SafetyOutcome {
        assessment.outcome.clone()
    }
}

fn classify_risk_level(outcome: &SafetyOutcome) -> RiskLevel {
    match outcome {
        SafetyOutcome::Allow => RiskLevel::Low,
        SafetyOutcome::Review => RiskLevel::Medium,
        SafetyOutcome::Deny => RiskLevel::High,
    }
}

#[must_use]
pub fn build_plan_dag(task: &Task, plan: &TaskPlan, selected_tools: &[Tool]) -> PlanDag {
    let root_id = task.id.clone();
    let mut nodes = vec![PlanDagNode {
        kind: PlanNodeKind::Task,
        id: root_id.clone(),
        title: task.description.clone(),
        parallelizable: matches!(plan.execution_mode, ExecutionMode::Parallel { .. }),
        estimated_effort: task.complexity,
        candidate_tools: selected_tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect(),
        notes: plan.notes.clone(),
    }];
    let mut edges = Vec::new();

    for (index, step) in plan.steps.iter().enumerate() {
        nodes.push(PlanDagNode {
            kind: PlanNodeKind::Step,
            id: step.id.clone(),
            title: step.title.clone(),
            parallelizable: step.parallelizable,
            estimated_effort: step.estimated_effort,
            candidate_tools: step.candidate_tools.clone(),
            notes: step.notes.clone(),
        });
        edges.push(PlanDagEdge {
            from: root_id.clone(),
            to: step.id.clone(),
            kind: PlanDagEdgeKind::Contains,
        });

        if index == 0 {
            continue;
        }

        match plan.execution_mode {
            ExecutionMode::Serial => edges.push(PlanDagEdge {
                from: plan.steps[index - 1].id.clone(),
                to: step.id.clone(),
                kind: PlanDagEdgeKind::DependsOn,
            }),
            ExecutionMode::Parallel { .. } if step.parallelizable => {
                if let Some(predecessor) = plan.steps[..index]
                    .iter()
                    .rev()
                    .find(|candidate| !candidate.parallelizable)
                {
                    edges.push(PlanDagEdge {
                        from: predecessor.id.clone(),
                        to: step.id.clone(),
                        kind: PlanDagEdgeKind::DependsOn,
                    });
                }
            }
            ExecutionMode::Parallel { .. } => {
                for predecessor in &plan.steps[..index] {
                    edges.push(PlanDagEdge {
                        from: predecessor.id.clone(),
                        to: step.id.clone(),
                        kind: PlanDagEdgeKind::DependsOn,
                    });
                }
            }
        }
    }

    PlanDag {
        task_id: task.id.clone(),
        root_id,
        nodes,
        edges,
    }
}

#[must_use]
pub fn build_plan_tree(task: &Task, plan: &TaskPlan, selected_tools: &[Tool]) -> PlanTreeNode {
    PlanTreeNode {
        kind: PlanNodeKind::Task,
        id: task.id.clone(),
        title: task.description.clone(),
        parallelizable: matches!(plan.execution_mode, ExecutionMode::Parallel { .. }),
        estimated_effort: task.complexity,
        candidate_tools: selected_tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect(),
        notes: plan.notes.clone(),
        children: plan
            .steps
            .iter()
            .map(|step| PlanTreeNode {
                kind: PlanNodeKind::Step,
                id: step.id.clone(),
                title: step.title.clone(),
                parallelizable: step.parallelizable,
                estimated_effort: step.estimated_effort,
                candidate_tools: step.candidate_tools.clone(),
                notes: step.notes.clone(),
                children: Vec::new(),
            })
            .collect(),
    }
}

fn build_tool_scores(
    task: &Task,
    selector: &ToolSelector,
    selected_tools: &[Tool],
) -> Vec<ToolScore> {
    let mut scores = selector
        .available_tools
        .iter()
        .map(|tool| ToolScore {
            name: tool.name.clone(),
            score: selector.score_tool(tool, task),
            success_rate: tool.avg_success_rate,
            latency_ms: tool.avg_latency_ms,
            cost: tool.cost,
            parallelizable: tool.parallelizable,
            capabilities: tool.capabilities.clone(),
            selected: selected_tools
                .iter()
                .any(|selected| selected.name == tool.name),
        })
        .collect::<Vec<_>>();

    scores.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.name.cmp(&right.name))
    });

    scores
}

/// High-level decisioning bundle intended for later runtime integration.
#[derive(Debug, Clone)]
pub struct DecisioningEngine {
    pub selector: ToolSelector,
    pub planner: TaskPlanner,
    pub safety: SafetyPolicy,
}

impl DecisioningEngine {
    #[must_use]
    pub fn new(selector: ToolSelector, planner: TaskPlanner, safety: SafetyPolicy) -> Self {
        Self {
            selector,
            planner,
            safety,
        }
    }

    #[must_use]
    pub fn analyze(&self, task: &Task) -> DecisioningSnapshot {
        let selected_tools = self.selector.select_optimal_tools(task);
        let plan = self.planner.decompose_task(task, &self.selector);
        let risk = plan
            .steps
            .iter()
            .map(|step| {
                self.safety
                    .assess(task, step, &self.selector.reasoning_context)
            })
            .max_by(|left, right| {
                left.score
                    .partial_cmp(&right.score)
                    .unwrap_or(Ordering::Equal)
            })
            .unwrap_or_else(|| RiskAssessment {
                score: 0.0,
                outcome: SafetyOutcome::Allow,
                reasons: vec!["No steps available for risk assessment.".to_string()],
            });
        let tool_scores = build_tool_scores(task, &self.selector, &selected_tools);
        let plan_tree = build_plan_tree(task, &plan, &selected_tools);
        let plan_dag = build_plan_dag(task, &plan, &selected_tools);
        let risk_level = classify_risk_level(&risk.outcome);

        let mut events = Vec::new();
        events.push(DecisioningEvent {
            kind: DecisioningEventKind::ToolSelection,
            title: "Tool selection".to_string(),
            summary: format!("Selected {} tool(s) for the task.", selected_tools.len()),
            task_id: task.id.clone(),
            confidence: Some(plan.confidence),
            risk_score: None,
            risk_level: None,
            selected_tools: selected_tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
            parallelizable: None,
            action: None,
            tool_scores: Some(tool_scores.clone()),
            plan_tree: None,
            plan_dag: None,
            details: selected_tools
                .iter()
                .map(|tool| {
                    format!(
                        "{} [{:.0}% success]",
                        tool.name,
                        tool.avg_success_rate * 100.0
                    )
                })
                .collect(),
        });
        events.push(DecisioningEvent {
            kind: DecisioningEventKind::TaskDecomposition,
            title: "Task decomposition".to_string(),
            summary: format!("Split into {} step(s).", plan.steps.len()),
            task_id: task.id.clone(),
            confidence: Some(plan.confidence),
            risk_score: None,
            risk_level: None,
            selected_tools: selected_tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
            parallelizable: Some(plan.steps.iter().any(|step| step.parallelizable)),
            action: None,
            tool_scores: None,
            plan_tree: Some(plan_tree.clone()),
            plan_dag: Some(plan_dag.clone()),
            details: plan
                .steps
                .iter()
                .map(|step| format!("{} -> {}", step.id, step.title))
                .collect(),
        });
        events.push(DecisioningEvent {
            kind: DecisioningEventKind::ParallelismDecision,
            title: "Execution mode".to_string(),
            summary: format!("Planning to run in {} mode.", plan.execution_mode.label()),
            task_id: task.id.clone(),
            confidence: Some(plan.confidence),
            risk_score: None,
            risk_level: None,
            selected_tools: selected_tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
            parallelizable: Some(matches!(
                plan.execution_mode,
                ExecutionMode::Parallel { .. }
            )),
            action: None,
            tool_scores: None,
            plan_tree: None,
            plan_dag: None,
            details: vec![format!("Execution mode: {}", plan.execution_mode.label())],
        });
        events.push(DecisioningEvent {
            kind: DecisioningEventKind::SafetyAssessment,
            title: "Safety assessment".to_string(),
            summary: format!("Risk score {:.2} -> {:?}", risk.score, risk.outcome),
            task_id: task.id.clone(),
            confidence: Some(plan.confidence),
            risk_score: Some(risk.score),
            risk_level: Some(risk_level),
            selected_tools: selected_tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
            parallelizable: None,
            action: Some(risk.outcome.clone()),
            tool_scores: None,
            plan_tree: None,
            plan_dag: None,
            details: risk.reasons.clone(),
        });

        DecisioningSnapshot {
            task: task.clone(),
            selected_tools,
            plan,
            risk,
            events,
        }
    }
}

fn sanitize_step_id(input: &str) -> String {
    input
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_prefers_matching_tools_with_better_history() {
        let context = ReasoningContext {
            memory_topics: vec!["fs".to_string()],
            recent_tool_history: vec![ToolHistoryEntry {
                tool_name: "fs-writer".to_string(),
                succeeded: true,
                latency_ms: 12,
                note: None,
            }],
            max_parallelism: 2,
            ..ReasoningContext::default()
        };

        let selector = ToolSelector::new(
            vec![
                Tool::new("search", vec!["search".to_string()], 0.70, 40, 0.2, true),
                Tool::new("fs-writer", vec!["fs".to_string()], 0.95, 15, 0.1, false),
            ],
            context,
        );
        let task = Task::new(
            "task-1",
            "update files",
            3,
            vec!["fs".to_string()],
            Vec::new(),
        );

        let selected = selector.select_optimal_tools(&task);
        assert_eq!(
            selected.first().map(|tool| tool.name.as_str()),
            Some("fs-writer")
        );
    }

    #[test]
    fn planner_splits_complex_tasks_and_enables_parallel_mode() {
        let context = ReasoningContext {
            max_parallelism: 4,
            ..ReasoningContext::default()
        };
        let selector = ToolSelector::new(
            vec![
                Tool::new("planner", vec!["planning".to_string()], 0.9, 10, 0.1, true),
                Tool::new(
                    "executor",
                    vec!["execution".to_string()],
                    0.9,
                    10,
                    0.1,
                    true,
                ),
            ],
            context,
        );
        let planner = TaskPlanner::new(4);
        let task = Task::new(
            "task-2",
            "ship a feature",
            4,
            vec!["planning".to_string(), "execution".to_string()],
            Vec::new(),
        );

        let plan = planner.decompose_task(&task, &selector);
        assert!(plan.steps.len() >= 3);
        assert!(matches!(
            plan.execution_mode,
            ExecutionMode::Parallel { .. }
        ));
        assert!(plan.confidence >= 0.0 && plan.confidence <= 1.0);
    }

    #[test]
    fn plan_dag_records_root_steps_and_dependencies() {
        let selector = ToolSelector::new(
            vec![Tool::new(
                "executor",
                vec!["execution".to_string()],
                0.9,
                10,
                0.1,
                true,
            )],
            ReasoningContext::default(),
        );
        let planner = TaskPlanner::new(1);
        let task = Task::new(
            "task-dag",
            "run serial work",
            2,
            vec!["execution".to_string()],
            Vec::new(),
        );
        let plan = planner.decompose_task(&task, &selector);
        let selected_tools = selector.select_optimal_tools(&task);

        let dag = build_plan_dag(&task, &plan, &selected_tools);

        assert_eq!(dag.task_id, "task-dag");
        assert_eq!(dag.root_id, "task-dag");
        assert_eq!(dag.nodes.len(), plan.steps.len() + 1);
        assert!(dag.edges.iter().any(|edge| {
            edge.from == "task-dag"
                && edge.to == plan.steps[0].id
                && edge.kind == PlanDagEdgeKind::Contains
        }));
        assert!(dag.edges.iter().any(|edge| {
            edge.from == plan.steps[0].id
                && edge.to == plan.steps[1].id
                && edge.kind == PlanDagEdgeKind::DependsOn
        }));
    }

    #[test]
    fn safety_policy_reacts_to_sensitive_terms() {
        let policy = SafetyPolicy::default();
        let task = Task::new(
            "task-3",
            "delete secrets from disk",
            2,
            vec!["fs".to_string()],
            vec!["no-network".to_string()],
        );
        let step = Subtask {
            id: "task-3-analyze".to_string(),
            title: "Delete secrets from disk".to_string(),
            required_capabilities: vec!["fs-write".to_string()],
            candidate_tools: Vec::new(),
            parallelizable: false,
            estimated_effort: 1,
            notes: Vec::new(),
        };
        let context = ReasoningContext {
            active_constraints: vec!["no-network".to_string()],
            ..ReasoningContext::default()
        };

        let assessment = policy.assess(&task, &step, &context);
        assert!(assessment.score >= 0.35);
        assert!(matches!(
            assessment.outcome,
            SafetyOutcome::Review | SafetyOutcome::Deny
        ));
        assert!(!assessment.reasons.is_empty());
    }

    #[test]
    fn safety_policy_uses_permission_mode_and_capability_signals() {
        let policy = SafetyPolicy::default();
        let task = Task::new(
            "task-3b",
            "review workspace state",
            2,
            vec!["inspection".to_string()],
            Vec::new(),
        );
        let step = Subtask {
            id: "task-3b-analyze".to_string(),
            title: "Inspect project state".to_string(),
            required_capabilities: vec!["shell".to_string(), "write".to_string()],
            candidate_tools: vec!["bash".to_string()],
            parallelizable: false,
            estimated_effort: 1,
            notes: Vec::new(),
        };
        let context = ReasoningContext {
            active_constraints: vec![
                "permission-mode:danger-full-access".to_string(),
                "workspace-root:/tmp/project".to_string(),
            ],
            ..ReasoningContext::default()
        };

        let assessment = policy.assess(&task, &step, &context);
        assert!(assessment.score >= policy.review_threshold);
        assert!(matches!(
            assessment.outcome,
            SafetyOutcome::Review | SafetyOutcome::Deny
        ));
        assert!(assessment
            .reasons
            .iter()
            .any(|reason| reason.contains("Active permission mode danger-full-access")));
        assert!(assessment.reasons.iter().any(|reason| {
            reason.contains("Capability signal shell") || reason.contains("Capability signal bash")
        }));
    }

    #[test]
    fn engine_builds_a_serializable_snapshot() {
        let selector = ToolSelector::new(
            vec![Tool::new(
                "search",
                vec!["search".to_string()],
                0.8,
                25,
                0.1,
                true,
            )],
            ReasoningContext::default(),
        );
        let engine = DecisioningEngine::new(selector, TaskPlanner::new(2), SafetyPolicy::default());
        let task = Task::new(
            "task-4",
            "gather context",
            2,
            vec!["search".to_string()],
            Vec::new(),
        );

        let snapshot = engine.analyze(&task);
        assert_eq!(snapshot.task.id, "task-4");
        assert_eq!(snapshot.events.len(), 4);
        assert!(snapshot.events[0]
            .tool_scores
            .as_ref()
            .map(|scores| !scores.is_empty())
            .unwrap_or(false));
        assert!(snapshot.events[1].plan_tree.is_some());
        assert!(snapshot.events[1].plan_dag.is_some());
        assert!(snapshot.events[3].risk_level.is_some());
        let serialized = serde_json::to_string(&snapshot).expect("snapshot should serialize");
        assert!(serialized.contains("tool_selection"));
        assert!(serialized.contains("plan_tree"));
        assert!(serialized.contains("plan_dag"));
        assert!(serialized.contains("risk_level"));
    }
}
