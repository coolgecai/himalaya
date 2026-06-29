use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::time::Instant;

// Multi-role team-convergence node execution lives in a child module so it can
// access ConversationRuntime's private fields/methods while keeping this file
// focused. See conversation_team.rs.
#[path = "conversation_team.rs"]
mod conversation_team;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use telemetry::SessionTracer;

use crate::compact::{
    compact_session, compact_session_with, estimate_session_tokens, CompactionConfig,
    CompactionResult,
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
    DecisioningEventKind, DecisioningSnapshot, ExecutionScheduler, FailureClassification,
    FailureClassifier, MoERoutingPolicy, ModelRouteDecision, ModelRouteFeedback, ModelRouter,
    PlanExecution, PlanExecutionEvent, ReasoningContext, RecoveryActionEngine,
    RecoveryActionExecution, RecoveryOrchestrator, RecoveryOrchestratorOutcome, RiskAssessment,
    RuntimeEvent, RuntimeEventReporter, SafetyOutcome, SafetyPolicy, StepOutcome, Subtask, Task,
    TaskExecutionEngine, TaskExecutionOutcome, TaskExecutionStep, TaskExecutionStepKind,
    TaskPacket, TaskRegistry, TeamExecutionEvent, TeamExecutionLedger, Tool, ToolHistoryEntry,
    ToolSelector, VerificationDecision, VerificationResult, VerificationRunner,
};

const DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD: u32 = 100_000;
const DEFAULT_MAX_CONVERSATION_ITERATIONS: usize = 64;
/// How many times a turn may automatically re-drive the model to fix a failed
/// verification before giving up. Each attempt feeds the verification failure
/// and recovery plan back into the conversation, then re-verifies. A value of
/// `0` preserves the legacy single-shot behavior (fail the turn immediately).
const DEFAULT_MAX_RECOVERY_ATTEMPTS: usize = 2;
const DOCUMENT_GENERATION_GATE_MAX_PROMPTS: usize = 3;
const AUTO_COMPACTION_THRESHOLD_ENV_VAR: &str = "Himalaya_CODE_AUTO_COMPACT_INPUT_TOKENS";
const WORKSPACE_CONTEXT_MAX_ENTRIES: usize = 220;
const WORKSPACE_CONTEXT_MAX_DEPTH: usize = 4;
const WORKSPACE_CONTEXT_MAX_SOURCE_FILES: usize = 14;

/// Fully assembled request payload sent to the upstream model client.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiRequest {
    pub system_prompt: Vec<String>,
    pub messages: Vec<ConversationMessage>,
    pub model_route: Option<ModelRouteDecision>,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReasoningStep {
    Analysis {
        content: String,
        confidence: Option<f32>,
        signature: Option<String>,
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
    RedactedThinking {
        data: String,
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

/// Alternative approach suggestion recorded alongside `ChainOfThought`.
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

impl Default for ChainOfThought {
    fn default() -> Self {
        Self::new()
    }
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
            if let ReasoningStep::Analysis {
                confidence: Some(c),
                ..
            } = s
            {
                sum += *c;
                count += 1;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    General,
    UserIdentity,
    AssistantIdentity,
    LanguagePreference,
    UserPreference,
}

impl Default for MemoryKind {
    fn default() -> Self {
        Self::General
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    #[serde(default)]
    pub kind: MemoryKind,
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
        // Primary path: workspace .Himalaya/long_term_memory.json
        let primary =
            workspace_root.map(|root| root.join(".Himalaya").join("long_term_memory.json"));
        // Fallback path: ~/.Himalaya/knowledge.json
        let fallback = std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".Himalaya").join("knowledge.json"));

        let mut fallback_entries = Vec::new();
        if let Some(candidate) = fallback.as_deref() {
            if let Ok(contents) = fs::read_to_string(candidate) {
                if let Ok(entries) = serde_json::from_str::<Vec<MemoryEntry>>(&contents) {
                    fallback_entries = entries;
                }
            }
        }

        if let Some(primary_path) = primary.as_deref() {
            let entries = fs::read_to_string(primary_path)
                .ok()
                .and_then(|contents| serde_json::from_str::<Vec<MemoryEntry>>(&contents).ok())
                .unwrap_or_else(|| fallback_entries.clone());
            return Self {
                path: primary_path.to_path_buf(),
                entries,
            };
        }

        if let Some(fallback_path) = fallback {
            return Self {
                path: fallback_path,
                entries: fallback_entries,
            };
        }

        Self {
            path: PathBuf::from(".Himalaya/knowledge.json"),
            entries: Vec::new(),
        }
    }

    pub fn add_entry(
        &mut self,
        topic: impl Into<String>,
        note: impl Into<String>,
        confidence: f32,
    ) {
        self.add_typed_entry(MemoryKind::General, topic, note, confidence);
    }

    pub fn add_typed_entry(
        &mut self,
        kind: MemoryKind,
        topic: impl Into<String>,
        note: impl Into<String>,
        confidence: f32,
    ) {
        let topic = topic.into();
        let note = note.into();
        if topic.trim().is_empty() || note.trim().is_empty() {
            return;
        }
        let confidence = confidence.clamp(0.0, 1.0);
        let now = current_time_millis();

        // Upsert: if an existing entry has the same kind + topic + normalised note,
        // update its confidence and timestamp instead of appending a duplicate.
        let normalized_note = note.trim().to_lowercase();
        if let Some(existing) = self.entries.iter_mut().find(|entry| {
            entry.kind == kind
                && entry.topic == topic
                && entry.note.trim().to_lowercase() == normalized_note
        }) {
            existing.confidence = confidence.max(existing.confidence);
            existing.ts_ms = now;
        } else {
            self.entries.push(MemoryEntry {
                kind,
                topic,
                note,
                confidence,
                ts_ms: now,
            });
        }
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

    #[must_use]
    pub fn relevant_entries(&self, query: &str, limit: usize) -> Vec<MemoryEntry> {
        let query_tokens = memory_query_tokens(query);
        let mut ranked = self
            .entries
            .iter()
            .filter(|entry| !entry.note.trim().is_empty())
            .map(|entry| {
                let haystack = format!("{} {}", entry.topic, entry.note).to_lowercase();
                let token_hits = query_tokens
                    .iter()
                    .filter(|token| haystack.contains(token.as_str()))
                    .count() as f32;
                let kind_bonus = match entry.kind {
                    MemoryKind::UserIdentity
                    | MemoryKind::AssistantIdentity
                    | MemoryKind::LanguagePreference
                    | MemoryKind::UserPreference => 2.0,
                    MemoryKind::General => 0.0,
                };
                let score = token_hits + kind_bonus + entry.confidence;
                (score, entry)
            })
            .filter(|(score, entry)| *score > entry.confidence || query_tokens.is_empty())
            .collect::<Vec<_>>();

        ranked.sort_by(|(left_score, left), (right_score, right)| {
            right_score
                .partial_cmp(left_score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| {
                    right
                        .confidence
                        .partial_cmp(&left.confidence)
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(|| right.ts_ms.cmp(&left.ts_ms))
        });

        ranked
            .into_iter()
            .map(|(_, entry)| entry.clone())
            .take(limit)
            .collect()
    }

    pub fn save(&self) -> Result<(), std::io::Error> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.entries).unwrap_or_else(|_| "[]".to_string());
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, &json)?;
        fs::rename(&tmp, &self.path)
    }
}

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn memory_query_tokens(query: &str) -> BTreeSet<String> {
    query
        .split(|ch: char| !ch.is_alphanumeric())
        .map(|token| token.trim().to_lowercase())
        .filter(|token| token.chars().count() >= 2)
        .take(64)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnTaskState {
    objective: String,
    requires_workspace_analysis: bool,
    requires_document_generation: bool,
    requires_generate_file_deliverable: bool,
    requires_super_ppt_deliverable: bool,
    required_evidence: Vec<&'static str>,
    source_attachments: Vec<AttachmentEvidence>,
    observed_tools: BTreeSet<String>,
    observed_files: BTreeSet<String>,
    read_file_calls: usize,
    generated_document: bool,
    generated_document_quality_blocked: bool,
    generated_document_quality_summary: Option<String>,
    document_asset_manifest_path: Option<String>,
    super_ppt_pptx_artifact: bool,
    super_ppt_manifest_artifact: bool,
    super_ppt_slide_artifact: bool,
    failed_tools: Vec<String>,
    evidence_gate_prompts: usize,
    document_generation_gate_prompts: usize,
    super_ppt_gate_prompts: usize,
    output_language: Option<String>,
}

impl TurnTaskState {
    fn new(
        user_input: &str,
        requires_workspace_analysis: bool,
        requires_document_generation: bool,
    ) -> Self {
        let required_evidence = if requires_workspace_analysis {
            vec![
                "directory/file discovery",
                "manifest or project metadata review",
                "multiple source file reads",
                "content search for entry points or module names",
            ]
        } else {
            Vec::new()
        };
        let requires_super_ppt_deliverable =
            requires_document_generation && is_super_ppt_request(user_input);
        Self {
            objective: user_input.trim().chars().take(240).collect(),
            requires_workspace_analysis,
            requires_document_generation,
            requires_generate_file_deliverable: requires_document_generation
                && !requires_super_ppt_deliverable,
            requires_super_ppt_deliverable,
            required_evidence,
            source_attachments: Vec::new(),
            observed_tools: BTreeSet::new(),
            observed_files: BTreeSet::new(),
            read_file_calls: 0,
            generated_document: false,
            generated_document_quality_blocked: false,
            generated_document_quality_summary: None,
            document_asset_manifest_path: None,
            super_ppt_pptx_artifact: false,
            super_ppt_manifest_artifact: false,
            super_ppt_slide_artifact: false,
            failed_tools: Vec::new(),
            evidence_gate_prompts: 0,
            document_generation_gate_prompts: 0,
            super_ppt_gate_prompts: 0,
            output_language: None,
        }
    }

    fn set_output_language(&mut self, language: Option<String>) {
        self.output_language = language;
    }

    fn set_source_attachments(&mut self, source_attachments: Vec<AttachmentEvidence>) {
        self.source_attachments = source_attachments;
    }

    fn record_tool_use(&mut self, tool_name: &str, input: &str) {
        self.observed_tools.insert(tool_name.to_string());
        let normalized = normalize_tool_key(tool_name);
        if normalized.contains("readfile") || normalized == "read" {
            self.read_file_calls += 1;
        }
        if let Some(path) = extract_json_string_field(input, "path") {
            self.observed_files.insert(path);
        }
        if let Some(pattern) = extract_json_string_field(input, "pattern") {
            self.observed_files.insert(pattern);
        }
    }

    fn record_tool_result(&mut self, tool_name: &str, is_error: bool, output: &str) {
        if is_error {
            self.failed_tools.push(format!(
                "{tool_name}: {}",
                output.chars().take(160).collect::<String>()
            ));
        } else if is_document_asset_extract_tool(tool_name) {
            if let Some(path) = document_asset_manifest_path_from_output(output) {
                self.document_asset_manifest_path = Some(path);
            }
        } else if is_generate_file_tool(tool_name) {
            self.generated_document = true;
            if let Some(summary) = document_quality_blocking_summary_for_request(
                output,
                &self.objective,
                &self.source_attachments,
            ) {
                self.generated_document_quality_blocked = true;
                self.generated_document_quality_summary = Some(summary);
            } else {
                self.generated_document_quality_blocked = false;
                self.generated_document_quality_summary = None;
            }
        }
        self.observe_super_ppt_artifacts(output);
        for file in extract_file_candidates(output).into_iter().take(12) {
            self.observe_super_ppt_artifacts(&file);
            self.observed_files.insert(file);
        }
    }

    fn source_read_count(&self) -> usize {
        self.read_file_calls
    }

    fn has_search(&self) -> bool {
        self.observed_tools.iter().any(|name| {
            let normalized = normalize_tool_key(name);
            normalized.contains("grep")
                || normalized.contains("glob")
                || normalized.contains("search")
        })
    }

    fn evidence_complete(&self) -> bool {
        !self.requires_workspace_analysis || (self.has_search() && self.source_read_count() >= 3)
    }

    fn should_prompt_for_evidence(&mut self) -> bool {
        if !self.requires_workspace_analysis
            || self.evidence_complete()
            || self.evidence_gate_prompts >= 2
        {
            return false;
        }
        self.evidence_gate_prompts += 1;
        true
    }

    fn document_generation_complete(&self) -> bool {
        !self.requires_generate_file_deliverable
            || (self.generated_document && !self.generated_document_quality_blocked)
    }

    fn observe_super_ppt_artifacts(&mut self, text: &str) {
        if !self.requires_super_ppt_deliverable {
            return;
        }
        let lower = text.to_lowercase();
        if lower.contains(".pptx") {
            self.super_ppt_pptx_artifact = true;
        }
        if super_ppt_manifest_text(&lower) {
            self.super_ppt_manifest_artifact = true;
        }
        if super_ppt_slide_text(&lower) {
            self.super_ppt_slide_artifact = true;
        }
    }

    fn super_ppt_artifact_complete(&self) -> bool {
        !self.requires_super_ppt_deliverable
            || (self.super_ppt_pptx_artifact
                && (self.super_ppt_manifest_artifact || self.super_ppt_slide_artifact))
    }

    fn should_prompt_for_super_ppt_completion(&mut self, assistant_text: &str) -> bool {
        if !self.requires_super_ppt_deliverable {
            return false;
        }
        self.observe_super_ppt_artifacts(assistant_text);
        if self.super_ppt_artifact_complete()
            || super_ppt_blocker_text(assistant_text)
            || self.super_ppt_gate_prompts >= 1
        {
            return false;
        }
        self.super_ppt_gate_prompts += 1;
        true
    }

    fn should_prompt_for_document_generation(&mut self) -> bool {
        if !self.requires_generate_file_deliverable
            || self.document_generation_complete()
            || self.document_generation_gate_prompts >= DOCUMENT_GENERATION_GATE_MAX_PROMPTS
        {
            return false;
        }
        self.document_generation_gate_prompts += 1;
        true
    }

    fn format_context(&self) -> String {
        let mut lines = vec![
            "# Structured task state".to_string(),
            format!("- Objective: {}", self.objective),
        ];
        if let Some(language) = &self.output_language {
            lines.push(format!("- Output language: {language}"));
            lines.push(format!("- {}", language_output_contract(language)));
        }
        if self.requires_workspace_analysis {
            lines.push("- Task class: current workspace/source analysis".to_string());
            lines.push(format!(
                "- Required evidence before final answer: {}",
                self.required_evidence.join("; ")
            ));
            lines.push(format!("- Evidence complete: {}", self.evidence_complete()));
        }
        if self.requires_document_generation {
            lines.push("- Task class: binary document generation".to_string());
            if self.requires_generate_file_deliverable {
                lines.push(
                    "- Required deliverable before final answer: successful quality-compliant document-generation tool result"
                        .to_string(),
                );
            } else {
                lines.push(
                    "- Required deliverable is governed by the active SuperPPT skill: imagegen manifests and PPTX artifacts; do not fall back to generate_file if the skill's imagegen hard gate is blocked."
                        .to_string(),
                );
            }
            if !self.source_attachments.is_empty() {
                lines.push(format!(
                    "- User-provided source attachments: {}",
                    self.source_attachments
                        .iter()
                        .take(8)
                        .map(AttachmentEvidence::summary)
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
                lines.push(
                    "- Attachment contract: the source document is already available in conversation context; do not ask the user to upload or paste it again."
                        .to_string(),
                );
                if requires_chunk_guided_document_reading(&self.source_attachments) {
                    lines.push(
                        "- Long-document strategy: first build/use section, figure, table, formula, and experiment-result indexes; then read only the required chunks/assets by index before composing document_spec."
                            .to_string(),
                    );
                }
            }
            if self.requires_generate_file_deliverable {
                lines.push(format!(
                    "- Generate-file completion gate complete: {}",
                    self.document_generation_complete()
                ));
                if let Some(summary) = &self.generated_document_quality_summary {
                    lines.push(format!(
                        "- Last generated document failed quality contract: {summary}"
                    ));
                }
                if let Some(path) = &self.document_asset_manifest_path {
                    lines.push(format!(
                        "- Extracted source asset manifest available for generation: {path}"
                    ));
                }
            } else if self.requires_super_ppt_deliverable {
                lines.push(format!(
                    "- SuperPPT completion gate complete: {}; pptx: {}; manifest: {}; slides: {}",
                    self.super_ppt_artifact_complete(),
                    self.super_ppt_pptx_artifact,
                    self.super_ppt_manifest_artifact,
                    self.super_ppt_slide_artifact
                ));
            } else {
                lines.push(
                    "- Generate-file completion gate active: false; skill-specific deliverable applies."
                        .to_string(),
                );
            }
        }
        if !self.observed_tools.is_empty() {
            lines.push(format!(
                "- Tools used this turn: {}",
                self.observed_tools
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.observed_files.is_empty() {
            lines.push(format!(
                "- Files/patterns observed this turn: {}",
                self.observed_files
                    .iter()
                    .take(16)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.failed_tools.is_empty() {
            lines.push("- Failed tool attempts:".to_string());
            lines.extend(
                self.failed_tools
                    .iter()
                    .take(6)
                    .map(|item| format!("  - {item}")),
            );
        }
        lines.join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachmentEvidence {
    label: String,
    kind: &'static str,
    extracted_chars: Option<usize>,
    details: Vec<String>,
}

impl AttachmentEvidence {
    fn summary(&self) -> String {
        let mut summary = match (self.kind, self.extracted_chars) {
            ("text", Some(chars)) => {
                format!("{} (extracted text: {chars} chars)", self.label)
            }
            ("image", _) => format!("{} (image)", self.label),
            _ => self.label.clone(),
        };
        if !self.details.is_empty() {
            summary.push_str("; ");
            summary.push_str(&self.details.join("; "));
        }
        summary
    }

    fn text(label: impl Into<String>, extracted_chars: Option<usize>) -> Self {
        Self {
            label: label.into(),
            kind: "text",
            extracted_chars,
            details: Vec::new(),
        }
    }

    fn image(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            kind: "image",
            extracted_chars: None,
            details: Vec::new(),
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        if !detail.trim().is_empty() {
            self.details.push(detail);
        }
        self
    }
}

fn requires_chunk_guided_document_reading(source_attachments: &[AttachmentEvidence]) -> bool {
    source_attachments.iter().any(|attachment| {
        attachment.extracted_chars.unwrap_or_default() >= 30_000
            || attachment
                .details
                .iter()
                .any(|detail| detail.contains("chunks:") || detail.contains("fullTextPath:"))
    })
}

fn push_capabilities(capabilities: &mut Vec<String>, values: &[&str]) {
    capabilities.extend(values.iter().map(|value| (*value).to_string()));
}

fn infer_prompt_capabilities(user_input: &str) -> Vec<String> {
    let lower = user_input.to_lowercase();
    let mut capabilities = Vec::new();

    if user_requests_current_workspace_analysis(user_input)
        || [
            "codebase",
            "source code",
            "directory structure",
            "repo",
            "repository",
            "当前工程",
            "源码",
            "目录结构",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        push_capabilities(
            &mut capabilities,
            &[
                "search",
                "read",
                "file",
                "grep",
                "glob",
                "workspace",
                "evidence",
                "source-analysis",
            ],
        );
    }

    if [
        "modify",
        "implement",
        "fix",
        "refactor",
        "edit",
        "write",
        "修改",
        "实现",
        "修复",
        "重构",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        push_capabilities(&mut capabilities, &["read", "edit", "write", "test"]);
    }

    if ["test", "verify", "validate", "run", "测试", "验证", "运行"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        push_capabilities(&mut capabilities, &["shell", "test", "verification"]);
    }

    if [
        "research",
        "web",
        "fetch",
        "search online",
        "查阅",
        "联网",
        "调研",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        push_capabilities(&mut capabilities, &["web", "research", "search", "fetch"]);
    }

    if user_requests_document_generation(user_input) {
        push_capabilities(
            &mut capabilities,
            &[
                "generatefile",
                "document",
                "document-generation",
                "file",
                "ppt",
                "pptx",
                "slides",
                "deck",
            ],
        );
    }

    if [
        "agent",
        "worker",
        "parallel",
        "multi-agent",
        "多agent",
        "并行",
        "分解",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        push_capabilities(&mut capabilities, &["agent", "worker", "team", "planning"]);
    }

    if capabilities.is_empty() {
        push_capabilities(&mut capabilities, &["read", "analysis", "planning"]);
    }

    capabilities.sort();
    capabilities.dedup();
    capabilities
}

fn workspace_evidence_tools_available(available_tool_names: &BTreeSet<String>) -> bool {
    let has_search = available_tool_names.iter().any(|name| {
        let normalized = normalize_tool_key(name);
        normalized.contains("grep") || normalized.contains("glob") || normalized.contains("search")
    });
    let has_read = available_tool_names.iter().any(|name| {
        let normalized = normalize_tool_key(name);
        normalized.contains("read")
    });
    has_search && has_read
}

fn user_requests_document_generation(input: &str) -> bool {
    let lower = input.to_lowercase();
    let mentions_document_format = [
        "ppt",
        "pptx",
        "powerpoint",
        "幻灯片",
        "演示文稿",
        "答辩",
        "word",
        "docx",
        "pdf",
        "excel",
        "xlsx",
        "电子文档",
        "表格",
        "spreadsheet",
        "slides",
        "deck",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if !mentions_document_format {
        return false;
    }

    [
        "生成",
        "制作",
        "创建",
        "输出",
        "导出",
        "整理",
        "撰写",
        "编写",
        "做一份",
        "转换",
        "generate",
        "create",
        "make",
        "build",
        "export",
        "produce",
        "draft",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn is_generate_file_tool(tool_name: &str) -> bool {
    matches!(
        normalize_tool_key(tool_name).as_str(),
        "generatefile"
            | "functiongeneratefile"
            | "generatepptx"
            | "generatedocx"
            | "generatepdf"
            | "generatexlsx"
            | "mcpdocservicegeneratepptx"
            | "mcpdocservicegeneratedocx"
            | "mcpdocservicegeneratepdf"
            | "mcpdocservicegeneratexlsx"
    )
}

fn is_document_asset_extract_tool(tool_name: &str) -> bool {
    matches!(
        normalize_tool_key(tool_name).as_str(),
        "extractdocumentassets" | "mcpdocserviceextractdocumentassets"
    )
}

fn is_doc_service_generation_tool(tool_name: &str) -> bool {
    matches!(
        normalize_tool_key(tool_name).as_str(),
        "mcpdocservicegeneratepptx"
            | "mcpdocservicegeneratedocx"
            | "mcpdocservicegeneratepdf"
            | "mcpdocservicegeneratexlsx"
    )
}

fn mcp_wrapper_document_generation_tool_name(input: &str) -> Option<String> {
    let value: Value = serde_json::from_str(input).ok()?;
    let object = value.as_object()?;
    let qualified_name = object
        .get("qualifiedName")
        .or_else(|| object.get("qualified_name"))
        .or_else(|| object.get("tool"))
        .and_then(Value::as_str)?;
    is_doc_service_generation_tool(qualified_name).then(|| qualified_name.to_string())
}

fn observed_document_generation_tool_name(tool_name: &str, input: &str) -> Option<String> {
    if is_generate_file_tool(tool_name) {
        return Some(tool_name.to_string());
    }
    if normalize_tool_key(tool_name) == "mcptool" {
        return mcp_wrapper_document_generation_tool_name(input);
    }
    None
}

fn document_generation_tool_available(available_tool_names: &BTreeSet<String>) -> bool {
    available_tool_names
        .iter()
        .any(|name| is_generate_file_tool(name))
}

fn document_generation_tool_format(user_input: &str, tool_name: &str) -> &'static str {
    match normalize_tool_key(tool_name).as_str() {
        "mcpdocservicegeneratedocx" | "generatedocx" => "docx",
        "mcpdocservicegeneratepdf" | "generatepdf" => "pdf",
        "mcpdocservicegeneratexlsx" | "generatexlsx" => "xlsx",
        "mcpdocservicegeneratepptx" | "generatepptx" => "pptx",
        _ => infer_requested_document_format(user_input),
    }
}

fn infer_requested_document_format(user_input: &str) -> &'static str {
    let lower = user_input.to_lowercase();
    if lower.contains("docx") || lower.contains("word") {
        "docx"
    } else if lower.contains("xlsx") || lower.contains("excel") || lower.contains("表格") {
        "xlsx"
    } else if lower.contains("pdf") {
        "pdf"
    } else if requested_presentation(user_input) {
        "pptx"
    } else {
        "docx"
    }
}

fn requested_presentation(user_input: &str) -> bool {
    let lower = user_input.to_lowercase();
    [
        "ppt",
        "pptx",
        "powerpoint",
        "slide",
        "slides",
        "deck",
        "幻灯片",
        "演示文稿",
        "答辩",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn preferred_document_generation_tool_name(
    user_input: &str,
    available_tool_names: &BTreeSet<String>,
) -> Option<String> {
    let preferred = match infer_requested_document_format(user_input) {
        "pptx" => [
            "mcpdocservicegeneratepptx",
            "generatepptx",
            "generatefile",
            "functiongeneratefile",
        ]
        .as_slice(),
        "docx" => [
            "mcpdocservicegeneratedocx",
            "generatedocx",
            "generatefile",
            "functiongeneratefile",
        ]
        .as_slice(),
        "pdf" => [
            "mcpdocservicegeneratepdf",
            "generatepdf",
            "generatefile",
            "functiongeneratefile",
        ]
        .as_slice(),
        "xlsx" => [
            "mcpdocservicegeneratexlsx",
            "generatexlsx",
            "generatefile",
            "functiongeneratefile",
        ]
        .as_slice(),
        _ => ["generatefile", "functiongeneratefile"].as_slice(),
    };
    for target in preferred {
        if let Some(name) = available_tool_names
            .iter()
            .find(|name| normalize_tool_key(name) == *target)
        {
            return Some(name.clone());
        }
    }
    available_tool_names
        .iter()
        .find(|name| is_generate_file_tool(name))
        .cloned()
}

fn document_asset_extract_tool_name(available_tool_names: &BTreeSet<String>) -> Option<String> {
    available_tool_names
        .iter()
        .find(|name| is_document_asset_extract_tool(name))
        .cloned()
}

fn document_asset_manifest_path_from_output(output: &str) -> Option<String> {
    let value: Value = serde_json::from_str(output).ok()?;
    json_str(&value, "manifest_path", "manifestPath")
        .or_else(|| json_str(&value, "asset_manifest_path", "assetManifestPath"))
        .or_else(|| json_str(&value, "assets_manifest_path", "assetsManifestPath"))
        .filter(|path| !path.trim().is_empty())
        .map(str::to_string)
}

fn request_requires_figure_assets(user_input: &str) -> bool {
    let lower = user_input.to_lowercase();
    [
        "图片",
        "插图",
        "图像",
        "图表",
        "图文并茂",
        "配图",
        "示意图",
        "原图",
        "扣取",
        "挖取",
        "figure",
        "figures",
        "image",
        "images",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn request_requires_table_assets(user_input: &str) -> bool {
    let lower = user_input.to_lowercase();
    ["表格", "数据表", "table", "tables"]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn request_requires_formula_assets(user_input: &str) -> bool {
    let lower = user_input.to_lowercase();
    ["公式", "方程", "equation", "formula", "latex"]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn request_requires_chart_assets(user_input: &str) -> bool {
    let lower = user_input.to_lowercase();
    [
        "图表",
        "曲线",
        "柱状图",
        "折线图",
        "chart",
        "charts",
        "plot",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn minimum_expected_slide_count(
    user_input: &str,
    source_attachments: &[AttachmentEvidence],
) -> usize {
    if let Some(count) = estimate_requested_slide_count(user_input) {
        return count.max(if source_attachments.is_empty() { 6 } else { 12 });
    }
    let lower = user_input.to_lowercase();
    if lower.contains("30分钟") || lower.contains("30 分钟") || lower.contains("30-minute") {
        24
    } else if !source_attachments.is_empty()
        && (lower.contains("学位") || lower.contains("论文") || lower.contains("答辩"))
    {
        18
    } else if requested_presentation(user_input) {
        12
    } else {
        0
    }
}

fn document_quality_blocking_summary(output: &str) -> Option<String> {
    let value: Value = serde_json::from_str(output).ok()?;
    if let Some(error) = json_str(&value, "error", "error").filter(|error| !error.trim().is_empty())
    {
        return Some(format!(
            "document-generation tool returned error: {}",
            error.chars().take(180).collect::<String>()
        ));
    }
    let quality = value.get("quality")?;
    let failures = json_count(quality, "failureCount", "failure_count");
    let blockers = json_count(quality, "blockerCount", "blocker_count");
    let level = json_str(quality, "qualityLevel", "quality_level")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let has_failed_check = quality
        .get("checks")
        .and_then(Value::as_array)
        .is_some_and(|checks| {
            checks.iter().any(|check| {
                json_str(check, "status", "status")
                    .map(|status| status.to_ascii_lowercase())
                    .is_some_and(|status| {
                        matches!(status.as_str(), "fail" | "failed" | "blocker" | "blocked")
                    })
            })
        });
    if failures == 0
        && blockers == 0
        && !matches!(level.as_str(), "fail" | "failed" | "blocker" | "blocked")
        && !has_failed_check
    {
        return None;
    }
    let mut parts = Vec::new();
    if blockers > 0 {
        parts.push(format!("{blockers} blocker(s)"));
    }
    if failures > 0 {
        parts.push(format!("{failures} failure(s)"));
    }
    if matches!(level.as_str(), "fail" | "failed" | "blocker" | "blocked") {
        parts.push(format!("qualityLevel={level}"));
    }
    for message in quality_failure_messages(quality) {
        parts.push(message);
    }
    Some(if parts.is_empty() {
        "quality contract failed".to_string()
    } else {
        parts.join("; ")
    })
}

fn document_quality_blocking_summary_for_request(
    output: &str,
    user_input: &str,
    source_attachments: &[AttachmentEvidence],
) -> Option<String> {
    if let Some(summary) = document_quality_blocking_summary(output) {
        return Some(summary);
    }
    let value: Value = serde_json::from_str(output).ok()?;
    let quality = value.get("quality").unwrap_or(&value);
    if requested_presentation(user_input) {
        if document_output_path(&value).is_none() && document_manifest_path(&value).is_none() {
            return Some(
                "document-generation tool did not report an output file path or manifestPath"
                    .to_string(),
            );
        }
        let minimum = minimum_expected_slide_count(user_input, source_attachments);
        let slide_count = json_count_any(quality, &["slideCount", "slide_count"])
            .or_else(|| json_count_any(&value, &["slideCount", "slide_count"]));
        if minimum > 0 {
            match slide_count {
                Some(count) if count < minimum => {
                    return Some(format!(
                        "slide count {count} is below the expected minimum {minimum}"
                    ));
                }
                None => {
                    return Some(format!(
                        "generated presentation did not report slide_count; expected at least {minimum} slide(s)"
                    ));
                }
                _ => {}
            }
        }
    }
    if !source_attachments.is_empty()
        && requested_presentation(user_input)
        && document_manifest_path(&value).is_none()
    {
        return Some(
            "source-backed document generation did not report manifestPath for verification"
                .to_string(),
        );
    }
    if !source_attachments.is_empty() && request_requires_figure_assets(user_input) {
        let image_count = json_count_sum(
            quality,
            &[
                "imageCount",
                "image_count",
                "embedded_image_count",
                "pptx_picture_count",
                "pptx_media_file_count",
                "extractedAssetCount",
                "extracted_asset_count",
                "source_extracted_figure_count",
                "asset_manifest_figure_count",
            ],
        );
        if image_count == 0 {
            return Some(
                "request required source images/figures, but generated quality reported none"
                    .to_string(),
            );
        }
    }
    if !source_attachments.is_empty() && request_requires_table_assets(user_input) {
        let table_count = json_count_sum(
            quality,
            &[
                "tableCount",
                "table_count",
                "rendered_table_count",
                "pptx_table_object_count",
                "source_extracted_table_count",
                "asset_manifest_table_count",
            ],
        );
        if table_count == 0 {
            return Some(
                "request required tables, but generated quality reported none".to_string(),
            );
        }
    }
    if !source_attachments.is_empty() && request_requires_formula_assets(user_input) {
        let formula_count = json_count_sum(
            quality,
            &[
                "formulaCount",
                "formula_count",
                "formulaImageCount",
                "formula_image_count",
                "editable_formula_count",
                "rendered_formula_count",
                "pptx_office_math_count",
                "pptx_formula_image_count",
                "source_extracted_formula_count",
                "asset_manifest_formula_count",
            ],
        );
        if formula_count == 0 {
            return Some(
                "request required formulas, but generated quality reported none".to_string(),
            );
        }
    }
    if !source_attachments.is_empty() && request_requires_chart_assets(user_input) {
        let chart_count = json_count_sum(
            quality,
            &[
                "chartCount",
                "chart_count",
                "rendered_chart_count",
                "pptx_chart_object_count",
                "source_grounded_chart_count",
            ],
        );
        if chart_count == 0 {
            return Some(
                "request required charts/plots, but generated quality reported none".to_string(),
            );
        }
    }
    None
}

fn document_output_path(value: &Value) -> Option<&str> {
    json_str(value, "filePath", "file_path")
        .or_else(|| json_str(value, "path", "path"))
        .or_else(|| json_str(value, "outputPath", "output_path"))
        .filter(|path| !path.trim().is_empty())
}

fn document_manifest_path(value: &Value) -> Option<&str> {
    json_str(value, "manifestPath", "manifest_path")
        .or_else(|| json_str(value, "assetManifestPath", "asset_manifest_path"))
        .filter(|path| !path.trim().is_empty())
}

fn json_count(value: &Value, camel: &str, snake: &str) -> usize {
    json_count_any(value, &[camel, snake]).unwrap_or(0)
}

fn json_count_any(value: &Value, keys: &[&str]) -> Option<usize> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_u64)
            .map(|count| count as usize)
    })
}

fn json_count_sum(value: &Value, keys: &[&str]) -> usize {
    keys.iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_u64))
        .map(|count| count as usize)
        .sum()
}

fn json_str<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a str> {
    value
        .get(camel)
        .or_else(|| value.get(snake))
        .and_then(Value::as_str)
}

fn quality_failure_messages(quality: &Value) -> Vec<String> {
    let mut messages = Vec::new();
    if let Some(checks) = quality.get("checks").and_then(Value::as_array) {
        for check in checks {
            let status = json_str(check, "status", "status").unwrap_or_default();
            if matches!(status, "fail" | "failed" | "blocker" | "blocked") {
                let id = json_str(check, "id", "id").unwrap_or("quality.issue");
                let message = json_str(check, "message", "message").unwrap_or("");
                messages.push(format!("{id}: {message}").chars().take(180).collect());
            }
            if messages.len() >= 3 {
                break;
            }
        }
    }
    messages
}

fn document_generation_needs_asset_preflight(
    user_input: &str,
    source_attachments: &[AttachmentEvidence],
) -> bool {
    !source_attachments.is_empty()
        && (request_requires_figure_assets(user_input)
            || request_requires_table_assets(user_input)
            || request_requires_formula_assets(user_input)
            || request_requires_chart_assets(user_input)
            || academic_source_presentation_request(user_input, source_attachments))
}

fn academic_source_presentation_request(
    user_input: &str,
    source_attachments: &[AttachmentEvidence],
) -> bool {
    if source_attachments.is_empty() || !requested_presentation(user_input) {
        return false;
    }
    let lower = user_input.to_lowercase();
    lower.contains("学位")
        || lower.contains("论文")
        || lower.contains("答辩")
        || lower.contains("学术")
        || lower.contains("thesis")
        || lower.contains("dissertation")
        || lower.contains("defense")
        || lower.contains("academic")
}

fn source_attachment_pdf_path(source_attachments: &[AttachmentEvidence]) -> Option<String> {
    source_attachments.iter().find_map(|attachment| {
        let label = attachment.label.trim();
        if label.is_empty() {
            return None;
        }
        let path = Path::new(label);
        let is_pdf = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"));
        (is_pdf && path.exists()).then(|| label.to_string())
    })
}

fn sanitize_path_component(value: &str) -> String {
    let mut out = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').chars().take(80).collect()
}

fn document_generation_asset_extract_tool_use(
    user_input: &str,
    source_attachments: &[AttachmentEvidence],
    available_tool_names: &BTreeSet<String>,
) -> Option<(String, String, String)> {
    if !document_generation_needs_asset_preflight(user_input, source_attachments) {
        return None;
    }
    let tool_name = document_asset_extract_tool_name(available_tool_names)?;
    let source_path = source_attachment_pdf_path(source_attachments)?;
    let stem = Path::new(&source_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(sanitize_path_component)
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "source".to_string());
    let academic_deck = academic_source_presentation_request(user_input, source_attachments);
    let input = serde_json::json!({
        "path": source_path,
        "format": "pdf",
        "output_dir": format!("output/extracted-assets/{stem}"),
        "scan_all": true,
        "resume": true,
        "max_runtime_seconds": 180,
        "page_batch_size": 24,
        "render_page_previews": false,
        "extract_images": academic_deck || request_requires_figure_assets(user_input) || request_requires_chart_assets(user_input),
        "extract_captioned_figures": academic_deck || request_requires_figure_assets(user_input) || request_requires_chart_assets(user_input),
        "extract_tables": academic_deck || request_requires_table_assets(user_input),
        "extract_formula_candidates": academic_deck || request_requires_formula_assets(user_input),
    })
    .to_string();
    Some((
        format!("himalaya-document-assets-{}", current_time_millis()),
        tool_name,
        input,
    ))
}

fn document_generation_text_tool_use(
    assistant_text: &str,
    user_input: &str,
    source_attachments: &[AttachmentEvidence],
    available_tool_names: &BTreeSet<String>,
    asset_manifest_path: Option<&str>,
) -> Option<(String, String, String)> {
    for (requested_name, mut input) in
        document_generation_text_tool_candidates(assistant_text, user_input, available_tool_names)
    {
        let Some(tool_name) = resolve_requested_tool_name(&requested_name, available_tool_names)
        else {
            continue;
        };
        if !is_generate_file_tool(&tool_name) {
            continue;
        }
        normalize_document_generation_tool_input(&mut input, user_input);
        if !input.is_object() {
            continue;
        }
        repair_document_generation_tool_input(
            &mut input,
            &tool_name,
            user_input,
            source_attachments,
            asset_manifest_path,
        );
        return Some((
            format!("himalaya-text-tool-{}", current_time_millis()),
            tool_name,
            input.to_string(),
        ));
    }
    None
}

fn document_generation_text_tool_candidates(
    assistant_text: &str,
    user_input: &str,
    available_tool_names: &BTreeSet<String>,
) -> Vec<(String, Value)> {
    let mut candidates = Vec::new();
    candidates.extend(xml_style_document_tool_candidates(assistant_text));
    for object_text in extract_json_objects(assistant_text) {
        let Ok(value) = serde_json::from_str::<Value>(&object_text) else {
            continue;
        };
        if let Some((name, input)) = explicit_tool_candidate_from_value(&value) {
            candidates.push((name, input));
            continue;
        }
        if document_generation_like_value(&value) {
            if let Some(name) = mentioned_document_tool_name(assistant_text, available_tool_names)
                .or_else(|| {
                    preferred_document_generation_tool_name(user_input, available_tool_names)
                })
            {
                candidates.push((name, value));
            }
        }
    }
    candidates
}

fn xml_style_document_tool_candidates(assistant_text: &str) -> Vec<(String, Value)> {
    let mut candidates = Vec::new();
    let mut remaining = assistant_text;
    while let Some(start) = remaining.find("<call:") {
        let after_call = &remaining[start + "<call:".len()..];
        let Some(end_name) = after_call.find('>') else {
            break;
        };
        let raw_name = after_call[..end_name]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches('/');
        let after_name = &after_call[end_name + 1..];
        let Some(params_start) = after_name.find("<params>") else {
            remaining = after_name;
            continue;
        };
        let after_params = &after_name[params_start + "<params>".len()..];
        let Some(params_end) = after_params.find("</params>") else {
            remaining = after_params;
            continue;
        };
        if let Some(object_text) = extract_json_object(&after_params[..params_end]) {
            if let Ok(value) = serde_json::from_str::<Value>(&object_text) {
                candidates.push((raw_name.to_string(), value));
            }
        }
        remaining = &after_params[params_end + "</params>".len()..];
    }
    candidates
}

fn explicit_tool_candidate_from_value(value: &Value) -> Option<(String, Value)> {
    let requested_name = value
        .get("name")
        .or_else(|| value.get("tool"))
        .or_else(|| value.get("tool_name"))
        .and_then(Value::as_str)?;
    let mut input = value
        .get("parameters")
        .or_else(|| value.get("arguments"))
        .or_else(|| value.get("input"))
        .cloned()
        .unwrap_or_else(|| {
            let mut fallback = value.clone();
            if let Some(object) = fallback.as_object_mut() {
                object.remove("name");
                object.remove("tool");
                object.remove("tool_name");
            }
            fallback
        });
    if let Some(raw) = input.as_str() {
        input = serde_json::from_str(raw).ok()?;
    }
    Some((requested_name.to_string(), input))
}

fn mentioned_document_tool_name(
    assistant_text: &str,
    available_tool_names: &BTreeSet<String>,
) -> Option<String> {
    let normalized_text = normalize_tool_key(assistant_text);
    available_tool_names
        .iter()
        .find(|name| {
            is_generate_file_tool(name) && normalized_text.contains(&normalize_tool_key(name))
        })
        .cloned()
}

fn document_generation_like_value(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    [
        "document_spec",
        "documentSpec",
        "slides",
        "blocks",
        "outline",
        "sheets",
        "title",
    ]
    .iter()
    .any(|key| object.contains_key(*key))
}

fn normalize_document_generation_tool_input(input: &mut Value, user_input: &str) {
    let Some(object) = input.as_object_mut() else {
        return;
    };
    if let Some(spec) = object.remove("documentSpec") {
        object.entry("document_spec".to_string()).or_insert(spec);
    }
    if let Some(spec) = object.get_mut("document_spec") {
        normalize_document_spec_value(spec);
    }
    if !object.contains_key("document_spec") {
        if let Some(outline) = object.remove("outline") {
            if !object.contains_key("slides") && !object.contains_key("blocks") {
                object.insert("slides".to_string(), outline);
            }
        }
        let spec_keys = [
            "title",
            "subtitle",
            "author",
            "language",
            "document_type",
            "documentType",
            "source_documents",
            "sourceDocuments",
            "generation_contract",
            "generationContract",
            "theme",
            "blocks",
            "slides",
            "sheets",
            "metadata",
        ];
        if spec_keys.iter().any(|key| object.contains_key(*key)) {
            let mut spec = Map::new();
            for key in spec_keys {
                if let Some(value) = object.remove(key) {
                    spec.insert(key.to_string(), value);
                }
            }
            object.insert("document_spec".to_string(), Value::Object(spec));
        }
    }
    if !object.contains_key("format") && normalize_tool_key(user_input).contains("ppt") {
        object.insert("format".to_string(), Value::String("pptx".to_string()));
    }
}

fn normalize_document_spec_value(spec: &mut Value) {
    match spec.take() {
        Value::String(raw) => {
            let trimmed = raw.trim();
            let parsed = serde_json::from_str::<Value>(trimmed)
                .or_else(|_| serde_json::from_str::<Value>(&jsonish_python_literals(trimmed)));
            *spec = match parsed {
                Ok(Value::Object(object)) => Value::Object(object),
                Ok(Value::Array(blocks)) => serde_json::json!({ "blocks": blocks }),
                _ => Value::String(raw),
            };
        }
        Value::Array(blocks) => {
            *spec = serde_json::json!({ "blocks": blocks });
        }
        other => {
            *spec = other;
        }
    }
}

fn jsonish_python_literals(value: &str) -> String {
    value
        .replace(": True", ": true")
        .replace(": False", ": false")
        .replace(": None", ": null")
        .replace(":True", ":true")
        .replace(":False", ":false")
        .replace(":None", ":null")
        .replace(", True", ", true")
        .replace(", False", ", false")
        .replace(", None", ", null")
        .replace("[True", "[true")
        .replace("[False", "[false")
        .replace("[None", "[null")
}

fn protect_document_generation_output_path(object: &mut Map<String, Value>, format: &str) {
    let Some(path) = object
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return;
    };
    let Some(repaired) = safe_document_generation_output_path(path, format) else {
        return;
    };
    object.insert("path".to_string(), Value::String(repaired));
}

fn safe_document_generation_output_path(path: &str, format: &str) -> Option<String> {
    let candidate = Path::new(path);
    let mut repaired = if candidate.is_absolute() {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|dir| dir.canonicalize().ok());
        let absolute = candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.to_path_buf());
        match cwd.and_then(|cwd| absolute.strip_prefix(cwd).ok().map(Path::to_path_buf)) {
            Some(relative) if !relative.as_os_str().is_empty() => relative,
            _ => PathBuf::from("output").join(
                candidate
                    .file_name()
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| std::ffi::OsStr::new("himalaya_document")),
            ),
        }
    } else {
        candidate.to_path_buf()
    };
    if repaired.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    }) {
        repaired = PathBuf::from("output").join(
            repaired
                .file_name()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| std::ffi::OsStr::new("himalaya_document")),
        );
    }
    if repaired
        .extension()
        .and_then(|extension| extension.to_str())
        .is_none()
    {
        repaired.set_extension(format);
    }
    let repaired = repaired.to_string_lossy().replace('\\', "/");
    (repaired != path).then_some(repaired)
}

fn normalize_document_spec_blocks(value: &mut Value) {
    let Some(blocks) = value.as_array_mut() else {
        return;
    };
    let original = std::mem::take(blocks);
    *blocks = original
        .into_iter()
        .flat_map(normalize_document_spec_block)
        .collect();
}

fn normalize_document_spec_block(mut block: Value) -> Vec<Value> {
    let Some(object) = block.as_object_mut() else {
        return vec![block];
    };
    if let Some(notes) = object.remove("speakerNotes") {
        object.entry("speaker_notes".to_string()).or_insert(notes);
    }
    if let Some(format) = object.remove("formulaFormat") {
        object.entry("formula_format".to_string()).or_insert(format);
    }
    let block_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("paragraph")
        .to_ascii_lowercase();
    match block_type.as_str() {
        "section" | "slide" => {
            let children = object
                .remove("content")
                .or_else(|| object.remove("children"))
                .or_else(|| object.remove("blocks"));
            let heading_text = object
                .remove("title")
                .or_else(|| object.remove("heading"))
                .or_else(|| object.get("text").cloned())
                .unwrap_or_else(|| Value::String("Section".to_string()));
            object.insert("type".to_string(), Value::String("heading".to_string()));
            object.insert("level".to_string(), Value::Number(1.into()));
            object.insert("text".to_string(), heading_text);
            let mut normalized = vec![block];
            if let Some(Value::Array(children)) = children {
                normalized.extend(children.into_iter().flat_map(normalize_document_spec_block));
            }
            normalized
        }
        "list" | "bullet" | "bullet_list" | "bullets_list" => {
            object.insert("type".to_string(), Value::String("bullets".to_string()));
            if !object.contains_key("items") {
                if let Some(content) = object.remove("content") {
                    object.insert("items".to_string(), coerce_document_string_list(content));
                }
            }
            vec![block]
        }
        "equation" | "math" => {
            object.insert("type".to_string(), Value::String("formula".to_string()));
            if !object.contains_key("latex") {
                if let Some(value) = object
                    .remove("formula")
                    .or_else(|| object.remove("content"))
                {
                    object.insert("latex".to_string(), value);
                }
            }
            vec![block]
        }
        "figure" | "picture" => {
            object.insert("type".to_string(), Value::String("image".to_string()));
            if !object.contains_key("path") {
                if let Some(path) = object
                    .remove("source_path")
                    .or_else(|| object.remove("src"))
                {
                    object.insert("path".to_string(), path);
                }
            }
            vec![block]
        }
        _ => vec![block],
    }
}

fn coerce_document_string_list(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| match item {
                    Value::String(text) => Value::String(text),
                    other => Value::String(other.to_string()),
                })
                .collect(),
        ),
        Value::String(text) => Value::Array(vec![Value::String(text)]),
        other => Value::Array(vec![Value::String(other.to_string())]),
    }
}

fn repair_document_generation_tool_input(
    input: &mut Value,
    tool_name: &str,
    user_input: &str,
    source_attachments: &[AttachmentEvidence],
    asset_manifest_path: Option<&str>,
) {
    let Some(object) = input.as_object_mut() else {
        return;
    };
    let format = document_generation_tool_format(user_input, tool_name);
    if !object.contains_key("path") {
        object.insert(
            "path".to_string(),
            Value::String(format!(
                "output/himalaya_document_{}.{}",
                current_time_millis(),
                format
            )),
        );
    } else {
        protect_document_generation_output_path(object, format);
    }
    if matches!(
        normalize_tool_key(tool_name).as_str(),
        "generatefile" | "functiongeneratefile"
    ) && !object.contains_key("format")
    {
        object.insert("format".to_string(), Value::String(format.to_string()));
    }
    if is_doc_service_generation_tool(tool_name) {
        object
            .entry("strict_quality".to_string())
            .or_insert(Value::Bool(true));
        if let Some(path) = asset_manifest_path.filter(|path| !path.trim().is_empty()) {
            object
                .entry("asset_manifest_path".to_string())
                .or_insert_with(|| Value::String(path.to_string()));
        }
    }

    let mut promoted_theme: Option<String> = None;
    {
        let spec_entry = object
            .entry("document_spec".to_string())
            .or_insert_with(|| serde_json::json!({}));
        if !spec_entry.is_object() {
            return;
        }
        let spec = spec_entry.as_object_mut().expect("checked object");
        if let Some(document_type) = spec.remove("documentType") {
            spec.entry("document_type".to_string())
                .or_insert(document_type);
        }
        if let Some(source_documents) = spec.remove("sourceDocuments") {
            spec.entry("source_documents".to_string())
                .or_insert(source_documents);
        }
        if let Some(contract) = spec.remove("generationContract") {
            spec.entry("generation_contract".to_string())
                .or_insert(contract);
        }
        if let Some(Value::String(theme)) = spec.get("theme").cloned() {
            if is_doc_service_generation_tool(tool_name) {
                promoted_theme = Some(theme);
                spec.remove("theme");
            }
        }
        if let Some(blocks) = spec.get_mut("blocks") {
            normalize_document_spec_blocks(blocks);
        }
        if !spec.contains_key("title") {
            spec.insert(
                "title".to_string(),
                Value::String(infer_rescue_document_title(user_input, true)),
            );
        }
        if requested_presentation(user_input) && !spec.contains_key("document_type") {
            spec.insert(
                "document_type".to_string(),
                Value::String(
                    if user_input.contains("答辩") || user_input.to_lowercase().contains("defense")
                    {
                        "degree_defense"
                    } else {
                        "academic_talk"
                    }
                    .to_string(),
                ),
            );
        }
        if !source_attachments.is_empty()
            && spec
                .get("source_documents")
                .and_then(Value::as_array)
                .is_none_or(|documents| documents.is_empty())
        {
            spec.insert(
                "source_documents".to_string(),
                Value::Array(
                    source_attachments
                        .iter()
                        .take(8)
                        .map(|attachment| {
                            serde_json::json!({
                                "document": attachment.label,
                                "paragraph": attachment.details.join("; ")
                            })
                        })
                        .collect(),
                ),
            );
        }
        let contract = spec
            .entry("generation_contract".to_string())
            .or_insert_with(|| serde_json::json!({}));
        if let Some(contract) = contract.as_object_mut() {
            contract
                .entry("strict_source_grounding".to_string())
                .or_insert(Value::Bool(!source_attachments.is_empty()));
            contract
                .entry("fail_on_contract_violation".to_string())
                .or_insert(Value::Bool(true));
            contract
                .entry("allow_degraded_output".to_string())
                .or_insert(Value::Bool(false));
            contract
                .entry("require_manifest".to_string())
                .or_insert(Value::Bool(true));
            contract
                .entry("minimum_quality_score".to_string())
                .or_insert(Value::Number(serde_json::Number::from(82)));
            if request_requires_formula_assets(user_input) {
                contract
                    .entry("require_editable_formulas".to_string())
                    .or_insert(Value::Bool(true));
            }
            if requested_presentation(user_input) {
                contract
                    .entry("expected_slide_count".to_string())
                    .or_insert(Value::Number(serde_json::Number::from(
                        minimum_expected_slide_count(user_input, source_attachments),
                    )));
            }
            if request_requires_figure_assets(user_input)
                || request_requires_table_assets(user_input)
                || request_requires_formula_assets(user_input)
                || request_requires_chart_assets(user_input)
            {
                let lower_input = user_input.to_lowercase();
                let academic_deck = requested_presentation(user_input)
                    && !source_attachments.is_empty()
                    && (lower_input.contains("学位")
                        || lower_input.contains("论文")
                        || lower_input.contains("答辩")
                        || lower_input.contains("thesis")
                        || lower_input.contains("defense"));
                let figures_min = if academic_deck { 4 } else { 1 };
                let tables_min = if academic_deck { 2 } else { 1 };
                let formulas_min = if academic_deck { 2 } else { 1 };
                contract
                    .entry("required_assets".to_string())
                    .or_insert_with(|| {
                        serde_json::json!({
                            "figures": if request_requires_figure_assets(user_input) { figures_min } else { 0 },
                            "tables": if request_requires_table_assets(user_input) { tables_min } else { 0 },
                            "formulas": if request_requires_formula_assets(user_input) { formulas_min } else { 0 },
                            "charts": if request_requires_chart_assets(user_input) { 1 } else { 0 },
                            "require_source_refs": !source_attachments.is_empty(),
                            "require_extracted_assets": !source_attachments.is_empty()
                        })
                    });
            }
        }
    }
    if let Some(theme) = promoted_theme {
        object
            .entry("theme".to_string())
            .or_insert(Value::String(theme));
    }
}

fn infer_rescue_document_title(user_input: &str, chinese: bool) -> String {
    let trimmed = user_input.trim();
    if trimmed.chars().count() <= 40 {
        return trimmed.to_string();
    }
    for (start_marker, end_marker) in [
        ("题目：", "\n"),
        ("标题：", "\n"),
        ("title:", "\n"),
        ("Title:", "\n"),
        ("论文", "，"),
        ("论文", ","),
    ] {
        if let Some(start) = trimmed.find(start_marker) {
            let after = &trimmed[start + start_marker.len()..];
            let end = after.find(end_marker).unwrap_or(after.len());
            let candidate = clean_attachment_label(&after[..end]);
            if !candidate.is_empty() && candidate.chars().count() <= 80 {
                return candidate;
            }
        }
    }
    trimmed
        .chars()
        .take(if chinese { 32 } else { 48 })
        .collect::<String>()
}

fn document_generation_failure_message(
    source_attachments: &[AttachmentEvidence],
    available_tool_names: &BTreeSet<String>,
) -> String {
    let available = available_tool_names
        .iter()
        .filter(|name| is_generate_file_tool(name))
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let attachment_hint = if source_attachments
        .iter()
        .any(|attachment| attachment.extracted_chars.unwrap_or_default() >= 120_000)
    {
        " The request includes a large attachment; use a cloud/long-context model for planning, or keep the local model on manifest/chunk-guided extraction before generating."
    } else {
        ""
    };
    format!(
        "document generation request required a successful, quality-compliant document-generation tool result, but the assistant stopped without generating an acceptable file after {DOCUMENT_GENERATION_GATE_MAX_PROMPTS} redrive prompt(s). Available document tool(s): {}.{}",
        if available.is_empty() { "none".to_string() } else { available },
        attachment_hint
    )
}

fn workspace_evidence_gate_prompt() -> &'static str {
    "Workspace analysis is not allowed to finish from the injected navigation snapshot. Use local evidence tools now: run glob_search or grep_search to discover relevant files/entry points, read root manifests with read_file, read at least three relevant source files with read_file, then answer only from those observations. Preserve the active output-language contract from the system prompt."
}

fn document_generation_gate_prompt(
    user_input: &str,
    source_attachments: &[AttachmentEvidence],
    quality_summary: Option<&str>,
) -> String {
    let quality_line = quality_summary
        .filter(|summary| !summary.trim().is_empty())
        .map(|summary| format!("\n上一轮生成结果未通过质量合同：{summary}"))
        .unwrap_or_default();
    if matches!(
        detect_input_language(user_input).as_deref(),
        Some("Chinese")
    ) {
        let attachments = gate_attachment_lines(source_attachments, "本轮已有附件/源材料");
        format!(
            "# 文档生成完成门禁\n当前任务明确要求生成 PPT/Word/PDF/Excel 等电子文档，但上一轮没有成功调用可用的文档生成工具，或生成结果未通过质量合同；不能用论文分析、摘要或大纲作为最终交付。{quality_line}{attachments}\n请继续执行：基于附件或会话中已经提取的 `[File: ...]` 内容，整理为适合目标文档的结构化 `document_spec`，调用可用的最佳文档生成工具输出实际文件（优先 doc-service，兜底 `generate_file`）；检查工具返回的 `manifestPath` 和 `quality`，可修复的问题先修复。不要要求用户再次上传附件、粘贴论文正文或提供文档内容。生成文件成功且质量合同通过之前不要给最终答复。"
        )
    } else {
        let quality_line = quality_summary
            .filter(|summary| !summary.trim().is_empty())
            .map(|summary| {
                format!("\nPrevious generated result failed the quality contract: {summary}")
            })
            .unwrap_or_default();
        let attachments = gate_attachment_lines(
            source_attachments,
            "Attached/source material already available",
        );
        format!(
            "# Document generation completion gate\nThis task explicitly requires a PPT/Word/PDF/Excel document file, but the previous attempt did not successfully call an available document-generation tool or the generated result failed the quality contract. A prose analysis, summary, or outline is not the deliverable.{quality_line}{attachments}\nContinue now: base the document on the attached source material or the already-extracted `[File: ...]` content in the conversation, organize it into a structured `document_spec`, call the best available document-generation tool to create the actual file (prefer doc-service, fall back to `generate_file`), inspect the returned `manifestPath` and `quality`, and fix any actionable issues before the final response. Do not ask the user to upload the attachment again, paste the source document, or provide document content. Do not finish until file generation succeeds and the quality contract passes."
        )
    }
}

fn document_generation_preflight_message(
    user_input: &str,
    source_attachments: &[AttachmentEvidence],
) -> String {
    let estimated_slides = estimate_requested_slide_count(user_input).unwrap_or(12);
    let attachment_count = source_attachments.len();
    if is_super_ppt_request(user_input) {
        format!(
            "SuperPPT preflight: {attachment_count} attachment/source item(s); estimated {estimated_slides} slides; expected imagegen calls at least {} (phase A) + {} (phase B). Verify imagegen availability before generation and write resumable manifests.",
            estimated_slides,
            estimated_slides * 3
        )
    } else {
        format!(
            "Document generation preflight: {attachment_count} attachment/source item(s); estimated {estimated_slides} deck slide(s) when PPT is requested; use the best available document-generation tool and pass quality checks before completion."
        )
    }
}

fn document_generation_long_task_prompt(
    user_input: &str,
    source_attachments: &[AttachmentEvidence],
    language: Option<&str>,
) -> String {
    let estimated_slides = estimate_requested_slide_count(user_input).unwrap_or(12);
    let super_ppt = is_super_ppt_request(user_input);
    let attachment_lines = if source_attachments.is_empty() {
        "- Source attachments: none detected in conversation markers.".to_string()
    } else {
        format!(
            "- Source attachments: {}",
            source_attachments
                .iter()
                .take(8)
                .map(AttachmentEvidence::summary)
                .collect::<Vec<_>>()
                .join("; ")
        )
    };
    if matches!(language, Some("Chinese")) {
        if super_ppt {
            format!(
                "# 长任务执行契约（SuperPPT 强制）\n- 这是长时 PPT 生成任务，必须按阶段推进并持续产出中间文件，不能只输出大纲或要求用户再次提供附件。\n{attachment_lines}\n- 预估页数：{estimated_slides} 页；阶段 1 至少 {estimated_slides} 次 imagegen；阶段 2 至少 {} 次背景/骨架/图标提取类 imagegen。\n- 启动前先做预检：确认 imagegen 可用、输出目录可写、附件 brief/manifest/fullTextPath 可用；若 imagegen 不可用，明确阻塞并停止，不能用代码绘图兜底。\n- 对超长附件，优先使用附件 brief 和 manifest；需要细节时读取 fullTextPath 或 chunk index，不要要求用户粘贴正文。\n- 必须写入可恢复产物清单：outline、prompts、imagegen manifest、slides、editable deck manifest、最终 pptx 路径。"
            , estimated_slides * 3)
        } else {
            format!(
                "# 长任务执行契约（文档生成）\n- 这是附件驱动的电子文档生成任务，最终交付必须是实际文件，不是摘要、策划案或逐页讲稿文本。\n{attachment_lines}\n- 对超长附件，优先使用附件 brief 和 manifest；需要细节时读取 fullTextPath 或 chunk index，不要要求用户再次上传或粘贴正文。\n- 如果当前模型上下文较小或是本地模型，不要尝试一次性理解整篇论文；先建立章节索引、图表索引、公式索引和实验结果索引，再按索引分块读取关键内容。\n- 严格基于论文来源：不要臆造实验数据、图号、表号或公式；缺少证据时先读附件/manifest/源文件。\n- 生成前先整理结构化 document_spec；优先使用可用的 doc-service 文档工具，兜底使用 generate_file。生成后检查 manifestPath 和 quality。若质量合同失败，必须修复并重新生成，不能作为最终交付。"
            )
        }
    } else if super_ppt {
        format!(
            "# Long-Running Task Contract (SuperPPT Mandatory)\n- This is a long-running deck generation task. Work stage by stage, persist intermediate files, and do not replace the deliverable with an outline or ask the user to resend attachments.\n{attachment_lines}\n- Estimated slides: {estimated_slides}; phase A needs at least {estimated_slides} imagegen calls; phase B needs at least {} extraction imagegen calls.\n- Preflight before starting: confirm imagegen availability, writable output directories, and attachment brief/manifest/fullTextPath availability. If imagegen is unavailable, stop with a blocker; do not fall back to code-drawn slides.\n- For long attachments, use the brief and manifest first; read fullTextPath or chunk index for details instead of asking the user to paste the source.\n- Persist resumable artifacts: outline, prompts, imagegen manifest, slides, editable deck manifest, and final pptx paths.",
            estimated_slides * 3
        )
    } else {
        format!(
            "# Long-Running Task Contract (Document Generation)\n- This attachment-backed document request must end with a real generated file, not a summary, outline, or slide-by-slide script text.\n{attachment_lines}\n- For long attachments, use the brief and manifest first; read fullTextPath or chunk index for details instead of asking the user to upload or paste the source again.\n- If the active model has a small context window or is local, do not try to understand the whole paper in one pass; first build section, figure, table, formula, and experiment indexes, then read key chunks by index.\n- Stay source-grounded: do not invent experiment data, figure/table numbers, or equations; read the attachment/manifest/source file when evidence is missing.\n- Build a structured document_spec before generation; prefer available doc-service document tools and fall back to generate_file. After generation inspect manifestPath and quality. If the quality contract fails, repair and regenerate instead of treating the file as final."
        )
    }
}

fn is_super_ppt_request(user_input: &str) -> bool {
    user_input.contains("GordenSuperPPTSkill")
        || user_input.contains("GordenImagePPTGen")
        || user_input.contains("GordenImage2PPTX")
        || user_input.contains("imagegen-manifest")
}

fn super_ppt_manifest_text(lower_text: &str) -> bool {
    lower_text.contains("imagegen-manifest")
        || lower_text.contains("imagegen_manifest")
        || lower_text.contains("imagegen manifest")
        || lower_text.contains("imagegen-assets-manifest")
        || lower_text.contains("imagegen_assets_manifest")
        || lower_text.contains("imagegen assets manifest")
        || lower_text.contains("editable deck manifest")
        || lower_text.contains("deck manifest")
        || lower_text.contains("manifestpath")
        || (lower_text.contains("imagegen") && lower_text.contains("manifest"))
}

fn super_ppt_slide_text(lower_text: &str) -> bool {
    lower_text.contains("slides/")
        || lower_text.contains("/slides/")
        || lower_text.contains("slides\\")
        || lower_text.contains("\\slides\\")
        || ((lower_text.contains(".png") || lower_text.contains(".jpg"))
            && (lower_text.contains("slide") || lower_text.contains("幻灯片")))
}

fn super_ppt_blocker_text(text: &str) -> bool {
    let lower = text.to_lowercase();
    let mentions_super_ppt = lower.contains("superppt")
        || lower.contains("gordensuperpptskill")
        || lower.contains("gordenimagepptgen")
        || lower.contains("gordenimage2pptx")
        || lower.contains("imagegen");
    let reports_blocker = [
        "阻塞",
        "不可用",
        "缺少",
        "无法",
        "未安装",
        "无权限",
        "超时",
        "失败",
        "blocked",
        "unavailable",
        "missing",
        "not available",
        "not installed",
        "cannot access",
        "cannot use",
        "can't access",
        "hard gate",
        "failed",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    mentions_super_ppt && reports_blocker
}

fn super_ppt_completion_gate_prompt(
    user_input: &str,
    source_attachments: &[AttachmentEvidence],
) -> String {
    if matches!(
        detect_input_language(user_input).as_deref(),
        Some("Chinese")
    ) {
        let attachments = gate_attachment_lines(source_attachments, "本轮已有附件/源材料");
        format!(
            "# SuperPPT 完成门禁\n当前任务要求通过 SuperPPT/imagegen 流程生成 PPT，但上一轮没有工具调用，也没有看到 `.pptx` + `imagegen` manifest/slides 产物证据；进度说明不能作为最终交付。{attachments}\n请继续执行：先确认 imagegen 可用；若可用，按 GordenSuperPPTSkill 流程生成图片型 PPT、可编辑 PPTX、`imagegen-manifest.json`/相关 manifest 和 slides 图片，并在最终答复中给出实际路径。若 imagegen 或必需技能硬性不可用，请明确说明“阻塞”及原因。不要要求用户重新上传附件或粘贴论文正文。"
        )
    } else {
        let attachments = gate_attachment_lines(
            source_attachments,
            "Attached/source material already available",
        );
        format!(
            "# SuperPPT completion gate\nThis task requires the SuperPPT/imagegen pipeline, but the previous attempt made no tool call and provided no `.pptx` plus imagegen manifest/slides artifact evidence. Progress prose is not the deliverable.{attachments}\nContinue now: verify imagegen availability; if available, run the GordenSuperPPTSkill pipeline and produce the image deck, editable PPTX, `imagegen-manifest.json`/related manifests, and slide images, then final-answer with the actual paths. If imagegen or a required skill is hard-blocked, explicitly report the blocker and reason. Do not ask the user to re-upload the attachment or paste the source text."
        )
    }
}

fn estimate_requested_slide_count(user_input: &str) -> Option<usize> {
    for marker in ["页", "slides", "slide"] {
        if let Some(count) = number_before_marker(user_input, marker) {
            if (1..=80).contains(&count) {
                return Some(count);
            }
        }
    }
    None
}

fn number_before_marker(input: &str, marker: &str) -> Option<usize> {
    let marker_start = input.find(marker)?;
    let before = &input[..marker_start];
    let digits = before
        .chars()
        .rev()
        .skip_while(|ch| ch.is_whitespace())
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    digits.parse().ok()
}

fn gate_attachment_lines(source_attachments: &[AttachmentEvidence], heading: &str) -> String {
    if source_attachments.is_empty() {
        return String::new();
    }
    let items = source_attachments
        .iter()
        .take(8)
        .map(|attachment| format!("\n- {}", attachment.summary()))
        .collect::<String>();
    format!("\n{heading}:{items}")
}

fn format_attachment_context_prompt(
    source_attachments: &[AttachmentEvidence],
    language: Option<&str>,
) -> Option<String> {
    if source_attachments.is_empty() {
        return None;
    }
    let items = source_attachments
        .iter()
        .take(8)
        .map(|attachment| format!("\n- {}", attachment.summary()))
        .collect::<String>();
    if matches!(language, Some("Chinese")) {
        Some(format!(
            "# 附件上下文（强制）\n用户已经为当前任务提供了以下附件/源材料：{items}\n必须基于这些附件或会话中已经提取的 `[File: ...]` 内容完成任务。不要要求用户再次上传附件、粘贴论文正文或补充文档内容；如果需要更细节，优先使用会话里的附件内容或可用的文件读取工具。"
        ))
    } else {
        Some(format!(
            "# Attachment Context (Mandatory)\nThe user has already provided the following attachment/source material for this task:{items}\nBase the work on these attachments or the already-extracted `[File: ...]` content in the conversation. Do not ask the user to upload the attachment again, paste the document, or provide its contents; if more detail is needed, use the conversation's attachment content or available file-reading tools first."
        ))
    }
}

fn collect_attachment_evidence_from_messages(
    messages: &[ConversationMessage],
) -> Vec<AttachmentEvidence> {
    let mut evidence = messages
        .iter()
        .flat_map(|message| collect_attachment_evidence_from_blocks(&message.blocks))
        .collect::<Vec<_>>();
    evidence.sort_by(|left, right| left.label.cmp(&right.label).then(left.kind.cmp(right.kind)));
    evidence.dedup_by(|left, right| left.label == right.label && left.kind == right.kind);
    evidence
}

fn collect_recent_workspace_attachment_manifests(
    workspace_root: Option<&Path>,
    limit: usize,
) -> Vec<AttachmentEvidence> {
    let Some(root) = workspace_root else {
        return Vec::new();
    };
    let attachments_dir = root.join(".Himalayad").join("attachments");
    let Ok(entries) = fs::read_dir(&attachments_dir) else {
        return Vec::new();
    };

    let mut manifests = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let is_manifest = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".manifest.json"));
            if !is_manifest {
                return None;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_secs());
            Some((modified, path))
        })
        .collect::<Vec<_>>();
    manifests.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));

    manifests
        .into_iter()
        .take(limit)
        .filter_map(|(_, path)| attachment_evidence_from_manifest(&path))
        .collect()
}

fn collect_attachment_evidence_from_blocks(blocks: &[ContentBlock]) -> Vec<AttachmentEvidence> {
    blocks
        .iter()
        .flat_map(|block| match block {
            ContentBlock::Text { text } => extract_file_attachment_markers(text),
            ContentBlock::Image { media_type, .. } => {
                vec![AttachmentEvidence::image(format!(
                    "image attachment ({media_type})"
                ))]
            }
            ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. } => Vec::new(),
        })
        .collect()
}

fn extract_file_attachment_markers(text: &str) -> Vec<AttachmentEvidence> {
    let mut evidence = Vec::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("[File: ") {
        let after_marker = &remaining[start + "[File: ".len()..];
        let Some(end) = after_marker.find(']') else {
            break;
        };
        let label = after_marker[..end].trim();
        if !label.is_empty() {
            let after_label = &after_marker[end + 1..];
            let body = after_label.strip_prefix('\n').unwrap_or(after_label);
            let body_end = body.find("\n[File: ").unwrap_or(body.len());
            let extracted_chars = body[..body_end].trim().chars().count();
            evidence.push(AttachmentEvidence::text(
                label.to_string(),
                (extracted_chars > 0).then_some(extracted_chars),
            ));
        }
        remaining = &after_marker[end + 1..];
    }
    evidence.extend(extract_compacted_attachment_markers(text));
    evidence.extend(extract_attachment_warning_markers(text));
    evidence
}

fn extract_compacted_attachment_markers(text: &str) -> Vec<AttachmentEvidence> {
    const MARKER: &str = "User-provided attachments before compaction:";
    text.lines()
        .filter_map(|line| line.split_once(MARKER).map(|(_, rest)| rest))
        .flat_map(|rest| rest.split(','))
        .filter_map(|raw| {
            let label = clean_attachment_label(raw);
            if label.is_empty() || label.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(AttachmentEvidence::text(label, None))
            }
        })
        .collect()
}

fn extract_attachment_warning_markers(text: &str) -> Vec<AttachmentEvidence> {
    const PREFIX: &str = "Attachment warning: extracted ";
    const MIDDLE: &str = " characters from ";
    let mut evidence = Vec::new();
    let mut remaining = text;
    while let Some(start) = remaining.find(PREFIX) {
        let after_prefix = &remaining[start + PREFIX.len()..];
        let Some((digits, after_digits)) = after_prefix.split_once(MIDDLE) else {
            break;
        };
        let extracted_chars = digits.trim().parse::<usize>().ok();
        let label_end = after_digits
            .find(|ch| ch == ';' || ch == '\n' || ch == '\r')
            .unwrap_or(after_digits.len());
        let label = clean_attachment_label(&after_digits[..label_end]);
        if !label.is_empty() {
            evidence.push(AttachmentEvidence::text(label, extracted_chars));
        }
        remaining = &after_digits[label_end..];
    }
    evidence
}

fn clean_attachment_label(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("- ")
        .trim()
        .trim_matches(|ch: char| matches!(ch, '.' | '。' | ';' | '；' | '"' | '\''))
        .trim()
        .to_string()
}

#[derive(Debug, Deserialize)]
struct AttachmentManifestProbe {
    #[serde(rename = "sourceName")]
    source_name: Option<String>,
    #[serde(rename = "sourcePath")]
    source_path: Option<String>,
    #[serde(rename = "fullTextPath")]
    full_text_path: Option<String>,
    #[serde(rename = "extractedChars")]
    extracted_chars: Option<usize>,
    #[serde(rename = "chunkCount")]
    chunk_count: Option<usize>,
}

fn attachment_evidence_from_manifest(path: &Path) -> Option<AttachmentEvidence> {
    let content = fs::read_to_string(path).ok()?;
    let manifest = serde_json::from_str::<AttachmentManifestProbe>(&content).ok()?;
    let label = manifest
        .source_path
        .as_deref()
        .or(manifest.source_name.as_deref())
        .or_else(|| path.to_str())
        .map(clean_attachment_label)?;
    if label.is_empty() {
        return None;
    }
    let mut evidence = AttachmentEvidence::text(label, manifest.extracted_chars);
    if let Some(source_name) = manifest
        .source_name
        .filter(|value| !value.trim().is_empty())
    {
        evidence = evidence.with_detail(format!("sourceName: {source_name}"));
    }
    if let Some(full_text_path) = manifest
        .full_text_path
        .filter(|value| !value.trim().is_empty())
    {
        evidence = evidence.with_detail(format!("fullTextPath: {full_text_path}"));
    }
    if let Some(chunk_count) = manifest.chunk_count {
        evidence = evidence.with_detail(format!("chunks: {chunk_count}"));
    }
    evidence = evidence.with_detail(format!("manifestPath: {}", path.display()));
    Some(evidence)
}

/// Build the user-facing guidance injected when a turn re-drives after a failed
/// verification. It states the failure, the recovery actions already attempted,
/// and the remaining budget so the model fixes the root cause rather than
/// repeating the same work.
fn format_recovery_redrive_guidance(
    attempt: usize,
    max_attempts: usize,
    reason: &str,
    action_plan: &crate::RecoveryActionPlan,
) -> String {
    let mut guidance = String::new();
    guidance.push_str("# Verification failed — automated fix attempt ");
    guidance.push_str(&attempt.to_string());
    guidance.push_str(" of ");
    guidance.push_str(&max_attempts.to_string());
    guidance.push_str("\nThe previous attempt did not pass verification:\n- Reason: ");
    guidance.push_str(reason);
    if !action_plan.actions.is_empty() {
        guidance.push_str("\n\nRecovery analysis suggested these actions:");
        for action in &action_plan.actions {
            guidance.push_str(&format!("\n- [{:?}] {}", action.scenario, action.message));
        }
    }
    guidance.push_str(
        "\n\nDiagnose the root cause from the evidence above, make the necessary changes with the available tools, and ensure the acceptance criteria will pass. Do not repeat work that already succeeded; focus on what made verification fail.",
    );
    guidance
}

/// Render a validated structured plan as an advisory system-prompt section so
/// the model is aware of the intended DAG of steps and their dependencies.
fn format_structured_plan_prompt(
    validated: &crate::structured_execution::ValidatedStructuredPlan,
) -> String {
    let mut out = String::from(
        "# Structured execution plan\nThis task was decomposed into a dependency graph. Work through the steps respecting their dependencies; satisfy each step's acceptance criteria before moving on.\n",
    );
    for step in &validated.plan.steps {
        let deps = validated
            .dependencies
            .iter()
            .filter(|(_, to)| to == &step.id)
            .map(|(from, _)| from.as_str())
            .collect::<Vec<_>>();
        let deps_label = if deps.is_empty() {
            "none".to_string()
        } else {
            deps.join(", ")
        };
        out.push_str(&format!(
            "- {} ({}): depends on [{}]\n",
            step.id, step.title, deps_label
        ));
    }
    out
}

/// Maximum characters of rendered transcript to send to the summarizer model.
/// Bounds the cost/latency of compaction summarization on very large windows.
const MODEL_SUMMARY_MAX_TRANSCRIPT_CHARS: usize = 24_000;

/// Ask the model (via the Summarization route) to summarize the messages being
/// compacted away. Returns `Some(summary_text)` on success, or `None` on any
/// failure so the caller falls back to the deterministic heuristic summary.
fn model_summarize_removed<C: ApiClient>(
    api_client: &mut C,
    route: &ModelRouteDecision,
    removed: &[ConversationMessage],
) -> Option<String> {
    if removed.is_empty() {
        return None;
    }
    let transcript = render_messages_for_summary(removed);
    if transcript.trim().is_empty() {
        return None;
    }
    let request = ApiRequest {
        system_prompt: vec![SUMMARIZATION_SYSTEM_PROMPT.to_string()],
        messages: vec![ConversationMessage::user_text(format!(
            "Summarize the following earlier conversation segment so work can continue without it. Capture the objective, decisions, key files, tool results, and pending next steps.\n\n{transcript}"
        ))],
        model_route: Some(route.clone()),
    };
    let events = api_client.stream(request).ok()?;
    let mut summary = String::new();
    for event in events {
        if let AssistantEvent::TextDelta(delta) = event {
            summary.push_str(&delta);
        }
    }
    let summary = summary.trim();
    if summary.is_empty() {
        None
    } else {
        Some(summary.to_string())
    }
}

const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are compacting a long coding-agent conversation. Produce a concise, factual summary that preserves: the active objective, decisions made, files and symbols touched, important tool results (successes and failures), and concrete pending next steps. Do not invent details. Output only the summary text.";

const PLANNING_SYSTEM_PROMPT: &str = "You are the planning model for a coding agent. Given a complex task and a heuristic pre-plan, produce a short, concrete, ordered execution plan (3-7 steps). Be specific to the task, call out risks and dependencies, and recommend an order. Do not write code or take actions; output only the plan as a short markdown list.";

const STRUCTURED_PLAN_SYSTEM_PROMPT: &str = "You are the planning model for a coding agent. Decompose the task into a small DAG of execution steps and return STRICT JSON only (no markdown, no prose). Each step needs a unique kebab-case id and a title; depends_on lists ids of steps that must finish first; parallelizable marks steps that can run alongside siblings; acceptance lists shell commands that must pass for the step to be considered done. Keep it to 2-7 steps.";

const STRUCTURED_NODE_SYSTEM_PROMPT: &str = "You are executing one step of a larger structured plan for a coding agent. Focus only on the named step and its goal; do not attempt the whole task. Produce a concise, concrete result for this step that downstream steps can build on.";

/// Extract the first balanced top-level JSON object from text that may include
/// prose or ```json code fences around it. Returns `None` if no `{...}` span is
/// found. Used to recover a structured plan from a chatty model response.
fn extract_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=offset].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_json_objects(text: &str) -> Vec<String> {
    let mut objects = Vec::new();
    let mut search_from = 0usize;
    while search_from < text.len() {
        let Some(relative_start) = text[search_from..].find('{') else {
            break;
        };
        let start = search_from + relative_start;
        let bytes = text.as_bytes();
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut found_end = None;
        for (offset, &byte) in bytes.iter().enumerate().skip(start) {
            if in_string {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                }
                continue;
            }
            match byte {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        found_end = Some(offset);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(end) = found_end {
            objects.push(text[start..=end].to_string());
            search_from = end + 1;
        } else {
            break;
        }
    }
    objects
}

/// Render messages into a compact, role-tagged transcript for summarization,
/// truncating to a bounded length to keep summarization cheap.
fn render_messages_for_summary(messages: &[ConversationMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        for block in &message.blocks {
            let rendered = match block {
                ContentBlock::Text { text } => text.clone(),
                ContentBlock::ToolUse { name, input, .. } => {
                    format!("[tool-use {name}] {input}")
                }
                ContentBlock::ToolResult {
                    tool_name,
                    output,
                    is_error,
                    ..
                } => format!(
                    "[tool-result {tool_name}{}] {output}",
                    if *is_error { " error" } else { "" }
                ),
                ContentBlock::Image { .. } => "[image]".to_string(),
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => continue,
            };
            if rendered.trim().is_empty() {
                continue;
            }
            out.push_str(role);
            out.push_str(": ");
            out.push_str(rendered.trim());
            out.push('\n');
            if out.len() >= MODEL_SUMMARY_MAX_TRANSCRIPT_CHARS {
                out.push_str("… [transcript truncated for summarization]\n");
                return out;
            }
        }
    }
    out
}

fn extract_json_string_field(input: &str, field: &str) -> Option<String> {
    serde_json::from_str::<Value>(input).ok().and_then(|value| {
        value
            .get(field)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UserMemoryFact {
    kind: MemoryKind,
    topic: &'static str,
    note: String,
}

fn clean_memory_value(value: &str) -> String {
    value
        .trim_matches(|ch: char| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    ',' | '.'
                        | ';'
                        | ':'
                        | '!'
                        | '?'
                        | '，'
                        | '。'
                        | '；'
                        | '：'
                        | '！'
                        | '？'
                        | '"'
                        | '\''
                        | '`'
                        | '“'
                        | '”'
                        | '‘'
                        | '’'
                )
        })
        .trim()
        .to_string()
}

fn take_until_delimiter(value: &str) -> String {
    let lower = value.to_lowercase();
    let mut end = value.len();
    for delimiter in [
        " and ",
        " but ",
        " from now on",
        " going forward",
        "以后",
        "以后用",
        "以后请",
    ] {
        if let Some(index) = lower.find(delimiter) {
            end = end.min(index);
        }
    }
    if let Some((index, _)) = value.char_indices().find(|(_, ch)| {
        matches!(
            ch,
            ',' | '.' | ';' | '!' | '?' | '，' | '。' | '；' | '！' | '？' | '\n' | '\r'
        )
    }) {
        end = end.min(index);
    }
    clean_memory_value(&value[..end])
}

fn extract_after_any(text: &str, lower_text: &str, markers: &[&str]) -> Option<String> {
    markers.iter().find_map(|marker| {
        lower_text.find(marker).and_then(|index| {
            let start = index + marker.len();
            let value = take_until_delimiter(&text[start..]);
            (!value.is_empty() && value.chars().count() <= 64).then_some(value)
        })
    })
}

fn detect_language_preference(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    if [
        "以后用中文",
        "请用中文",
        "用中文回答",
        "中文交流",
        "中文回复",
        "preferred response language: chinese",
        "output language: chinese",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || [
            "respond in chinese",
            "respond to the user in chinese",
            "answer in chinese",
            "use chinese",
            "speak chinese",
            "reply in chinese",
            "write in chinese",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        Some("Chinese")
    } else if [
        "以后用英文",
        "请用英文",
        "用英文回答",
        "英文交流",
        "英文回复",
        "preferred response language: english",
        "output language: english",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || [
            "respond in english",
            "respond to the user in english",
            "answer in english",
            "use english",
            "speak english",
            "reply in english",
            "write in english",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        Some("English")
    } else {
        None
    }
}

pub fn extract_user_memory_facts(text: &str) -> Vec<(MemoryKind, String, String)> {
    let lower = text.to_lowercase();
    let mut facts = Vec::<UserMemoryFact>::new();

    if let Some(name) = extract_after_any(
        text,
        &lower,
        &["我叫", "我的名字是", "叫我", "my name is ", "call me "],
    ) {
        facts.push(UserMemoryFact {
            kind: MemoryKind::UserIdentity,
            topic: "user_identity",
            note: name,
        });
    }

    if let Some(name) = extract_after_any(
        text,
        &lower,
        &[
            "以后你叫",
            "你叫",
            "你的名字是",
            "your name is ",
            "i will call you ",
            "i'll call you ",
        ],
    ) {
        facts.push(UserMemoryFact {
            kind: MemoryKind::AssistantIdentity,
            topic: "assistant_identity",
            note: name,
        });
    }

    if let Some(language) = detect_language_preference(text) {
        facts.push(UserMemoryFact {
            kind: MemoryKind::LanguagePreference,
            topic: "language_preference",
            note: language.to_string(),
        });
    }

    if let Some(preference) = extract_after_any(
        text,
        &lower,
        &["我喜欢", "我偏好", "我希望", "i prefer ", "i like "],
    ) {
        facts.push(UserMemoryFact {
            kind: MemoryKind::UserPreference,
            topic: "user_preference",
            note: preference,
        });
    }

    let mut seen = BTreeSet::new();
    facts
        .into_iter()
        .filter(|fact| seen.insert((fact.kind, fact.topic, fact.note.clone())))
        .map(|fact| (fact.kind, fact.topic.to_string(), fact.note))
        .collect()
}

fn record_user_memory(memory: &mut LongTermMemory, facts: &[(MemoryKind, String, String)]) {
    for (kind, topic, note) in facts {
        memory.add_typed_entry(*kind, topic.clone(), note.clone(), 0.98);
    }
}

fn task_packet_from_input(input: &str) -> Option<TaskPacket> {
    let trimmed = input.trim();
    serde_json::from_str::<TaskPacket>(trimmed)
        .ok()
        .or_else(|| {
            let start = trimmed.find('{')?;
            let end = trimmed.rfind('}')?;
            if end <= start {
                return None;
            }
            serde_json::from_str::<TaskPacket>(&trimmed[start..=end]).ok()
        })
}

fn user_requests_current_workspace_analysis(text: &str) -> bool {
    let lower = text.to_lowercase();
    let has_current_scope = [
        "当前工程",
        "当前项目",
        "当前工作目录",
        "当前vscode",
        "当前 vscode",
        "当前workspace",
        "当前 workspace",
        "本工程",
        "本项目",
        "current project",
        "current repo",
        "current repository",
        "current workspace",
        "current working directory",
        "this project",
        "this repo",
        "this repository",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    let has_code_analysis = [
        "源代码",
        "代码",
        "目录结构",
        "源码目录",
        "功能模块",
        "source code",
        "source tree",
        "codebase",
        "directory structure",
        "project structure",
        "module",
        "architecture",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    has_current_scope && has_code_analysis
}

fn should_skip_workspace_entry(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "target"
            | "node_modules"
            | "dist"
            | "build"
            | "out"
            | ".Himalaya"
            | ".claude"
            | ".vscode-test"
            | "coverage"
    )
}

fn collect_workspace_tree(
    root: &Path,
    dir: &Path,
    depth: usize,
    entries: &mut Vec<String>,
    truncated: &mut bool,
) {
    if depth > WORKSPACE_CONTEXT_MAX_DEPTH || entries.len() >= WORKSPACE_CONTEXT_MAX_ENTRIES {
        *truncated = true;
        return;
    }

    let mut children = match fs::read_dir(dir) {
        Ok(children) => children
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| !should_skip_workspace_entry(name))
            })
            .collect::<Vec<_>>(),
        Err(_) => return,
    };
    children.sort_by_key(|entry| entry.path());

    for child in children {
        if entries.len() >= WORKSPACE_CONTEXT_MAX_ENTRIES {
            *truncated = true;
            return;
        }
        let path = child.path();
        let is_dir = path.is_dir();
        let relative = path.strip_prefix(root).unwrap_or(path.as_path());
        let indent = "  ".repeat(depth);
        entries.push(format!(
            "{indent}{}{}",
            relative.display(),
            if is_dir { "/" } else { "" }
        ));
        if is_dir {
            collect_workspace_tree(root, &path, depth + 1, entries, truncated);
        }
    }
}

fn manifest_candidates(root: &Path) -> Vec<PathBuf> {
    [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "settings.gradle",
        "README.md",
        "Himalaya.md",
    ]
    .into_iter()
    .map(|path| root.join(path))
    .filter(|path| path.is_file())
    .collect()
}

fn is_source_candidate(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "rs" | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "py"
                    | "go"
                    | "java"
                    | "kt"
                    | "swift"
                    | "c"
                    | "cc"
                    | "cpp"
                    | "h"
                    | "hpp"
            )
        })
}

fn source_priority(relative: &Path) -> usize {
    let text = relative.to_string_lossy().to_ascii_lowercase();
    let mut score = 10_000usize;
    for marker in [
        "src/main.",
        "src/lib.",
        "src/extension.",
        "src/chatpanel.",
        "src/conversation.",
        "src/prompt.",
        "src/session.",
        "src/cli.",
        "main.",
        "lib.",
        "index.",
    ] {
        if text.contains(marker) {
            score = score.min(100);
        }
    }
    if text.contains("test") || text.contains("spec") {
        score += 500;
    }
    score + text.matches('/').count() * 10 + text.len()
}

fn collect_source_candidates(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) {
    if files.len() >= WORKSPACE_CONTEXT_MAX_ENTRIES {
        return;
    }
    let Ok(children) = fs::read_dir(dir) else {
        return;
    };
    for child in children.filter_map(Result::ok) {
        let path = child.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if should_skip_workspace_entry(name) {
            continue;
        }
        if path.is_dir() {
            collect_source_candidates(root, &path, files);
        } else if is_source_candidate(&path) {
            files.push(
                path.strip_prefix(root)
                    .unwrap_or(path.as_path())
                    .to_path_buf(),
            );
        }
    }
}

fn workspace_analysis_context(root: &Path) -> Option<String> {
    if !root.is_dir() {
        return None;
    }

    let mut entries = Vec::new();
    let mut truncated = false;
    collect_workspace_tree(root, root, 0, &mut entries, &mut truncated);
    let mut sections = vec![format!(
        "[Current workspace context]\nWorkspace root: {}\n\nDirectory tree snapshot:",
        root.display()
    )];
    if entries.is_empty() {
        sections.push("(no readable entries found)".to_string());
    } else {
        sections.push(entries.join("\n"));
        if truncated {
            sections.push("... directory snapshot truncated ...".to_string());
        }
    }

    let manifests = manifest_candidates(root)
        .into_iter()
        .filter_map(|path| {
            path.strip_prefix(root)
                .ok()
                .map(|relative| relative.display().to_string())
        })
        .collect::<Vec<_>>();
    if !manifests.is_empty() {
        sections.push(format!(
            "\nManifest candidates to read with read_file: {}",
            manifests.join(", ")
        ));
    }

    let mut source_candidates = Vec::new();
    collect_source_candidates(root, root, &mut source_candidates);
    source_candidates.sort_by_key(|relative| source_priority(relative));
    source_candidates.dedup();
    let source_preview = source_candidates
        .into_iter()
        .take(WORKSPACE_CONTEXT_MAX_SOURCE_FILES)
        .map(|relative| relative.display().to_string())
        .collect::<Vec<_>>();
    if !source_preview.is_empty() {
        sections.push(format!(
            "\nSource candidates to inspect with read_file after search/glob discovery: {}",
            source_preview.join(", ")
        ));
    }

    sections.push(
        "\nWorkspace-analysis execution contract: the directory tree above is only navigation aid, not analysis evidence. Before giving a final architecture or modification recommendation, use available local file tools to inspect this workspace. Minimum evidence: glob_search or grep_search results, root manifests read with read_file, at least three relevant source files read with read_file, and at least one content search for major entry points or module names. Preserve the active output-language contract from the system prompt. If tools are unavailable, explicitly say the conclusion is based only on the navigation snapshot. Do not ask the user to upload code or provide a repository link for this request."
            .to_string(),
    );
    Some(sections.join("\n"))
}

fn is_interesting_file_candidate(candidate: &str) -> bool {
    Path::new(candidate)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            [
                "rs", "ts", "tsx", "js", "jsx", "json", "toml", "yaml", "yml", "md", "txt", "csv",
                "pdf", "docx", "pptx", "xlsx", "xls",
            ]
            .iter()
            .any(|expected| extension.eq_ignore_ascii_case(expected))
        })
}

fn extract_file_candidates(content: &str) -> Vec<String> {
    content
        .split_whitespace()
        .filter_map(|token| {
            let candidate = token.trim_matches(|ch: char| {
                matches!(
                    ch,
                    ',' | '.' | ':' | ';' | ')' | '(' | ']' | '[' | '}' | '{' | '"' | '\'' | '`'
                )
            });
            (candidate.contains('/') && is_interesting_file_candidate(candidate))
                .then_some(candidate.to_string())
        })
        .collect()
}

fn is_workspace_evidence_tool(tool_name: &str) -> bool {
    let normalized = normalize_tool_key(tool_name);
    normalized.contains("read")
        || normalized.contains("grep")
        || normalized.contains("glob")
        || normalized.contains("search")
        || normalized.contains("find")
}

fn normalize_tool_key(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn tool_alias_key(normalized_key: &str) -> Option<&'static str> {
    match normalized_key {
        "functionwebsearch" | "functiongooglesearch" | "googlesearch" | "searchweb" => {
            Some("websearch")
        }
        "functionwebfetch" | "fetchurl" | "urlfetch" => Some("webfetch"),
        "read" | "readfile" | "functionreadfile" => Some("readfile"),
        "write" | "writefile" | "functionwritefile" => Some("writefile"),
        "generate" | "generatefile" | "functiongeneratefile" => Some("generatefile"),
        "generatepptx" | "mcpdocservicegeneratepptx" => Some("mcpdocservicegeneratepptx"),
        "generatedocx" | "mcpdocservicegeneratedocx" => Some("mcpdocservicegeneratedocx"),
        "generatepdf" | "mcpdocservicegeneratepdf" => Some("mcpdocservicegeneratepdf"),
        "generatexlsx" | "mcpdocservicegeneratexlsx" => Some("mcpdocservicegeneratexlsx"),
        "extractdocumentassets" | "mcpdocserviceextractdocumentassets" => {
            Some("mcpdocserviceextractdocumentassets")
        }
        "edit" | "editfile" | "functioneditfile" => Some("editfile"),
        "grep" | "grepsearch" | "functiongrepsearch" => Some("grepsearch"),
        "glob" | "globsearch" | "functionglobsearch" => Some("globsearch"),
        _ => None,
    }
}

fn resolve_requested_tool_name(
    requested_name: &str,
    available_tool_names: &BTreeSet<String>,
) -> Option<String> {
    if available_tool_names.contains(requested_name) {
        return Some(requested_name.to_string());
    }

    let normalized_available = available_tool_names
        .iter()
        .map(|name| (normalize_tool_key(name), name.clone()))
        .collect::<BTreeMap<_, _>>();
    let requested_key = normalize_tool_key(requested_name);
    if let Some(canonical) = normalized_available.get(&requested_key) {
        return Some(canonical.clone());
    }
    tool_alias_key(&requested_key).and_then(|alias| normalized_available.get(alias).cloned())
}

fn normalize_tool_uses(
    message: &mut ConversationMessage,
    available_tool_names: &BTreeSet<String>,
) -> Vec<(String, String, String)> {
    message
        .blocks
        .iter_mut()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => {
                if let Some(canonical_name) =
                    resolve_requested_tool_name(name, available_tool_names)
                {
                    *name = canonical_name;
                }
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .collect()
}

fn repair_pending_document_generation_tool_uses(
    message: &mut ConversationMessage,
    pending_tool_uses: &mut [(String, String, String)],
    user_input: &str,
    source_attachments: &[AttachmentEvidence],
    asset_manifest_path: Option<&str>,
) {
    let mut repaired_by_id = BTreeMap::new();
    for (tool_use_id, tool_name, input) in pending_tool_uses.iter_mut() {
        let Ok(mut value) = serde_json::from_str::<Value>(input) else {
            continue;
        };
        let mut repaired = false;
        if is_generate_file_tool(tool_name) {
            normalize_document_generation_tool_input(&mut value, user_input);
            if value.is_object() {
                repair_document_generation_tool_input(
                    &mut value,
                    tool_name,
                    user_input,
                    source_attachments,
                    asset_manifest_path,
                );
                repaired = true;
            }
        } else if normalize_tool_key(tool_name) == "mcptool" {
            let qualified_name = value
                .get("qualifiedName")
                .or_else(|| value.get("qualified_name"))
                .or_else(|| value.get("tool"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(qualified_name) =
                qualified_name.filter(|name| is_doc_service_generation_tool(name))
            {
                if let Some(object) = value.as_object_mut() {
                    let mut arguments = object
                        .remove("arguments")
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    if let Value::String(raw) = arguments {
                        arguments = serde_json::from_str::<Value>(raw.trim())
                            .or_else(|_| {
                                serde_json::from_str::<Value>(&jsonish_python_literals(raw.trim()))
                            })
                            .unwrap_or_else(|_| Value::Object(Map::new()));
                    }
                    normalize_document_generation_tool_input(&mut arguments, user_input);
                    if arguments.is_object() {
                        repair_document_generation_tool_input(
                            &mut arguments,
                            &qualified_name,
                            user_input,
                            source_attachments,
                            asset_manifest_path,
                        );
                        object.insert("arguments".to_string(), arguments);
                        repaired = true;
                    }
                }
            }
        }
        if !repaired {
            continue;
        }
        let repaired_input = value.to_string();
        *input = repaired_input.clone();
        repaired_by_id.insert(tool_use_id.clone(), repaired_input);
    }
    if repaired_by_id.is_empty() {
        return;
    }
    for block in &mut message.blocks {
        if let ContentBlock::ToolUse { id, input, .. } = block {
            if let Some(repaired) = repaired_by_id.get(id) {
                *input = repaired.clone();
            }
        }
    }
}

fn unsupported_tool_output(tool_name: &str, available_tool_names: &BTreeSet<String>) -> String {
    let mut available = available_tool_names.iter().cloned().collect::<Vec<_>>();
    available.sort();
    let preview = if available.is_empty() {
        "no tools are currently available".to_string()
    } else {
        let mut shown = available
            .into_iter()
            .take(16)
            .collect::<Vec<_>>()
            .join(", ");
        if available_tool_names.len() > 16 {
            shown.push_str(", ...");
        }
        format!("available tools include: {shown}")
    };
    format!(
        "unsupported tool requested by model: `{tool_name}`. The tool was not executed; {preview}. Use one of the advertised tool names exactly."
    )
}

fn language_output_contract(language: &str) -> String {
    match language {
        "Chinese" => "Output-language contract: respond to the user in Chinese for this and later turns unless the user explicitly changes language. Keep technical identifiers, commands, code, file paths, and quoted source text unchanged; prose headings and explanations must be Chinese.".to_string(),
        "English" => "Output-language contract: respond to the user in English for this and later turns unless the user explicitly changes language. Keep technical identifiers, commands, code, file paths, and quoted source text unchanged.".to_string(),
        other => format!(
            "Output-language contract: respond to the user in {other} for this and later turns unless the user explicitly changes language. Keep technical identifiers, commands, code, file paths, and quoted source text unchanged."
        ),
    }
}

/// Detect the dominant natural language of free-text user input by script, so
/// the agent can default its output language to match the user's input when no
/// explicit preference is configured. Conservative for Latin (English stays the
/// model default), but recognizes major non-Latin scripts so a user writing in
/// Chinese/Japanese/Korean/Cyrillic/Arabic gets a reply in that language.
fn detect_input_language(text: &str) -> Option<String> {
    let mut han = 0usize;
    let mut hiragana_katakana = 0usize;
    let mut hangul = 0usize;
    let mut cyrillic = 0usize;
    let mut arabic = 0usize;
    let mut latin = 0usize;
    for ch in text.chars() {
        if ('\u{4E00}'..='\u{9FFF}').contains(&ch)
            || ('\u{3400}'..='\u{4DBF}').contains(&ch)
            || ('\u{F900}'..='\u{FAFF}').contains(&ch)
        {
            han += 1;
        } else if ('\u{3040}'..='\u{30FF}').contains(&ch) {
            hiragana_katakana += 1;
        } else if ('\u{AC00}'..='\u{D7A3}').contains(&ch) || ('\u{1100}'..='\u{11FF}').contains(&ch)
        {
            hangul += 1;
        } else if ('\u{0400}'..='\u{04FF}').contains(&ch) {
            cyrillic += 1;
        } else if ('\u{0600}'..='\u{06FF}').contains(&ch) {
            arabic += 1;
        } else if ch.is_ascii_alphabetic() {
            latin += 1;
        }
    }

    // Japanese kana is the strongest signal for Japanese (even mixed with Han).
    if hiragana_katakana >= 2 {
        return Some("Japanese".to_string());
    }
    if hangul >= 2 {
        return Some("Korean".to_string());
    }
    // Require a meaningful share so a Latin prompt quoting one foreign word does
    // not flip the whole response.
    if han >= 2 && han * 2 >= latin {
        return Some("Chinese".to_string());
    }
    if cyrillic >= 2 && cyrillic * 2 >= latin {
        return Some("Russian".to_string());
    }
    if arabic >= 2 && arabic * 2 >= latin {
        return Some("Arabic".to_string());
    }
    None
}

fn latest_language_preference(memory: &LongTermMemory) -> Option<String> {
    memory
        .entries
        .iter()
        .filter(|entry| {
            entry.kind == MemoryKind::LanguagePreference
                && entry.topic == "language_preference"
                && !entry.note.trim().is_empty()
        })
        .max_by(|left, right| {
            left.confidence
                .partial_cmp(&right.confidence)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.ts_ms.cmp(&right.ts_ms))
        })
        .map(|entry| entry.note.clone())
}

fn active_language_preference(facts: &[(MemoryKind, String, String)]) -> Option<String> {
    facts
        .iter()
        .rev()
        .find(|(kind, topic, note)| {
            *kind == MemoryKind::LanguagePreference
                && topic == "language_preference"
                && !note.trim().is_empty()
        })
        .map(|(_, _, note)| note.clone())
}

fn format_user_memory_override(facts: &[(MemoryKind, String, String)]) -> Option<String> {
    if facts.is_empty() {
        return None;
    }

    let mut lines = vec![
        "# User-declared identity and preferences for this turn".to_string(),
        "The user explicitly provided these facts or preferences in the current message. Follow them immediately.".to_string(),
    ];
    for (kind, _, note) in facts {
        match kind {
            MemoryKind::UserIdentity => lines.push(format!("- Address the user as: {note}")),
            MemoryKind::AssistantIdentity => {
                lines.push(format!("- Use this assistant name for yourself: {note}"));
            }
            MemoryKind::LanguagePreference => {
                lines.push(format!("- Preferred response language: {note}"));
                lines.push(format!("- {}", language_output_contract(note)));
            }
            MemoryKind::UserPreference => lines.push(format!("- User preference: {note}")),
            MemoryKind::General => {}
        }
    }
    lines.push("User-declared assistant name and language preference override provider/model identity and default language. Do not identify yourself as the underlying model or provider unless the user asks about technical implementation.".to_string());
    Some(lines.join("\n"))
}

fn format_relevant_memory_context(entries: &[MemoryEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }

    let mut lines = vec![
        "# Relevant long-term memory for this turn".to_string(),
        "These recalled notes matched the current request. Use them when they are applicable, but prefer current workspace evidence when they conflict.".to_string(),
    ];
    for entry in entries {
        let kind = match entry.kind {
            MemoryKind::General => "general",
            MemoryKind::UserIdentity => "user_identity",
            MemoryKind::AssistantIdentity => "assistant_identity",
            MemoryKind::LanguagePreference => "language_preference",
            MemoryKind::UserPreference => "user_preference",
        };
        lines.push(format!(
            "- [{kind}] {}: {} (confidence: {:.0}%)",
            entry.topic,
            entry.note,
            entry.confidence * 100.0
        ));
    }
    Some(lines.join("\n"))
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
        ReasoningStep::Reflection {
            critique,
            adjustment,
        } => adjustment
            .as_ref()
            .map_or_else(|| critique.clone(), |value| format!("{critique} {value}")),
        ReasoningStep::Decision { choice, reasoning } => format!("{choice} {reasoning}"),
        ReasoningStep::RedactedThinking { .. } => "[redacted thinking]".to_string(),
    }
}

fn extract_memory_topics(text: &str, limit: usize) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "about",
        "after",
        "again",
        "also",
        "analysis",
        "and",
        "another",
        "because",
        "before",
        "being",
        "between",
        "could",
        "decision",
        "during",
        "first",
        "from",
        "have",
        "into",
        "need",
        "next",
        "only",
        "plan",
        "reason",
        "reasoning",
        "reflection",
        "should",
        "steps",
        "that",
        "their",
        "there",
        "this",
        "tool",
        "turn",
        "used",
        "using",
        "with",
        "within",
        "would",
        "your",
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

            for capability in infer_tool_capabilities(&tool_name, None)
                .into_iter()
                .take(3)
            {
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
                .map(|alternative| {
                    alternative
                        .description
                        .chars()
                        .take(100)
                        .collect::<String>()
                })
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

pub trait PlanExecutionEventReporter: Send + Sync {
    fn emit_plan_execution_event(&self, event: &PlanExecutionEvent);
}

pub trait ModelRouteEventReporter: Send + Sync {
    fn emit_model_route_event(&self, event: &ModelRouteDecision);
}

pub trait TaskLedgerEventReporter: Send + Sync {
    fn emit_task_ledger_event(&self, event: &crate::ProgressLedgerEntry);
}

pub trait TeamExecutionEventReporter: Send + Sync {
    fn emit_team_execution_event(&self, event: &TeamExecutionEvent);
}

struct DecisioningTurnPlan {
    engine: DecisioningEngine,
    task: Task,
    snapshot: DecisioningSnapshot,
    dag: crate::PlanDag,
    execution: PlanExecution,
    next_execution_event_offset: usize,
    selected_positions: BTreeMap<String, usize>,
    workspace_evidence_stage_active: bool,
    /// Optional model-driven planning guidance for high-complexity tasks,
    /// folded into the advisory prompt alongside the heuristic plan.
    model_planning_guidance: Option<String>,
}

/// Flow decision returned by `finalize_turn_or_recover` after verification.
enum TurnFlow {
    /// Verification passed or was not required — finish the turn.
    Complete,
    /// Verification failed but recovery converged and budget remains — feed the
    /// failure detail and recovery plan back into the conversation, then retry.
    Redrive { guidance: String },
    /// Verification failed terminally — fail the turn with this error.
    Fail(RuntimeError),
}

/// Outcome of running one structured-plan node (with per-node verification and
/// bounded local re-drive).
enum NodeRunResult {
    Succeeded { summary: String },
    Failed { reason: String },
}

/// Outcome of verifying a node's acceptance criteria.
enum NodeVerifyOutcome {
    Passed,
    /// No acceptance commands, or running them is not permitted in this mode.
    Skipped,
    Failed {
        reason: String,
    },
}

/// Coordinates the model loop, tool execution, hooks, and session updates.
pub struct ConversationRuntime<C, T> {
    session: Session,
    api_client: C,
    tool_executor: T,
    permission_policy: PermissionPolicy,
    system_prompt: Vec<String>,
    max_iterations: usize,
    max_recovery_attempts: usize,
    usage_tracker: UsageTracker,
    hook_runner: HookRunner,
    decisioning_config: DecisioningConfig,
    decisioning_event_reporter: Option<Arc<dyn DecisioningEventReporter>>,
    plan_execution_event_reporter: Option<Arc<dyn PlanExecutionEventReporter>>,
    task_ledger_event_reporter: Option<Arc<dyn TaskLedgerEventReporter>>,
    model_route_event_reporter: Option<Arc<dyn ModelRouteEventReporter>>,
    team_execution_event_reporter: Option<Arc<dyn TeamExecutionEventReporter>>,
    runtime_event_reporter: Option<Arc<dyn RuntimeEventReporter>>,
    task_registry: TaskRegistry,
    model_router: ModelRouter,
    workspace_route_feedback: Vec<ModelRouteFeedback>,
    verification_runner: VerificationRunner,
    failure_classifier: FailureClassifier,
    recovery_action_engine: RecoveryActionEngine,
    auto_compaction_input_tokens_threshold: u32,
    hook_abort_signal: HookAbortSignal,
    hook_progress_reporter: Option<Box<dyn HookProgressReporter>>,
    session_tracer: Option<SessionTracer>,
    pending_user_blocks: Vec<ContentBlock>,
    resume_task_id: Option<String>,
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
        let verification_workspace_root = session.workspace_root().map(Path::to_path_buf);
        Self {
            session,
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
            max_iterations: DEFAULT_MAX_CONVERSATION_ITERATIONS,
            max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
            usage_tracker,
            hook_runner: HookRunner::from_feature_config(feature_config),
            decisioning_config: feature_config.decisioning().clone(),
            decisioning_event_reporter: None,
            plan_execution_event_reporter: None,
            task_ledger_event_reporter: None,
            model_route_event_reporter: None,
            team_execution_event_reporter: None,
            runtime_event_reporter: None,
            task_registry: TaskRegistry::new(),
            model_router: ModelRouter::new(MoERoutingPolicy::balanced("sonnet")),
            workspace_route_feedback: Vec::new(),
            verification_runner: VerificationRunner::new(verification_workspace_root),
            failure_classifier: FailureClassifier::new(),
            recovery_action_engine: RecoveryActionEngine::new(),
            auto_compaction_input_tokens_threshold: auto_compaction_threshold_from_env(),
            hook_abort_signal: HookAbortSignal::default(),
            hook_progress_reporter: None,
            session_tracer: None,
            pending_user_blocks: Vec::new(),
            resume_task_id: None,
        }
    }

    #[must_use]
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    /// Override how many times a turn may automatically re-drive the model to
    /// fix a failed verification before failing the turn. `0` restores the
    /// legacy single-shot behavior.
    #[must_use]
    pub fn with_max_recovery_attempts(mut self, max_recovery_attempts: usize) -> Self {
        self.max_recovery_attempts = max_recovery_attempts;
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

    #[must_use]
    pub fn with_plan_execution_event_reporter(
        mut self,
        reporter: impl PlanExecutionEventReporter + 'static,
    ) -> Self {
        self.plan_execution_event_reporter = Some(Arc::new(reporter));
        self
    }

    #[must_use]
    pub fn with_task_ledger_event_reporter(
        mut self,
        reporter: impl TaskLedgerEventReporter + 'static,
    ) -> Self {
        self.task_ledger_event_reporter = Some(Arc::new(reporter));
        self
    }

    #[must_use]
    pub fn with_model_route_event_reporter(
        mut self,
        reporter: impl ModelRouteEventReporter + 'static,
    ) -> Self {
        self.model_route_event_reporter = Some(Arc::new(reporter));
        self
    }

    #[must_use]
    pub fn with_team_execution_event_reporter(
        mut self,
        reporter: impl TeamExecutionEventReporter + 'static,
    ) -> Self {
        self.team_execution_event_reporter = Some(Arc::new(reporter));
        self
    }

    #[must_use]
    pub fn with_runtime_event_reporter(
        mut self,
        reporter: impl RuntimeEventReporter + 'static,
    ) -> Self {
        self.runtime_event_reporter = Some(Arc::new(reporter));
        self
    }

    #[must_use]
    pub fn with_task_registry(mut self, registry: TaskRegistry) -> Self {
        self.task_registry = registry;
        self
    }

    #[must_use]
    pub fn with_model_router(mut self, router: ModelRouter) -> Self {
        self.model_router = router;
        self
    }

    #[must_use]
    pub fn with_workspace_route_feedback(mut self, feedback: Vec<ModelRouteFeedback>) -> Self {
        self.workspace_route_feedback = feedback;
        self
    }

    #[must_use]
    pub fn task_registry(&self) -> &TaskRegistry {
        &self.task_registry
    }

    #[must_use]
    pub fn with_resume_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.resume_task_id = Some(task_id.into());
        self
    }

    #[must_use]
    pub fn with_verification_runner(mut self, runner: VerificationRunner) -> Self {
        self.verification_runner = runner;
        self
    }

    #[must_use]
    pub fn with_failure_classifier(mut self, classifier: FailureClassifier) -> Self {
        self.failure_classifier = classifier;
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

    /// Queue content blocks to be included in the next user turn.
    ///
    /// This is used to attach file or image content without changing the
    /// `run_turn` signature. The next [`run_turn`] call merges the blocks after
    /// the prompt text in the same user message so providers and models see the
    /// attachment as context for the request without burying the actual ask.
    ///
    /// # Errors
    /// This method currently cannot fail; it returns [`Result`] for API
    /// compatibility with earlier eager-persistence behavior.
    pub fn inject_user_blocks(&mut self, blocks: Vec<ContentBlock>) -> Result<(), RuntimeError> {
        self.pending_user_blocks.extend(blocks);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub fn run_turn(
        &mut self,
        user_input: impl Into<String>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnSummary, RuntimeError> {
        let user_input = user_input.into();
        let requires_workspace_analysis = user_requests_current_workspace_analysis(&user_input);
        let requires_document_generation = user_requests_document_generation(&user_input);
        let mut task_state = TurnTaskState::new(
            &user_input,
            requires_workspace_analysis,
            requires_document_generation,
        );
        let runtime_task = if let Some(task_id) = self.resume_task_id.take() {
            self.task_registry
                .resume(&task_id)
                .map_err(RuntimeError::new)?
        } else {
            task_packet_from_input(&user_input)
                .and_then(|packet| self.task_registry.create_from_packet(packet).ok())
                .unwrap_or_else(|| {
                    self.task_registry
                        .create(&user_input, Some("conversation_turn"))
                })
        };
        let runtime_task_id = runtime_task.task_id.clone();
        let mut task_ledger_offset = 0;
        self.emit_task_ledger_events(&runtime_task_id, task_ledger_offset);
        task_ledger_offset = self.task_registry.ledger_for_task(&runtime_task_id).len();
        if !matches!(
            runtime_task.status,
            crate::TaskStatus::Completed
                | crate::TaskStatus::Failed
                | crate::TaskStatus::Stopped
                | crate::TaskStatus::Cancelled
        ) {
            let _ = self
                .task_registry
                .set_status(&runtime_task_id, crate::TaskStatus::Running);
        }
        self.emit_task_ledger_events(&runtime_task_id, task_ledger_offset);
        task_ledger_offset = self.task_registry.ledger_for_task(&runtime_task_id).len();
        self.record_turn_started(&user_input);
        let mut user_blocks = vec![ContentBlock::Text {
            text: user_input.clone(),
        }];
        if requires_workspace_analysis {
            if let Some(root) = self.session.workspace_root() {
                if let Some(context) = workspace_analysis_context(root) {
                    user_blocks.push(ContentBlock::Text { text: context });
                }
            }
        }
        user_blocks.extend(std::mem::take(&mut self.pending_user_blocks));
        self.session
            .push_message(ConversationMessage {
                role: MessageRole::User,
                blocks: user_blocks,
                usage: None,
            })
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let mut source_attachments =
            collect_attachment_evidence_from_messages(&self.session.messages);
        if source_attachments.is_empty() && task_state.requires_document_generation {
            source_attachments =
                collect_recent_workspace_attachment_manifests(self.session.workspace_root(), 3);
        }
        task_state.set_source_attachments(source_attachments);
        if !task_state.source_attachments.is_empty() {
            self.record_and_emit_task_progress(
                &runtime_task_id,
                &mut task_ledger_offset,
                "attachments_ready",
                Some(format!(
                    "{} attachment/source item(s) available",
                    task_state.source_attachments.len()
                )),
            );
        }

        let mut assistant_messages = Vec::new();
        let mut tool_results = Vec::new();
        let mut prompt_cache_events = Vec::new();
        let mut iterations = 0;
        let mut recovery_attempts = 0;
        let mut chain_of_thought: Option<ChainOfThought> = None;
        let user_memory_facts = extract_user_memory_facts(&user_input);
        if !user_memory_facts.is_empty() {
            let mut memory = LongTermMemory::load_for_workspace(self.session.workspace_root());
            record_user_memory(&mut memory, &user_memory_facts);
        }
        let mut effective_system_prompt = self.system_prompt.clone();
        // Complexity signal used to bias model routing toward higher-quality
        // routes for harder tasks (difficulty-aware routing).
        let mut task_complexity: Option<u8> = None;
        // Validated structured plan (model-derived DAG) when this turn is
        // eligible; consumed by later stages to dispatch node-by-node.
        let mut structured_plan: Option<crate::structured_execution::ValidatedStructuredPlan> =
            None;
        if let Some(initial_plan) =
            self.build_initial_decisioning_plan(&runtime_task_id, &user_input)
        {
            task_complexity = Some(initial_plan.task.complexity);
            // Stage 0: assess whether this turn is eligible for structured DAG
            // execution. The decision is recorded for observability. Stage 1
            // builds the model-derived structured plan when eligible.
            let feasibility = crate::structured_execution::assess_feasibility(
                self.decisioning_config.enabled(),
                self.decisioning_config.structured_execution_threshold(),
                initial_plan.task.complexity,
                &initial_plan.snapshot.plan,
            );
            self.record_structured_feasibility(&runtime_task_id, &feasibility);
            if feasibility.is_eligible() {
                structured_plan = self.maybe_build_structured_plan(
                    &runtime_task_id,
                    &user_input,
                    &initial_plan.task,
                );
                if let Some(validated) = &structured_plan {
                    effective_system_prompt.push(format_structured_plan_prompt(validated));
                }
            }
            effective_system_prompt.push(Self::format_initial_decisioning_prompt(&initial_plan));
        }
        // Stage 2: dispatch the structured plan node-by-node in dependency
        // order before the main turn. Each node runs a focused model sub-turn;
        // results are folded into the system prompt so the main loop completes
        // the user-facing answer with the structured work already done.
        if let Some(validated) = structured_plan.take() {
            if let Some(node_report) =
                self.dispatch_structured_plan(&runtime_task_id, &user_input, &validated)
            {
                effective_system_prompt.push(node_report);
            }
        }
        if let Some(memory_override) = format_user_memory_override(&user_memory_facts) {
            effective_system_prompt.push(memory_override);
        }
        let memory = LongTermMemory::load_for_workspace(self.session.workspace_root());
        // Explicit language declarations win. Otherwise default to the
        // language the user used in this turn, and only fall back to stored
        // language memory for language-ambiguous prompts (for example short
        // code-only follow-ups).
        let active_language = active_language_preference(&user_memory_facts)
            .or_else(|| detect_input_language(&user_input))
            .or_else(|| latest_language_preference(&memory));
        task_state.set_output_language(active_language.clone());
        if let Some(language) = &active_language {
            // Insert at the FRONT of the system prompt for maximum salience —
            // weaker local models otherwise ignore a contract buried mid-prompt.
            effective_system_prompt.insert(
                0,
                format!(
                    "# Output language (MANDATORY)\nYou MUST write your entire response to the user in {language}. This overrides any default. {}",
                    language_output_contract(language)
                ),
            );
            // Also append a terse reminder to the END of the just-pushed user
            // message. Weak local models follow an instruction placed closest
            // to generation far more reliably than one buried in the system
            // prompt; this is the single most effective lever for them.
            if let Some(last) = self.session.messages.last_mut() {
                if last.role == MessageRole::User {
                    last.blocks.push(ContentBlock::Text {
                        text: format!("(Reply in {language}.)"),
                    });
                }
            }
        }
        if let Some(relevant_memory) =
            format_relevant_memory_context(&memory.relevant_entries(&user_input, 12))
        {
            effective_system_prompt.push(relevant_memory);
        }
        if task_state.requires_document_generation {
            self.record_and_emit_task_progress(
                &runtime_task_id,
                &mut task_ledger_offset,
                "document_generation_preflight",
                Some(document_generation_preflight_message(
                    &user_input,
                    &task_state.source_attachments,
                )),
            );
            if let Some(attachment_context) = format_attachment_context_prompt(
                &task_state.source_attachments,
                active_language.as_deref(),
            ) {
                effective_system_prompt.push(attachment_context);
            }
            effective_system_prompt.push(document_generation_long_task_prompt(
                &user_input,
                &task_state.source_attachments,
                active_language.as_deref(),
            ));
        }
        effective_system_prompt.push(task_state.format_context());

        'turn: loop {
            iterations += 1;
            if iterations > self.max_iterations {
                let error = RuntimeError::new(
                    "conversation loop exceeded the maximum number of iterations",
                );
                let error = self.fail_turn_with_task_status(
                    &runtime_task_id,
                    &mut task_ledger_offset,
                    iterations,
                    error,
                );
                return Err(error);
            }

            let model_route = self.select_model_route_for_task_with_complexity(
                &runtime_task_id,
                crate::ModelRoutePhase::Coding,
                task_complexity,
            );
            let model_started_at = std::time::Instant::now();
            let request = ApiRequest {
                system_prompt: {
                    let mut prompt = effective_system_prompt.clone();
                    prompt.push(task_state.format_context());
                    prompt
                },
                messages: self.session.messages.clone(),
                model_route: Some(model_route),
            };
            let events = match self.api_client.stream(request) {
                Ok(events) => events,
                Err(error) => {
                    let error = self.fail_turn_with_task_status(
                        &runtime_task_id,
                        &mut task_ledger_offset,
                        iterations,
                        error,
                    );
                    return Err(error);
                }
            };
            let (mut assistant_message, usage, turn_prompt_cache_events, cot_part) =
                match build_assistant_message(events) {
                    Ok(result) => result,
                    Err(error) => {
                        let error = self.fail_turn_with_task_status(
                            &runtime_task_id,
                            &mut task_ledger_offset,
                            iterations,
                            error,
                        );
                        return Err(error);
                    }
                };
            if let Some(usage) = usage {
                let model_latency_ms = model_started_at
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u32::MAX)) as u32;
                self.usage_tracker.record(usage);
                self.enrich_latest_route_feedback_from_usage(
                    &runtime_task_id,
                    usage,
                    model_latency_ms,
                    None,
                );
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
            let available_tool_names = self
                .tool_executor
                .available_tools()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<BTreeSet<_>>();
            let mut pending_tool_uses =
                normalize_tool_uses(&mut assistant_message, &available_tool_names);
            let assistant_text = assistant_plain_text(&assistant_message);
            task_state.observe_super_ppt_artifacts(&assistant_text);
            if task_state.requires_generate_file_deliverable {
                repair_pending_document_generation_tool_uses(
                    &mut assistant_message,
                    &mut pending_tool_uses,
                    &user_input,
                    &task_state.source_attachments,
                    task_state.document_asset_manifest_path.as_deref(),
                );
            }
            if pending_tool_uses.is_empty()
                && task_state.requires_workspace_analysis
                && !task_state.evidence_complete()
                && workspace_evidence_tools_available(&available_tool_names)
            {
                if task_state.should_prompt_for_evidence() {
                    self.session
                        .push_message(ConversationMessage::user_text(
                            workspace_evidence_gate_prompt(),
                        ))
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                    continue;
                }
                let error = RuntimeError::new(
                    "workspace analysis required local search/read evidence, but the assistant did not request the required workspace tools before answering",
                );
                let error = self.fail_turn_with_task_status(
                    &runtime_task_id,
                    &mut task_ledger_offset,
                    iterations,
                    error,
                );
                return Err(error);
            }
            if pending_tool_uses.is_empty()
                && task_state.requires_super_ppt_deliverable
                && !task_state.super_ppt_artifact_complete()
                && !super_ppt_blocker_text(&assistant_text)
            {
                if task_state.should_prompt_for_super_ppt_completion(&assistant_text) {
                    self.record_and_emit_task_progress(
                        &runtime_task_id,
                        &mut task_ledger_offset,
                        "super_ppt_completion_redrive",
                        Some("assistant stopped before producing SuperPPT artifacts or an explicit imagegen blocker; completion gate redrive scheduled".to_string()),
                    );
                    self.session
                        .push_message(ConversationMessage::user_text(
                            super_ppt_completion_gate_prompt(
                                &user_input,
                                &task_state.source_attachments,
                            ),
                        ))
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                    continue;
                }
                let error = RuntimeError::new(
                    "SuperPPT request required PPTX plus imagegen manifest/slides artifacts or an explicit imagegen blocker, but the assistant stopped with progress text only",
                );
                let error = self.fail_turn_with_task_status(
                    &runtime_task_id,
                    &mut task_ledger_offset,
                    iterations,
                    error,
                );
                return Err(error);
            }
            if pending_tool_uses.is_empty()
                && task_state.requires_generate_file_deliverable
                && !task_state.document_generation_complete()
            {
                if !document_generation_tool_available(&available_tool_names) {
                    let error = RuntimeError::new(
                        "document generation request requires a document-generation tool, but none is available",
                    );
                    let error = self.fail_turn_with_task_status(
                        &runtime_task_id,
                        &mut task_ledger_offset,
                        iterations,
                        error,
                    );
                    return Err(error);
                }
                let asset_preflight_tool_use = if task_state.document_asset_manifest_path.is_none()
                    && !task_state
                        .observed_tools
                        .iter()
                        .any(|name| is_document_asset_extract_tool(name))
                {
                    document_generation_asset_extract_tool_use(
                        &user_input,
                        &task_state.source_attachments,
                        &available_tool_names,
                    )
                } else {
                    None
                };
                if let Some((tool_use_id, tool_name, input)) = asset_preflight_tool_use {
                    self.record_and_emit_task_progress(
                        &runtime_task_id,
                        &mut task_ledger_offset,
                        "document_asset_extraction_preflight",
                        Some("scheduling deterministic source asset extraction before document generation".to_string()),
                    );
                    assistant_message.blocks.push(ContentBlock::ToolUse {
                        id: tool_use_id.clone(),
                        name: tool_name.clone(),
                        input: input.clone(),
                    });
                    pending_tool_uses.push((tool_use_id, tool_name, input));
                } else if let Some((tool_use_id, tool_name, input)) =
                    document_generation_text_tool_use(
                        &assistant_text,
                        &user_input,
                        &task_state.source_attachments,
                        &available_tool_names,
                        task_state.document_asset_manifest_path.as_deref(),
                    )
                {
                    self.record_and_emit_task_progress(
                        &runtime_task_id,
                        &mut task_ledger_offset,
                        "document_generation_text_tool_call",
                        Some("assistant wrote a document-generation tool call as text; converting it into an executable tool call".to_string()),
                    );
                    assistant_message.blocks.push(ContentBlock::ToolUse {
                        id: tool_use_id.clone(),
                        name: tool_name.clone(),
                        input: input.clone(),
                    });
                    pending_tool_uses.push((tool_use_id, tool_name, input));
                } else if task_state.should_prompt_for_document_generation() {
                    self.record_and_emit_task_progress(
                        &runtime_task_id,
                        &mut task_ledger_offset,
                        "document_generation_redrive",
                        Some("assistant stopped before generating the requested file; completion gate redrive scheduled".to_string()),
                    );
                    self.session
                        .push_message(ConversationMessage::user_text(
                            document_generation_gate_prompt(
                                &user_input,
                                &task_state.source_attachments,
                                task_state.generated_document_quality_summary.as_deref(),
                            ),
                        ))
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                    continue;
                }
                let error = RuntimeError::new(document_generation_failure_message(
                    &task_state.source_attachments,
                    &available_tool_names,
                ));
                let error = self.fail_turn_with_task_status(
                    &runtime_task_id,
                    &mut task_ledger_offset,
                    iterations,
                    error,
                );
                return Err(error);
            }
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
                if task_state.requires_super_ppt_deliverable
                    && !task_state.super_ppt_artifact_complete()
                    && super_ppt_blocker_text(&assistant_text)
                {
                    self.finalize_turn_as_blocked(
                        &runtime_task_id,
                        &mut task_ledger_offset,
                        format!(
                            "SuperPPT blocked before delivery: {}",
                            assistant_text.trim().chars().take(320).collect::<String>()
                        ),
                    );
                    break 'turn;
                }
                match self.finalize_turn_or_recover(
                    &runtime_task_id,
                    &mut task_ledger_offset,
                    iterations,
                    recovery_attempts,
                ) {
                    TurnFlow::Complete => break 'turn,
                    TurnFlow::Fail(error) => return Err(error),
                    TurnFlow::Redrive { guidance } => {
                        recovery_attempts += 1;
                        self.session
                            .push_message(ConversationMessage::user_text(guidance))
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                        continue 'turn;
                    }
                }
            }

            let mut decisioning_plan = self.build_decisioning_turn_plan(
                &runtime_task_id,
                &user_input,
                chain_of_thought.as_ref(),
                &pending_tool_uses,
                !task_state.evidence_complete(),
            );

            let mut ordered_pending_tool_uses = pending_tool_uses
                .into_iter()
                .enumerate()
                .collect::<Vec<_>>();
            if let Some(plan) = decisioning_plan.as_ref() {
                ordered_pending_tool_uses.sort_by(
                    |(left_index, left_use), (right_index, right_use)| {
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
                    },
                );
            }

            let mut step_outcomes = Vec::new();

            for (_, (tool_use_id, tool_name, input)) in ordered_pending_tool_uses {
                let tool_started_at = Instant::now();
                self.record_and_emit_task_progress(
                    &runtime_task_id,
                    &mut task_ledger_offset,
                    "tool_requested",
                    Some(format!("{tool_name} requested")),
                );
                if !available_tool_names.contains(&tool_name) {
                    let plan_step_id = decisioning_plan
                        .as_ref()
                        .and_then(|plan| {
                            plan.snapshot.plan.steps.iter().find(|step| {
                                step.candidate_tools
                                    .iter()
                                    .any(|candidate| candidate == &tool_name)
                            })
                        })
                        .map(|step| step.id.clone())
                        .unwrap_or_else(|| tool_use_id.clone());
                    if let Some(plan) = decisioning_plan.as_mut() {
                        let mut scheduler = ExecutionScheduler::new(&plan.dag, &mut plan.execution);
                        let _ = scheduler.fail_node(&plan_step_id, "unsupported_tool");
                    }
                    if let Some(plan) = decisioning_plan.as_ref() {
                        self.emit_plan_execution_events(
                            &plan.execution,
                            plan.next_execution_event_offset,
                        );
                    }
                    if let Some(plan) = decisioning_plan.as_mut() {
                        plan.next_execution_event_offset = plan.execution.events.len();
                    }
                    task_state.record_tool_use(&tool_name, &input);
                    let output = unsupported_tool_output(&tool_name, &available_tool_names);
                    let result_message = ConversationMessage::tool_result(
                        tool_use_id.clone(),
                        tool_name.clone(),
                        output.clone(),
                        true,
                    );
                    let step_latency_ms = tool_started_at
                        .elapsed()
                        .as_millis()
                        .min(u128::from(u32::MAX)) as u32;
                    step_outcomes.push(StepOutcome {
                        step_id: plan_step_id,
                        succeeded: false,
                        latency_ms: step_latency_ms,
                        notes: vec![format!("unsupported_tool={tool_name}")],
                    });
                    self.session
                        .push_message(result_message.clone())
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                    self.record_tool_finished(iterations, &result_message);
                    let state_tool_name =
                        observed_document_generation_tool_name(&tool_name, &input)
                            .unwrap_or_else(|| tool_name.clone());
                    task_state.record_tool_result(&state_tool_name, true, &output);
                    self.record_and_emit_task_progress(
                        &runtime_task_id,
                        &mut task_ledger_offset,
                        "tool_finished",
                        Some(format!("{tool_name} failed: unsupported tool")),
                    );
                    tool_results.push(result_message);
                    continue;
                }
                let pre_hook_result = self.run_pre_tool_use_hook(&tool_name, &input);
                let effective_input = pre_hook_result
                    .updated_input()
                    .map_or_else(|| input.clone(), ToOwned::to_owned);
                let permission_context = PermissionContext::new(
                    pre_hook_result.permission_override(),
                    pre_hook_result.permission_reason().map(ToOwned::to_owned),
                );

                let decisioning_outcome = decisioning_plan.as_ref().and_then(|plan| {
                    self.assess_tool_decisioning(plan, &tool_name, &effective_input)
                });

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

                task_state.record_tool_use(&tool_name, &effective_input);

                let plan_step_id = decisioning_plan
                    .as_ref()
                    .and_then(|plan| {
                        plan.snapshot.plan.steps.iter().find(|step| {
                            step.candidate_tools
                                .iter()
                                .any(|candidate| candidate == &tool_name)
                        })
                    })
                    .map(|step| step.id.clone())
                    .unwrap_or_else(|| tool_use_id.clone());
                let permission_allowed = matches!(&permission_outcome, PermissionOutcome::Allow);
                let started_plan_step_id = if let Some(plan) = decisioning_plan.as_mut() {
                    let mut scheduler = ExecutionScheduler::new(&plan.dag, &mut plan.execution);
                    scheduler
                        .start_ready_node_for_tool(&tool_name)
                        .map(|selection| selection.node_id)
                } else {
                    None
                };
                if let Some(plan) = decisioning_plan.as_ref() {
                    self.emit_plan_execution_events(
                        &plan.execution,
                        plan.next_execution_event_offset,
                    );
                }
                if let Some(plan) = decisioning_plan.as_mut() {
                    plan.next_execution_event_offset = plan.execution.events.len();
                }
                let result_message = match permission_outcome {
                    PermissionOutcome::Allow => {
                        self.record_and_emit_task_progress(
                            &runtime_task_id,
                            &mut task_ledger_offset,
                            "tool_started",
                            Some(format!("{tool_name} started")),
                        );
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
                let step_latency_ms = tool_started_at
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u32::MAX)) as u32;
                let step_succeeded = permission_allowed
                    && !matches!(
                        result_message.blocks.first(),
                        Some(ContentBlock::ToolResult { is_error: true, .. })
                    );
                if let Some(node_id) = started_plan_step_id.as_ref() {
                    if let Some(plan) = decisioning_plan.as_mut() {
                        let mut scheduler = ExecutionScheduler::new(&plan.dag, &mut plan.execution);
                        let _ = scheduler.finish_node(
                            node_id,
                            step_succeeded,
                            Some(format!("tool {tool_name} completed")),
                            "tool_result_error",
                        );
                    }
                    if let Some(plan) = decisioning_plan.as_ref() {
                        self.emit_plan_execution_events(
                            &plan.execution,
                            plan.next_execution_event_offset,
                        );
                    }
                    if let Some(plan) = decisioning_plan.as_mut() {
                        plan.next_execution_event_offset = plan.execution.events.len();
                    }
                }
                step_outcomes.push(StepOutcome {
                    step_id: plan_step_id,
                    succeeded: step_succeeded,
                    latency_ms: step_latency_ms,
                    notes: vec![format!("tool={tool_name}",)],
                });
                if let Some(ContentBlock::ToolResult {
                    tool_name,
                    output,
                    is_error,
                    ..
                }) = result_message.blocks.first()
                {
                    let state_tool_name =
                        observed_document_generation_tool_name(tool_name, &effective_input)
                            .unwrap_or_else(|| tool_name.clone());
                    task_state.record_tool_result(&state_tool_name, *is_error, output);
                }
                self.session
                    .push_message(result_message.clone())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                self.record_tool_finished(iterations, &result_message);
                self.record_and_emit_task_progress(
                    &runtime_task_id,
                    &mut task_ledger_offset,
                    "tool_finished",
                    Some(format!(
                        "{tool_name} {}",
                        if step_succeeded {
                            "succeeded"
                        } else {
                            "failed"
                        }
                    )),
                );
                tool_results.push(result_message);
            }

            if let Some(plan) = decisioning_plan.as_ref() {
                if step_outcomes.iter().any(|outcome| !outcome.succeeded) {
                    let adjustment = plan
                        .engine
                        .planner
                        .adjust_plan(&plan.snapshot.plan, &step_outcomes);
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
        self.reflect_on_outcome(&chain_of_thought, &summary);
        self.record_turn_completed(&summary);

        Ok(summary)
    }

    /// Drive verification once the model has stopped requesting tools, then
    /// decide how the turn should proceed:
    ///
    /// - `Complete` — verification passed or was not required; finish the turn.
    /// - `Redrive` — verification failed but recovery succeeded and the
    ///   recovery budget still has room, so the failure detail and recovery
    ///   plan are fed back into the conversation for another fix-verify pass.
    /// - `Fail` — verification failed and the turn cannot recover (budget
    ///   exhausted, recovery did not converge, or verification stayed blocked).
    ///
    /// The bounded re-drive loop is what turns the verifier + recovery
    /// orchestrator from a single-shot reporter into an autonomous fix loop.
    fn finalize_turn_or_recover(
        &mut self,
        runtime_task_id: &str,
        task_ledger_offset: &mut usize,
        iterations: usize,
        recovery_attempts: usize,
    ) -> TurnFlow {
        let verification_route =
            self.select_model_route_for_task(runtime_task_id, crate::ModelRoutePhase::Verification);
        let mut verification_decision = self.evaluate_runtime_task_completion(runtime_task_id);
        if let VerificationDecision::Required(request) = &verification_decision {
            let result = if matches!(
                self.permission_policy.active_mode(),
                crate::PermissionMode::DangerFullAccess | crate::PermissionMode::Allow
            ) {
                self.verification_runner.run(request)
            } else {
                VerificationResult {
                    task_id: runtime_task_id.to_string(),
                    passed: false,
                    observed_green_level: None,
                    summary: format!(
                        "verification command execution requires danger-full-access; current mode is {}",
                        self.permission_policy.active_mode().as_str()
                    ),
                    evidence: request.acceptance_tests.clone(),
                }
            };
            let _ = self
                .task_registry
                .record_verification(runtime_task_id, result);
            self.emit_task_ledger_events(runtime_task_id, *task_ledger_offset);
            *task_ledger_offset = self.task_registry.ledger_for_task(runtime_task_id).len();
            verification_decision = self.evaluate_runtime_task_completion(runtime_task_id);
        }
        let mut team_ledger = TeamExecutionLedger::new(
            format!("team-{}", self.session.session_id),
            runtime_task_id.to_string(),
        );
        self.emit_team_execution_event_for_task(
            runtime_task_id,
            team_ledger
                .record_verification(&verification_decision, Some(verification_route.clone())),
        );
        match verification_decision.clone() {
            VerificationDecision::Failed { reason } => {
                let _ = self
                    .task_registry
                    .set_status(runtime_task_id, crate::TaskStatus::Recovering);
                self.emit_task_ledger_events(runtime_task_id, *task_ledger_offset);
                *task_ledger_offset = self.task_registry.ledger_for_task(runtime_task_id).len();
                let classification = self.failure_classifier.classify_reason(&reason);
                let mut recovery = RecoveryOrchestrator::new();
                let outcome = recovery.recover_once(classification.scenario);
                let recovery_succeeded = matches!(
                    outcome.decision,
                    crate::RecoveryOrchestratorDecision::Recovered
                );
                for event in outcome.events.clone() {
                    let _ = self
                        .task_registry
                        .record_recovery_event(runtime_task_id, event.clone());
                    self.emit_runtime_event(RuntimeEvent::Recovery(event));
                }
                self.emit_task_ledger_events(runtime_task_id, *task_ledger_offset);
                *task_ledger_offset = self.task_registry.ledger_for_task(runtime_task_id).len();
                let action_plan = self.recovery_action_engine.plan(
                    runtime_task_id.to_string(),
                    &outcome,
                    self.task_registry
                        .get(runtime_task_id)
                        .and_then(|task| task.plan)
                        .and_then(|plan| plan.resume_cursor)
                        .and_then(|cursor| cursor.node_id),
                );
                let action_execution = self.recovery_action_engine.execute_against_registry(
                    action_plan.clone(),
                    self.permission_policy.active_mode(),
                    &self.task_registry,
                );
                self.emit_runtime_event(RuntimeEvent::RecoveryAction(action_execution.clone()));
                self.emit_task_ledger_events(runtime_task_id, *task_ledger_offset);
                *task_ledger_offset = self.task_registry.ledger_for_task(runtime_task_id).len();

                // Bounded autonomous re-drive: if recovery converged and we
                // still have budget, feed the failure + recovery plan back to
                // the model and re-verify instead of giving up.
                if recovery_succeeded && recovery_attempts < self.max_recovery_attempts {
                    let _ = self
                        .task_registry
                        .set_status(runtime_task_id, crate::TaskStatus::Running);
                    self.emit_task_ledger_events(runtime_task_id, *task_ledger_offset);
                    *task_ledger_offset = self.task_registry.ledger_for_task(runtime_task_id).len();
                    self.emit_conversation_task_report(
                        runtime_task_id,
                        verification_route.clone(),
                        "conversation recovery redrive scheduled".to_string(),
                        false,
                        true,
                        Some(false),
                        verification_decision.clone(),
                        Some(classification.clone()),
                        Some(outcome.clone()),
                        Some(action_execution.clone()),
                    );
                    // Clear the stale failing result so the next pass re-verifies
                    // against the model's new attempt.
                    let _ = self.task_registry.clear_verification(runtime_task_id);
                    let guidance = format_recovery_redrive_guidance(
                        recovery_attempts + 1,
                        self.max_recovery_attempts,
                        &reason,
                        &action_plan,
                    );
                    return TurnFlow::Redrive { guidance };
                }

                let terminal_status = if recovery_succeeded {
                    crate::TaskStatus::Blocked
                } else {
                    crate::TaskStatus::Failed
                };
                let _ = self
                    .task_registry
                    .set_status(runtime_task_id, terminal_status);
                self.emit_task_ledger_events(runtime_task_id, *task_ledger_offset);
                self.emit_conversation_task_report(
                    runtime_task_id,
                    verification_route.clone(),
                    reason.clone(),
                    false,
                    true,
                    Some(false),
                    VerificationDecision::Failed {
                        reason: reason.clone(),
                    },
                    Some(classification),
                    Some(outcome),
                    Some(action_execution),
                );
                self.record_turn_failed(iterations, &RuntimeError::new(reason.clone()));
                TurnFlow::Fail(RuntimeError::new(reason))
            }
            VerificationDecision::Required(request) => {
                let result = VerificationResult {
                    task_id: runtime_task_id.to_string(),
                    passed: false,
                    observed_green_level: None,
                    summary: format!(
                        "verification required before completion: {:?}",
                        request.policy
                    ),
                    evidence: request.acceptance_tests,
                };
                let _ = self
                    .task_registry
                    .record_verification(runtime_task_id, result);
                self.emit_task_ledger_events(runtime_task_id, *task_ledger_offset);
                *task_ledger_offset = self.task_registry.ledger_for_task(runtime_task_id).len();
                let _ = self
                    .task_registry
                    .set_status(runtime_task_id, crate::TaskStatus::Failed);
                self.emit_task_ledger_events(runtime_task_id, *task_ledger_offset);
                let error = RuntimeError::new("verification is required before task completion");
                self.emit_conversation_task_report(
                    runtime_task_id,
                    verification_route.clone(),
                    "verification remained required".to_string(),
                    false,
                    true,
                    Some(false),
                    VerificationDecision::Failed {
                        reason: "verification is required before task completion".to_string(),
                    },
                    None,
                    None,
                    None,
                );
                self.record_turn_failed(iterations, &error);
                TurnFlow::Fail(error)
            }
            VerificationDecision::NotRequired | VerificationDecision::Passed => {
                let _ = self
                    .task_registry
                    .set_status(runtime_task_id, crate::TaskStatus::Completed);
                self.emit_task_ledger_events(runtime_task_id, *task_ledger_offset);
                self.emit_conversation_task_report(
                    runtime_task_id,
                    verification_route,
                    "conversation turn completed".to_string(),
                    true,
                    false,
                    Some(true),
                    verification_decision,
                    None,
                    None,
                    None,
                );
                TurnFlow::Complete
            }
        }
    }

    fn finalize_turn_as_blocked(
        &mut self,
        runtime_task_id: &str,
        task_ledger_offset: &mut usize,
        reason: String,
    ) {
        let verification_route =
            self.select_model_route_for_task(runtime_task_id, crate::ModelRoutePhase::Verification);
        let verification_decision = VerificationDecision::Failed {
            reason: reason.clone(),
        };
        let mut team_ledger = TeamExecutionLedger::new(
            format!("team-{}", self.session.session_id),
            runtime_task_id.to_string(),
        );
        self.emit_team_execution_event_for_task(
            runtime_task_id,
            team_ledger
                .record_verification(&verification_decision, Some(verification_route.clone())),
        );
        let _ = self
            .task_registry
            .set_status(runtime_task_id, crate::TaskStatus::Blocked);
        self.emit_task_ledger_events(runtime_task_id, *task_ledger_offset);
        *task_ledger_offset = self.task_registry.ledger_for_task(runtime_task_id).len();
        let classification = self.failure_classifier.classify_reason(&reason);
        self.emit_conversation_task_report(
            runtime_task_id,
            verification_route,
            reason,
            false,
            true,
            Some(false),
            verification_decision,
            Some(classification),
            None,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_conversation_task_report(
        &self,
        runtime_task_id: &str,
        verification_route: ModelRouteDecision,
        message: String,
        completed: bool,
        blocked: bool,
        verification_passed: Option<bool>,
        verification_decision: VerificationDecision,
        failure: Option<FailureClassification>,
        recovery: Option<RecoveryOrchestratorOutcome>,
        recovery_action: Option<RecoveryActionExecution>,
    ) {
        let Some(task) = self.task_registry.get(runtime_task_id) else {
            return;
        };
        let verification_result = task.verification_result.clone();
        let outcome = TaskExecutionOutcome {
            task_id: runtime_task_id.to_string(),
            steps: vec![TaskExecutionStep {
                task_id: runtime_task_id.to_string(),
                node_id: None,
                kind: if completed {
                    TaskExecutionStepKind::CompleteTask
                } else {
                    TaskExecutionStepKind::Blocked
                },
                message: message.clone(),
            }],
            completed,
            blocked,
            message,
        };
        let report = crate::task_execution_report_from_parts(
            runtime_task_id,
            outcome.clone(),
            verification_result,
            verification_decision,
            failure,
            recovery,
            recovery_action,
            &task,
        );
        let _ = self
            .task_registry
            .record_task_execution_report(runtime_task_id, report.clone());
        self.emit_runtime_event(RuntimeEvent::TaskExecution(outcome));
        self.emit_runtime_event(RuntimeEvent::TaskExecutionReport(Box::new(report.clone())));

        let mut route = verification_route;
        let verification_passed = verification_passed.unwrap_or(matches!(
            report.verification_decision,
            VerificationDecision::NotRequired | VerificationDecision::Passed
        ));
        route.reason = format!(
            "{}; verification_passed={verification_passed}",
            route.reason
        );
        let engine =
            TaskExecutionEngine::new(self.task_registry.clone(), self.verification_runner.clone());
        let _ = engine.record_execution_route_feedback_for_route(&report, Some(route));
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

        // Borrow the fields the summarizer needs disjointly from `self.session`
        // (which `compact_session_with` borrows) so the closure can call the
        // model while compaction reads the session.
        let api_client = &mut self.api_client;
        let summarization_route = self
            .model_router
            .select(crate::ModelRoutePhase::Summarization);
        let result = compact_session_with(
            &self.session,
            CompactionConfig {
                max_estimated_tokens: 0,
                ..CompactionConfig::default()
            },
            |removed| model_summarize_removed(api_client, &summarization_route, removed),
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

    fn build_reasoning_context(
        &self,
        chain_of_thought: Option<&ChainOfThought>,
    ) -> ReasoningContext {
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

        if let Some(language) = latest_language_preference(&LongTermMemory::load_for_workspace(
            self.session.workspace_root(),
        )) {
            constraints.push(format!("output-language:{language}"));
        }

        constraints
    }

    fn build_decisioning_tools(
        &self,
        user_input: &str,
        pending_tool_uses: &[(String, String, String)],
    ) -> Vec<Tool> {
        let mut tools = BTreeMap::new();

        for tool in self.tool_executor.available_tools() {
            tools.entry(tool.name.clone()).or_insert(tool);
        }

        for (_, tool_name, _) in pending_tool_uses {
            tools
                .entry(tool_name.clone())
                .or_insert_with(|| tool_from_profile(tool_name, None, None));
        }

        let recent_history = self.collect_recent_tool_history();
        for tool in tools.values_mut() {
            let matching_history = recent_history
                .iter()
                .filter(|entry| entry.tool_name == tool.name)
                .collect::<Vec<_>>();
            let successful_count = matching_history
                .iter()
                .filter(|entry| entry.succeeded)
                .count() as f32;
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
                    + (historical_latency_ms as f32 * 0.4))
                    as u32;
            }
        }

        if user_requests_current_workspace_analysis(user_input) {
            for tool in tools.values_mut() {
                if is_workspace_evidence_tool(&tool.name) {
                    tool.capabilities.extend([
                        "workspace".to_string(),
                        "evidence".to_string(),
                        "source-analysis".to_string(),
                    ]);
                    tool.capabilities.sort();
                    tool.capabilities.dedup();
                    tool.avg_success_rate = (tool.avg_success_rate + 0.20).min(0.99);
                    tool.cost = (tool.cost - 0.10).max(0.0);
                }
            }
        }

        if user_requests_document_generation(user_input) && !is_super_ppt_request(user_input) {
            for tool in tools.values_mut() {
                if is_generate_file_tool(&tool.name) {
                    tool.capabilities.extend([
                        "generatefile".to_string(),
                        "document".to_string(),
                        "document-generation".to_string(),
                        "ppt".to_string(),
                        "pptx".to_string(),
                        "slides".to_string(),
                        "deck".to_string(),
                        "file".to_string(),
                    ]);
                    tool.capabilities.sort();
                    tool.capabilities.dedup();
                    tool.avg_success_rate = (tool.avg_success_rate + 0.25).min(0.99);
                    tool.cost = (tool.cost - 0.10).max(0.0);
                }
            }
        }

        tools.into_values().collect()
    }

    fn build_decisioning_task(
        &self,
        task_id: &str,
        user_input: &str,
        pending_tool_uses: &[(String, String, String)],
        reasoning_context: &ReasoningContext,
    ) -> Task {
        let mut required_capabilities = if pending_tool_uses.is_empty() {
            infer_prompt_capabilities(user_input)
        } else if user_requests_current_workspace_analysis(user_input) {
            vec![
                "search".to_string(),
                "read".to_string(),
                "file".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "workspace".to_string(),
                "evidence".to_string(),
                "source-analysis".to_string(),
            ]
        } else {
            pending_tool_uses
                .iter()
                .flat_map(|(_, tool_name, _)| infer_tool_capabilities(tool_name, None))
                .collect::<Vec<_>>()
        };
        required_capabilities.sort();
        required_capabilities.dedup();

        Task::new(
            task_id.to_string(),
            user_input.to_string(),
            if pending_tool_uses.is_empty() {
                required_capabilities.len().clamp(1, 5)
            } else {
                pending_tool_uses.len().clamp(1, 5)
            } as u8,
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

    fn build_initial_decisioning_plan(
        &mut self,
        task_id: &str,
        user_input: &str,
    ) -> Option<DecisioningTurnPlan> {
        if !self.decisioning_config.enabled() {
            return None;
        }

        let reasoning_context = self.build_reasoning_context(None);
        let decisioning_task =
            self.build_decisioning_task(task_id, user_input, &[], &reasoning_context);
        let decisioning_engine = self.build_decisioning_engine(
            self.build_decisioning_tools(user_input, &[]),
            reasoning_context,
        );
        let snapshot = decisioning_engine.analyze(&decisioning_task);
        let dag = crate::build_plan_dag(&snapshot.task, &snapshot.plan, &snapshot.selected_tools);
        let execution = PlanExecution::new(&dag);
        let mut next_execution_event_offset = 0;
        if self.decisioning_config.emit_events() {
            self.record_decisioning_snapshot(&snapshot);
            self.emit_decisioning_events(&snapshot);
            self.emit_plan_execution_events(&execution, next_execution_event_offset);
            next_execution_event_offset = execution.events.len();
        }
        let _ = self
            .task_registry
            .record_plan(task_id, dag.clone(), execution.clone());

        let selected_positions = snapshot
            .selected_tools
            .iter()
            .enumerate()
            .map(|(index, tool)| (tool.name.clone(), index))
            .collect::<BTreeMap<_, _>>();

        // Difficulty gate: for complex tasks, enrich the heuristic plan with a
        // real model-driven planning pass via the Planning route. The heuristic
        // plan remains the base; the model output is advisory guidance only.
        let model_planning_guidance =
            self.maybe_model_plan(task_id, user_input, &decisioning_task, &snapshot);

        Some(DecisioningTurnPlan {
            engine: decisioning_engine,
            task: decisioning_task,
            snapshot,
            dag,
            execution,
            next_execution_event_offset,
            selected_positions,
            workspace_evidence_stage_active: user_requests_current_workspace_analysis(user_input),
            model_planning_guidance,
        })
    }

    /// When the task is complex enough (per `planning_complexity_threshold`),
    /// ask the model (Planning route) for a short, concrete plan to augment the
    /// heuristic decomposition. Returns `None` when the gate is disabled, the
    /// task is below threshold, or the model call fails — in every case the
    /// heuristic plan still stands on its own.
    fn maybe_model_plan(
        &mut self,
        task_id: &str,
        user_input: &str,
        task: &Task,
        snapshot: &DecisioningSnapshot,
    ) -> Option<String> {
        let threshold = self.decisioning_config.planning_complexity_threshold();
        if threshold == 0 || task.complexity < threshold {
            return None;
        }
        let route = self.select_model_route_for_task(task_id, crate::ModelRoutePhase::Planning);
        let heuristic_steps = snapshot
            .plan
            .steps
            .iter()
            .map(|step| format!("- {}: {}", step.id, step.title))
            .collect::<Vec<_>>()
            .join("\n");
        let memory_context = self.task_memory_planning_context(task_id);
        let memory_block = memory_context
            .as_ref()
            .map(|context| format!("\n\nMemory-informed planning context:\n{context}"))
            .unwrap_or_default();
        let request = ApiRequest {
            system_prompt: vec![PLANNING_SYSTEM_PROMPT.to_string()],
            messages: vec![ConversationMessage::user_text(format!(
                "Task (complexity {}/5): {user_input}\n\nA heuristic pre-plan proposed these steps:\n{heuristic_steps}{memory_block}\n\nProduce a short, concrete execution plan (3-7 ordered steps) that improves on the heuristic where useful. Note risks and the recommended order. Be specific to this task.",
                task.complexity,
            ))],
            model_route: Some(route),
        };
        let events = self.api_client.stream(request).ok()?;
        let mut text = String::new();
        for event in events {
            if let AssistantEvent::TextDelta(delta) = event {
                text.push_str(&delta);
            }
        }
        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    }

    /// When structured execution is eligible, ask the planning model for a
    /// machine-readable plan (JSON), validate it, and record the resulting DAG.
    /// Returns the validated plan so Stage 2 can dispatch it. On any failure
    /// (gate off, below threshold, malformed/invalid JSON) returns `None` and
    /// the turn proceeds on the heuristic plan / single-shot loop.
    fn maybe_build_structured_plan(
        &mut self,
        task_id: &str,
        user_input: &str,
        task: &Task,
    ) -> Option<crate::structured_execution::ValidatedStructuredPlan> {
        if !self
            .decisioning_config
            .uses_structured_execution(task.complexity)
        {
            return None;
        }
        let route = self.select_model_route_for_task_with_complexity(
            task_id,
            crate::ModelRoutePhase::Planning,
            Some(task.complexity),
        );
        let memory_context = self.task_memory_planning_context(task_id);
        let memory_block = memory_context
            .as_ref()
            .map(|context| format!("\n\nMemory-informed planning context:\n{context}"))
            .unwrap_or_default();
        let request = ApiRequest {
            system_prompt: vec![STRUCTURED_PLAN_SYSTEM_PROMPT.to_string()],
            messages: vec![ConversationMessage::user_text(format!(
                "Task (complexity {}/5): {user_input}{memory_block}\n\nReturn ONLY a JSON object: {{\"steps\":[{{\"id\":\"kebab-id\",\"title\":\"...\",\"depends_on\":[\"other-id\"],\"parallelizable\":false,\"estimated_effort\":1,\"acceptance\":[\"shell command that must pass\"],\"capabilities\":[\"read\"]}}],\"notes\":[\"...\"]}}. 2-7 steps. depends_on must reference declared ids only. No prose outside the JSON.",
                task.complexity,
            ))],
            model_route: Some(route),
        };
        let events = self.api_client.stream(request).ok()?;
        let mut text = String::new();
        for event in events {
            if let AssistantEvent::TextDelta(delta) = event {
                text.push_str(&delta);
            }
        }
        let json = extract_json_object(&text)?;
        let validated = crate::structured_execution::parse_structured_plan(task_id, &json)?;
        // Record the structured DAG so it is observable and persisted; Stage 2
        // dispatches it.
        let dag =
            crate::structured_execution::build_structured_dag(task_id, user_input, &validated);
        let execution = PlanExecution::new(&dag);
        let _ = self.task_registry.record_plan(task_id, dag, execution);
        Some(validated)
    }

    /// Stage 2: dispatch a validated structured plan node-by-node in dependency
    /// order. Each node runs one focused model sub-turn; the per-node outcomes
    /// are recorded as plan-execution events and summarized back into a report
    /// the main turn uses to finish. Returns `None` if nothing was dispatched.
    fn dispatch_structured_plan(
        &mut self,
        task_id: &str,
        user_input: &str,
        validated: &crate::structured_execution::ValidatedStructuredPlan,
    ) -> Option<String> {
        let dag = crate::structured_execution::build_structured_dag(task_id, user_input, validated);
        let mut execution = PlanExecution::new(&dag);
        let mut event_offset = 0usize;
        let mut reports: Vec<String> = Vec::new();

        // Per-node acceptance criteria (id -> shell commands) for Stage 3
        // per-node verification.
        let acceptance: std::collections::HashMap<String, Vec<String>> =
            validated.acceptance.iter().cloned().collect();
        let max_node_recovery = self.max_recovery_attempts;
        let team_convergence = self.decisioning_config.clone();

        // Bound total node dispatches as a runaway guard.
        let max_nodes = dag.nodes.len().saturating_add(1);
        let outcome =
            crate::structured_execution::dispatch_plan(&dag, &mut execution, max_nodes, |node| {
                let node_acceptance = acceptance.get(&node.id).cloned().unwrap_or_default();
                // High-effort nodes run the multi-role convergence loop; others
                // run the single-Executor verified path from Stage 3.
                let result = if team_convergence.uses_team_convergence(node.estimated_effort) {
                    self.execute_structured_node_with_team(
                        task_id,
                        user_input,
                        node,
                        &node_acceptance,
                        max_node_recovery,
                    )
                } else {
                    self.execute_structured_node_verified(
                        task_id,
                        user_input,
                        node,
                        &node_acceptance,
                        max_node_recovery,
                    )
                };
                match result {
                    NodeRunResult::Succeeded { summary } => {
                        reports.push(format!("- {} ({}): {}", node.id, node.title, summary));
                        let artifact = crate::NodeExecutionArtifact::new(&node.id, summary.clone())
                            .with_evidence(vec![
                                "structured node sub-turn completed".to_string(),
                                if node_acceptance.is_empty() {
                                    "acceptance skipped: no node command".to_string()
                                } else {
                                    "acceptance satisfied".to_string()
                                },
                            ])
                            .with_commands_run(node_acceptance.clone())
                            .with_confidence_percent(if node_acceptance.is_empty() {
                                60
                            } else {
                                90
                            })
                            .with_producer("structured-node");
                        crate::structured_execution::NodeExecutionResult::success_with_artifact(
                            summary, artifact,
                        )
                    }
                    NodeRunResult::Failed { reason } => {
                        reports.push(format!(
                            "- {} ({}): FAILED — {}",
                            node.id, node.title, reason
                        ));
                        let artifact = crate::NodeExecutionArtifact::new(&node.id, reason.clone())
                            .with_evidence(vec!["structured node failed".to_string()])
                            .with_commands_run(node_acceptance.clone())
                            .with_blocking_reason(reason.clone())
                            .with_confidence_percent(0)
                            .with_producer("structured-node");
                        crate::structured_execution::NodeExecutionResult::failure_with_artifact(
                            reason, artifact,
                        )
                    }
                }
            });

        // Emit the plan-execution events the dispatch produced and persist the
        // final execution state.
        self.emit_plan_execution_events(&execution, event_offset);
        event_offset = execution.events.len();
        let _ = event_offset;
        let _ = self
            .task_registry
            .record_plan(task_id, dag.clone(), execution);

        if outcome.dispatched == 0 {
            return None;
        }
        let mut report = format!(
            "# Structured execution results\n{} of {} node(s) completed, {} failed. Use these results to finish the task; do not redo completed work.\n",
            outcome.completed.len(),
            outcome.dispatched,
            outcome.failed.len(),
        );
        report.push_str(&reports.join("\n"));
        Some(report)
    }

    /// Run a single structured-plan node with bounded local re-drive: execute
    /// a focused model sub-turn, verify the node's acceptance commands (when it
    /// declared any and the permission mode allows running them), and on
    /// verification failure re-drive the node with the failure detail up to
    /// `max_recovery` times before failing it. This extends the P0 fix-verify
    /// loop to node granularity so a failing node is repaired without aborting
    /// the whole plan.
    fn execute_structured_node_verified(
        &mut self,
        task_id: &str,
        user_input: &str,
        node: &crate::structured_execution::ExecutionNode,
        acceptance: &[String],
        max_recovery: usize,
    ) -> NodeRunResult {
        let mut attempt = 0usize;
        let mut last_failure = String::from("node produced no output");
        loop {
            let extra = if attempt == 0 {
                None
            } else {
                Some(format!(
                    "A previous attempt did not satisfy the acceptance criteria: {last_failure}. Fix the root cause for this step only."
                ))
            };
            let Some((summary, route)) =
                self.run_structured_node_turn(task_id, user_input, node, extra)
            else {
                last_failure = "node produced no output".to_string();
                if attempt >= max_recovery {
                    return NodeRunResult::Failed {
                        reason: last_failure,
                    };
                }
                attempt += 1;
                continue;
            };

            // Verify acceptance commands when present and permitted.
            match self.verify_node_acceptance(task_id, node, acceptance) {
                NodeVerifyOutcome::Passed | NodeVerifyOutcome::Skipped => {
                    return NodeRunResult::Succeeded { summary };
                }
                NodeVerifyOutcome::Failed { reason } => {
                    last_failure = reason.clone();
                    if attempt >= max_recovery {
                        return NodeRunResult::Failed { reason };
                    }
                    // Record per-node route failure feedback so the next
                    // attempt's route selection can adaptively escalate this
                    // node to a higher-quality route (difficulty-aware routing).
                    let _ = self.task_registry.update_latest_route_feedback(
                        task_id,
                        crate::ModelRouteFeedback::pending(
                            task_id.to_string(),
                            route,
                            current_time_millis() / 1_000,
                        )
                        .with_outcome(
                            false,
                            Some(false),
                            true,
                            Some(reason),
                        ),
                    );
                    attempt += 1;
                }
            }
        }
    }

    /// Run one focused model sub-turn for a node, optionally with extra
    /// re-drive guidance. Returns the node's text output and the route used
    /// (so the caller can record per-node route feedback for adaptive
    /// escalation), or `None` if the output is empty.
    fn run_structured_node_turn(
        &mut self,
        task_id: &str,
        user_input: &str,
        node: &crate::structured_execution::ExecutionNode,
        extra_guidance: Option<String>,
    ) -> Option<(String, ModelRouteDecision)> {
        let route = self.select_model_route_for_task_with_complexity(
            task_id,
            crate::ModelRoutePhase::Coding,
            Some(node.estimated_effort.max(1)),
        );
        let deps = if node.depends_on.is_empty() {
            "none".to_string()
        } else {
            node.depends_on.join(", ")
        };
        let mut prompt = format!(
            "Overall task: {user_input}\n\nFocus only on this step:\n- id: {}\n- goal: {}\n- depends on: {deps}\n\nProduce the concrete result/output for this step in a few sentences.",
            node.id, node.title
        );
        if let Some(extra) = extra_guidance {
            prompt.push_str("\n\n");
            prompt.push_str(&extra);
        }
        let request = ApiRequest {
            system_prompt: vec![STRUCTURED_NODE_SYSTEM_PROMPT.to_string()],
            messages: vec![ConversationMessage::user_text(prompt)],
            model_route: Some(route.clone()),
        };
        let events = self.api_client.stream(request).ok()?;
        let mut text = String::new();
        for event in events {
            if let AssistantEvent::TextDelta(delta) = event {
                text.push_str(&delta);
            }
        }
        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some((text.to_string(), route))
        }
    }

    /// Verify a node's acceptance commands. Skipped when the node declared none
    /// or when the permission mode forbids running verification commands.
    fn verify_node_acceptance(
        &self,
        task_id: &str,
        node: &crate::structured_execution::ExecutionNode,
        acceptance: &[String],
    ) -> NodeVerifyOutcome {
        if acceptance.is_empty() {
            return NodeVerifyOutcome::Skipped;
        }
        if !matches!(
            self.permission_policy.active_mode(),
            crate::PermissionMode::DangerFullAccess | crate::PermissionMode::Allow
        ) {
            // Cannot run commands in this mode; treat as skipped so the node is
            // accepted on its model output (matches turn-level gating behavior).
            return NodeVerifyOutcome::Skipped;
        }
        let request = crate::VerificationRequest {
            task_id: format!("{task_id}:{}", node.id),
            objective: node.title.clone(),
            scope: "structured node".to_string(),
            acceptance_tests: acceptance.to_vec(),
            reporting_contract: "node acceptance".to_string(),
            policy: crate::VerificationPolicy::Targeted,
            required_green_level: None,
        };
        let result = self.verification_runner.run(&request);
        if result.passed {
            NodeVerifyOutcome::Passed
        } else {
            NodeVerifyOutcome::Failed {
                reason: result.summary,
            }
        }
    }

    fn format_initial_decisioning_prompt(plan: &DecisioningTurnPlan) -> String {
        let selected_tools = plan
            .snapshot
            .selected_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        let steps = plan
            .snapshot
            .plan
            .steps
            .iter()
            .take(8)
            .map(|step| {
                let tools = if step.candidate_tools.is_empty() {
                    "no preferred tool".to_string()
                } else {
                    step.candidate_tools.join(", ")
                };
                format!("- {}: {} (tools: {tools})", step.id, step.title)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let selected_summary = if selected_tools.is_empty() {
            "none".to_string()
        } else {
            selected_tools.join(", ")
        };
        let mut prompt = format!(
            "# Advisory task plan\nThis capability-aware plan was generated before the first model step. Treat it as guidance, not permission escalation. Follow workspace evidence and permission requirements before answering.\n- Task id: {}\n- Execution mode: {}\n- Confidence: {:.0}%\n- Safety outcome: {:?} (risk {:.0}%)\n- Preferred tools: {}\n\n{}",
            plan.task.id,
            plan.snapshot.plan.execution_mode.label(),
            plan.snapshot.plan.confidence * 100.0,
            plan.snapshot.risk.outcome,
            plan.snapshot.risk.score * 100.0,
            selected_summary,
            steps,
        );
        if let Some(guidance) = &plan.model_planning_guidance {
            prompt.push_str(
                "\n\n## Model planning guidance (high-complexity task)\nThe following plan was produced by the planning model. Use it to refine your approach; still gather evidence and respect permissions.\n",
            );
            prompt.push_str(guidance);
        }
        prompt
    }

    fn build_decisioning_turn_plan(
        &self,
        task_id: &str,
        user_input: &str,
        chain_of_thought: Option<&ChainOfThought>,
        pending_tool_uses: &[(String, String, String)],
        workspace_evidence_stage_active: bool,
    ) -> Option<DecisioningTurnPlan> {
        if !self.decisioning_config.enabled() || pending_tool_uses.is_empty() {
            return None;
        }

        let reasoning_context = self.build_reasoning_context(chain_of_thought);
        let decisioning_task =
            self.build_decisioning_task(task_id, user_input, pending_tool_uses, &reasoning_context);
        let decisioning_engine = self.build_decisioning_engine(
            self.build_decisioning_tools(user_input, pending_tool_uses),
            reasoning_context,
        );
        let snapshot = decisioning_engine.analyze(&decisioning_task);
        let dag = crate::build_plan_dag(&snapshot.task, &snapshot.plan, &snapshot.selected_tools);
        let execution = PlanExecution::new(&dag);
        let mut next_execution_event_offset = 0;
        if self.decisioning_config.emit_events() {
            self.record_decisioning_snapshot(&snapshot);
            self.emit_decisioning_events(&snapshot);
            self.emit_plan_execution_events(&execution, next_execution_event_offset);
            next_execution_event_offset = execution.events.len();
        }
        let _ = self
            .task_registry
            .record_plan(task_id, dag.clone(), execution.clone());

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
            dag,
            execution,
            next_execution_event_offset,
            selected_positions,
            workspace_evidence_stage_active: workspace_evidence_stage_active
                && user_requests_current_workspace_analysis(user_input),
            // Per-iteration plans rely on the heuristic; model planning runs
            // once up front in build_initial_decisioning_plan.
            model_planning_guidance: None,
        })
    }

    fn record_structured_feasibility(
        &self,
        task_id: &str,
        feasibility: &crate::structured_execution::StructuredFeasibility,
    ) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };
        let mut attributes = Map::new();
        attributes.insert("task_id".to_string(), Value::String(task_id.to_string()));
        attributes.insert(
            "eligible".to_string(),
            Value::Bool(feasibility.is_eligible()),
        );
        attributes.insert("reason".to_string(), Value::String(feasibility.reason()));
        session_tracer.record("structured_execution_feasibility", attributes);
    }

    fn record_decisioning_snapshot(&self, snapshot: &DecisioningSnapshot) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert(
            "task_id".to_string(),
            Value::String(snapshot.task.id.clone()),
        );
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
        for event in &snapshot.events {
            if let Some(reporter) = &self.decisioning_event_reporter {
                reporter.emit_decisioning_event(event);
            }
            self.emit_runtime_event(RuntimeEvent::Decisioning(Box::new(event.clone())));
        }
    }

    fn emit_plan_execution_events(&self, execution: &PlanExecution, offset: usize) {
        if !self.decisioning_config.emit_events() {
            return;
        }

        for event in execution.events_since(offset) {
            if let Some(reporter) = &self.plan_execution_event_reporter {
                reporter.emit_plan_execution_event(event);
            }
            self.emit_runtime_event(RuntimeEvent::PlanExecution(event.clone()));
        }
        if offset < execution.events.len() {
            if let Some(snapshot) = self
                .task_registry
                .get(&execution.task_id)
                .and_then(|task| task.plan)
            {
                let _ = self.task_registry.record_plan(
                    &execution.task_id,
                    snapshot.dag,
                    execution.clone(),
                );
            }
        }
    }

    fn emit_runtime_event(&self, event: RuntimeEvent) {
        if let Some(reporter) = &self.runtime_event_reporter {
            reporter.emit_runtime_event(&event);
        }
    }

    fn emit_task_ledger_events(&self, task_id: &str, offset: usize) {
        for event in self
            .task_registry
            .ledger_for_task(task_id)
            .into_iter()
            .skip(offset)
        {
            if let Some(reporter) = &self.task_ledger_event_reporter {
                reporter.emit_task_ledger_event(&event);
            }
            self.emit_runtime_event(RuntimeEvent::TaskLedger(event));
        }
    }

    fn record_and_emit_task_progress(
        &self,
        task_id: &str,
        task_ledger_offset: &mut usize,
        event: &str,
        message: Option<String>,
    ) {
        let _ = self
            .task_registry
            .record_progress_event(task_id, event, message);
        self.emit_task_ledger_events(task_id, *task_ledger_offset);
        *task_ledger_offset = self.task_registry.ledger_for_task(task_id).len();
    }

    fn enrich_latest_route_feedback_from_usage(
        &self,
        task_id: &str,
        usage: TokenUsage,
        latency_ms: u32,
        succeeded: Option<bool>,
    ) {
        if usage.total_tokens() == 0 && succeeded.is_none() {
            return;
        }
        let route = self.task_registry.get(task_id).and_then(|task| {
            task.route_feedback
                .last()
                .map(|feedback| feedback.route.clone())
        });
        let Some(route) = route else {
            return;
        };
        let input_tokens = usage
            .input_tokens
            .saturating_add(usage.cache_creation_input_tokens)
            .saturating_add(usage.cache_read_input_tokens);
        let cost = usage.estimate_cost_usd().total_cost_usd();
        let mut feedback = crate::ModelRouteFeedback::pending(
            task_id.to_string(),
            route,
            current_time_millis() / 1_000,
        )
        .with_metrics(
            (latency_ms > 0).then_some(latency_ms),
            (input_tokens > 0).then_some(u64::from(input_tokens)),
            (usage.output_tokens > 0).then_some(u64::from(usage.output_tokens)),
            (cost > 0.0).then_some(cost),
        );
        if let Some(succeeded) = succeeded {
            feedback.succeeded = Some(succeeded);
        }
        let _ = self
            .task_registry
            .update_latest_route_feedback(task_id, feedback);
    }

    fn select_model_route_for_task(
        &self,
        task_id: &str,
        phase: crate::ModelRoutePhase,
    ) -> ModelRouteDecision {
        self.select_model_route_for_task_with_complexity(task_id, phase, None)
    }

    fn task_memory_planning_context(&self, task_id: &str) -> Option<String> {
        let task = self.task_registry.get(task_id)?;
        let store = crate::TaskMemoryStore::from_tasks(&self.task_registry.list(None));
        let context = store.context_for_task(&task);
        let has_specific_memory = context.similar_count > 0
            || !context.successful_acceptance_tests.is_empty()
            || !context.common_failure_classes.is_empty()
            || context.current_handoff_summary.is_some()
            || !context.current_node_handoffs.is_empty()
            || context
                .recovery_actions
                .iter()
                .any(|action| action.total > 0);
        if !has_specific_memory {
            return None;
        }
        let mut lines = vec![format!(
            "- Task memory type: {} ({} similar task(s))",
            context.task_type, context.similar_count
        )];
        if let Some(summary) = context.current_handoff_summary.as_ref() {
            lines.push(format!("- Current task handoff: {summary}"));
        }
        if let Some(next_node) = context.next_node_hint.as_ref() {
            lines.push(format!("- Resume from next node: {next_node}"));
        }
        if !context.current_node_handoffs.is_empty() {
            let handoffs = context
                .current_node_handoffs
                .iter()
                .rev()
                .take(5)
                .filter_map(|artifact| {
                    artifact.summary.as_ref().map(|summary| {
                        format!(
                            "{} [{}; confidence={}]: {}",
                            artifact.node_id,
                            artifact.status,
                            artifact
                                .confidence_percent
                                .map_or_else(|| "n/a".to_string(), |value| value.to_string()),
                            summary
                        )
                    })
                })
                .collect::<Vec<_>>();
            if !handoffs.is_empty() {
                lines.push(format!(
                    "- Completed/current node artifacts: {}",
                    handoffs.join("; ")
                ));
            }
        }
        if !context.successful_acceptance_tests.is_empty() {
            lines.push(format!(
                "- Reuse or adapt successful acceptance tests: {}",
                context
                    .successful_acceptance_tests
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        if !context.common_failure_classes.is_empty() {
            let failures = context
                .common_failure_classes
                .iter()
                .map(|(class, count)| format!("{class}={count}"))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("- Pre-check recurring failure classes: {failures}"));
        }
        let recovery = context
            .recovery_actions
            .iter()
            .take(3)
            .map(|action| {
                format!(
                    "{} {:.0}% success ({} total, {} blocked)",
                    action.kind,
                    action.success_rate * 100.0,
                    action.total,
                    action.blocked
                )
            })
            .collect::<Vec<_>>();
        if !recovery.is_empty() {
            lines.push(format!(
                "- Prefer historically effective recovery actions: {}",
                recovery.join("; ")
            ));
        }
        if let Some(rate) = context.route_failure_rate {
            lines.push(format!(
                "- Similar route failure rate: {:.0}%",
                rate * 100.0
            ));
        }
        if !context.recommendations.is_empty() {
            lines.push(format!(
                "- Memory recommendations: {}",
                context.recommendations.join(" ")
            ));
        }
        Some(lines.join("\n"))
    }

    fn select_model_route_for_task_with_complexity(
        &self,
        task_id: &str,
        phase: crate::ModelRoutePhase,
        complexity: Option<u8>,
    ) -> ModelRouteDecision {
        let mut feedback = self.workspace_route_feedback.clone();
        let task = self.task_registry.get(task_id);
        let task_type = task.as_ref().map(crate::TaskMemoryStore::task_type_for);
        feedback.extend(
            task.as_ref()
                .map(|task| task.route_feedback.clone())
                .unwrap_or_default(),
        );
        let decision = self.model_router.select_with_task_feedback(
            phase,
            &feedback,
            complexity,
            task_type.as_deref(),
        );
        if let Some(reporter) = &self.model_route_event_reporter {
            reporter.emit_model_route_event(&decision);
        }
        self.emit_runtime_event(RuntimeEvent::ModelRoute(decision.clone()));
        let _ = self.task_registry.record_route_feedback(
            task_id,
            crate::ModelRouteFeedback::pending(
                task_id.to_string(),
                decision.clone(),
                current_time_millis() / 1_000,
            )
            .with_task_type(task_type),
        );
        decision
    }

    fn emit_team_execution_event(&self, event: TeamExecutionEvent) {
        if let Some(reporter) = &self.team_execution_event_reporter {
            reporter.emit_team_execution_event(&event);
        }
        self.emit_runtime_event(RuntimeEvent::TeamExecution(event));
    }

    fn emit_team_execution_event_for_task(&self, task_id: &str, event: TeamExecutionEvent) {
        let _ = self.task_registry.record_team_event(task_id, event.clone());
        self.emit_team_execution_event(event);
    }

    fn evaluate_runtime_task_completion(&self, task_id: &str) -> VerificationDecision {
        TaskExecutionEngine::new(self.task_registry.clone(), self.verification_runner.clone())
            .recorded_completion_decision(task_id)
            .unwrap_or_else(|reason| VerificationDecision::Failed { reason })
    }

    fn fail_turn_with_task_status(
        &mut self,
        runtime_task_id: &str,
        task_ledger_offset: &mut usize,
        iterations: usize,
        error: RuntimeError,
    ) -> RuntimeError {
        let _ = self
            .task_registry
            .set_status(runtime_task_id, crate::TaskStatus::Failed);
        self.emit_task_ledger_events(runtime_task_id, *task_ledger_offset);
        *task_ledger_offset = self.task_registry.ledger_for_task(runtime_task_id).len();
        self.record_turn_failed(iterations, &error);
        error
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
            plan_dag: Some(crate::build_plan_dag(
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
        if plan.workspace_evidence_stage_active && !is_workspace_evidence_tool(tool_name) {
            return Some((
                PermissionOverride::Deny,
                format!(
                    "Decisioning evidence stage requires workspace discovery/search/read tools before {tool_name}; gather local source evidence first."
                ),
            ));
        }

        if !plan.selected_positions.contains_key(tool_name) {
            if user_requests_current_workspace_analysis(&plan.task.description)
                && is_workspace_evidence_tool(tool_name)
            {
                return None;
            }
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
    fn reflect_on_outcome(&mut self, chain: &Option<ChainOfThought>, summary: &TurnSummary) {
        // Load or create a workspace-scoped memory store.
        let workspace = self.session.workspace_root();
        let mut memory = LongTermMemory::load_for_workspace(workspace);

        record_reflection_memory(&mut memory, chain.as_ref(), summary);

        // best-effort save (already attempted in add_entry)
        let _ = memory.save();
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

type AssistantBuildResult = (
    ConversationMessage,
    Option<TokenUsage>,
    Vec<PromptCacheEvent>,
    Option<ChainOfThought>,
);

fn build_assistant_message(
    events: Vec<AssistantEvent>,
) -> Result<AssistantBuildResult, RuntimeError> {
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
                flush_text_block(&mut text, &mut blocks);
                if chain.is_none() {
                    chain = Some(ChainOfThought::new());
                }
                if let Some(ref mut c) = chain {
                    c.add_step(step.clone());
                }
                // Store thinking content in session blocks so the API can
                // round-trip reasoning_content. Providers like DeepSeek reject
                // 400 when this is missing.
                match step {
                    ReasoningStep::Analysis {
                        content, signature, ..
                    } => {
                        blocks.push(ContentBlock::Thinking {
                            thinking: content,
                            signature,
                        });
                    }
                    ReasoningStep::RedactedThinking { data } => {
                        blocks.push(ContentBlock::RedactedThinking { data });
                    }
                    _ => {}
                }
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

fn assistant_plain_text(message: &ConversationMessage) -> String {
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
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
        build_assistant_message, document_generation_long_task_prompt,
        parse_auto_compaction_threshold, user_requests_document_generation, AlternativeApproach,
        ApiClient, ApiRequest, AssistantEvent, AutoCompactionEvent, ChainOfThought,
        ConversationRuntime, DecisioningEvent, DecisioningEventReporter, LongTermMemory,
        MemoryEntry, MemoryKind, PromptCacheEvent, ReasoningStep, RuntimeError, StaticToolExecutor,
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
    use serde_json::Value;
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

    struct AliasedToolApiClient {
        call_count: usize,
    }

    impl ApiClient for AliasedToolApiClient {
        fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.call_count += 1;
            if self.call_count == 1 {
                return Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "search-1".to_string(),
                        name: "function:google_search".to_string(),
                        input: r#"{"query":"himalaya"}"#.to_string(),
                    },
                    AssistantEvent::MessageStop,
                ]);
            }

            let last_message = request.messages.last().expect("tool result should exist");
            assert!(matches!(
                &last_message.blocks[0],
                ContentBlock::ToolResult { tool_name, is_error: false, .. } if tool_name == "WebSearch"
            ));
            Ok(vec![
                AssistantEvent::TextDelta("searched".to_string()),
                AssistantEvent::MessageStop,
            ])
        }
    }

    #[test]
    fn current_workspace_analysis_request_injects_local_tree_context() {
        struct InspectingApi;
        impl ApiClient for InspectingApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let first_message = request
                    .messages
                    .first()
                    .expect("user message should be present");
                assert_eq!(first_message.role, MessageRole::User);
                let joined = first_message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(joined.contains("[Current workspace context]"));
                assert!(joined.contains("Workspace root:"));
                assert!(joined.contains("Directory tree snapshot:"));
                assert!(joined.contains("Cargo.toml"));
                assert!(joined.contains("src/"));
                assert!(joined.contains("Manifest candidates to read with read_file"));
                assert!(joined.contains("Source candidates to inspect with read_file"));
                assert!(joined.contains("only navigation aid, not analysis evidence"));
                assert!(!joined.contains("fn main()"));
                assert!(joined.contains("Do not ask the user to upload code"));
                Ok(vec![
                    AssistantEvent::TextDelta("已检查当前工程。".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let root = std::env::temp_dir().join(format!(
            "himalaya-workspace-analysis-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).expect("workspace src dir");
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n")
            .expect("manifest should be written");
        fs::write(root.join("src").join("main.rs"), "fn main() {}\n")
            .expect("source should be written");
        fs::create_dir_all(root.join(".Himalaya")).expect("memory dir should exist");
        fs::write(root.join(".Himalaya").join("long_term_memory.json"), "[]")
            .expect("memory should be isolated");

        let session = Session::new().with_workspace_root(&root);
        let mut runtime = ConversationRuntime::new(
            session,
            InspectingApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::ReadOnly),
            vec!["system".to_string()],
        );

        runtime
            .run_turn("请分析当前工程下的源代码目录结构及源代码", None)
            .expect("workspace context should be injected");
        fs::remove_dir_all(root).expect("cleanup workspace");
    }

    #[test]
    fn workspace_analysis_blocks_snapshot_answer_when_evidence_tools_are_available() {
        struct SnapshotAnswerApi;
        impl ApiClient for SnapshotAnswerApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta(
                        "Workspace layout derived from supplied snapshot".to_string(),
                    ),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let root = std::env::temp_dir().join(format!(
            "himalaya-workspace-analysis-blocks-snapshot-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).expect("workspace src dir");
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n")
            .expect("manifest should be written");
        fs::write(root.join("src").join("main.rs"), "fn main() {}\n")
            .expect("source should be written");
        fs::create_dir_all(root.join(".Himalaya")).expect("memory dir should exist");
        fs::write(root.join(".Himalaya").join("long_term_memory.json"), "[]")
            .expect("memory should be isolated");

        let mut runtime = ConversationRuntime::new(
            Session::new().with_workspace_root(&root),
            SnapshotAnswerApi,
            StaticToolExecutor::new()
                .register(
                    "grep_search",
                    |_input| Ok("src/main.rs:fn main".to_string()),
                )
                .register("read_file", |input| Ok(format!("read {input}"))),
            PermissionPolicy::new(PermissionMode::ReadOnly),
            vec!["system".to_string()],
        );

        let error = runtime
            .run_turn("请分析当前工程下的源代码目录结构及源代码", None)
            .expect_err("snapshot-only workspace answer should be blocked");

        assert!(error
            .to_string()
            .contains("workspace analysis required local search/read evidence"));
        assert!(!runtime
            .session()
            .messages
            .iter()
            .any(|message| message.role == MessageRole::Assistant));
        fs::remove_dir_all(root).expect("cleanup workspace");
    }

    #[test]
    fn normalizes_common_function_tool_aliases_before_execution() {
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            AliasedToolApiClient { call_count: 0 },
            StaticToolExecutor::new().register("WebSearch", |input| {
                assert!(input.contains("himalaya"));
                Ok("search results".to_string())
            }),
            PermissionPolicy::new(PermissionMode::ReadOnly)
                .with_tool_requirement("WebSearch", PermissionMode::ReadOnly),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("search", None)
            .expect("aliased tool should execute");

        assert_eq!(summary.tool_results.len(), 1);
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult { tool_name, is_error: false, .. } if tool_name == "WebSearch"
        ));
    }

    #[test]
    fn document_generation_request_redrives_until_generate_file_succeeds() {
        struct DocumentGenerationApiClient {
            call_count: usize,
        }

        impl ApiClient for DocumentGenerationApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.call_count += 1;
                match self.call_count {
                    1 => {
                        let attachment_context = request
                            .system_prompt
                            .iter()
                            .find(|item| item.contains("附件上下文"))
                            .cloned()
                            .unwrap_or_default();
                        assert!(
                            attachment_context.contains("thesis.pdf"),
                            "expected attachment context in system prompt, got: {attachment_context}"
                        );
                        assert!(
                            attachment_context.contains("不要要求用户再次上传附件"),
                            "expected no-reupload contract, got: {attachment_context}"
                        );
                        Ok(vec![
                            AssistantEvent::TextDelta(
                                "这是一篇关于复杂网络瓦解的论文分析。".to_string(),
                            ),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    2 => {
                        let last_user_text = request
                            .messages
                            .last()
                            .and_then(|message| message.blocks.first())
                            .and_then(|block| match block {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        assert!(last_user_text.contains("文档生成完成门禁"));
                        assert!(last_user_text.contains("thesis.pdf"));
                        assert!(last_user_text.contains("不要要求用户再次上传附件"));
                        Ok(vec![
                            AssistantEvent::ToolUse {
                                id: "generate-1".to_string(),
                                name: "function:generate_file".to_string(),
                                input: r#"{"path":"output/defense.pptx","format":"pptx","document_spec":{"title":"集群对抗下的多层复杂网络建模与瓦解方法","blocks":[{"type":"heading","text":"答辩汇报"}]}}"#.to_string(),
                            },
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => Ok(vec![
                        AssistantEvent::TextDelta(
                            "已生成答辩 PPT：output/defense.pptx".to_string(),
                        ),
                        AssistantEvent::MessageStop,
                    ]),
                }
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            DocumentGenerationApiClient { call_count: 0 },
            StaticToolExecutor::new().register("generate_file", |_input| {
                Ok(r#"{"filePath":"output/defense.pptx","format":"pptx","manifestPath":"output/defense.pptx.manifest.json","quality":{"warnings":[],"slideCount":24}}"#.to_string())
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        runtime
            .inject_user_blocks(vec![ContentBlock::Text {
                text: "[File: /tmp/thesis.pdf]\n论文正文：多层复杂网络建模与瓦解方法。".to_string(),
            }])
            .expect("attachment blocks should inject");

        let summary = runtime
            .run_turn(
                "请依据附件中的学位论文，整理生成一份答辩用高质量的ppt电子文档。",
                None,
            )
            .expect("document generation should be re-driven until generate_file succeeds");

        assert!(
            summary.iterations >= 3,
            "expected initial prose, tool call, and final response iterations"
        );
        assert!(summary.tool_results.iter().any(|message| matches!(
            &message.blocks[0],
            ContentBlock::ToolResult {
                tool_name,
                is_error: false,
                ..
            } if tool_name == "generate_file"
        )));
    }

    #[test]
    fn document_generation_direct_tool_use_is_repaired_before_execution() {
        let mut message = ConversationMessage::assistant(vec![ContentBlock::ToolUse {
            id: "doc-1".to_string(),
            name: "mcp__doc-service__generate_pptx".to_string(),
            input: r#"{"path":"/test.pptx","documentSpec":"{\"title\":\"答辩PPT\",\"theme\":\"ocean\",\"blocks\":[{\"type\":\"section\",\"title\":\"研究内容\",\"content\":[{\"type\":\"list\",\"content\":[\"建模\",\"实验\"]}]}],\"generationContract\":{\"fail_on_contract_violation\": True}}"}"#.to_string(),
        }]);
        let mut pending = vec![(
            "doc-1".to_string(),
            "mcp__doc-service__generate_pptx".to_string(),
            match &message.blocks[0] {
                ContentBlock::ToolUse { input, .. } => input.clone(),
                _ => unreachable!(),
            },
        )];
        let attachments = vec![
            super::AttachmentEvidence::text("/tmp/thesis.pdf", Some(88_000))
                .with_detail("chunks: 12")
                .with_detail("fullTextPath: /tmp/thesis.txt"),
        ];

        super::repair_pending_document_generation_tool_uses(
            &mut message,
            &mut pending,
            "请依据附件中的学位论文生成30分钟答辩PPT，要求图片、表格和公式。",
            &attachments,
            Some("output/extracted-assets/thesis/assets.manifest.json"),
        );

        let repaired: Value =
            serde_json::from_str(&pending[0].2).expect("repaired tool input should be json");
        assert_eq!(repaired["path"], "output/test.pptx");
        assert_eq!(repaired["theme"], "ocean");
        assert_eq!(repaired["strict_quality"], true);
        assert_eq!(
            repaired["asset_manifest_path"],
            "output/extracted-assets/thesis/assets.manifest.json"
        );
        assert_eq!(repaired["document_spec"]["blocks"][0]["type"], "heading");
        assert_eq!(repaired["document_spec"]["blocks"][1]["type"], "bullets");
        assert_eq!(
            repaired["document_spec"]["generation_contract"]["expected_slide_count"],
            24
        );
        assert_eq!(
            repaired["document_spec"]["generation_contract"]["required_assets"]["figures"],
            4
        );
        match &message.blocks[0] {
            ContentBlock::ToolUse { input, .. } => assert_eq!(input, &pending[0].2),
            _ => unreachable!(),
        }
    }

    #[test]
    fn document_generation_mcp_wrapper_tool_use_is_repaired_before_execution() {
        let mut message = ConversationMessage::assistant(vec![ContentBlock::ToolUse {
            id: "doc-wrapper-1".to_string(),
            name: "MCPTool".to_string(),
            input: r#"{"qualifiedName":"mcp__doc-service__generate_pptx","arguments":{"path":"/wrapped.pptx","documentSpec":"{\"title\":\"答辩PPT\",\"theme\":\"ocean\",\"blocks\":[{\"type\":\"section\",\"title\":\"研究内容\",\"content\":[{\"type\":\"list\",\"content\":[\"建模\",\"实验\"]}]}]}"}}"#.to_string(),
        }]);
        let mut pending = vec![(
            "doc-wrapper-1".to_string(),
            "MCPTool".to_string(),
            match &message.blocks[0] {
                ContentBlock::ToolUse { input, .. } => input.clone(),
                _ => unreachable!(),
            },
        )];
        let attachments = vec![
            super::AttachmentEvidence::text("/tmp/thesis.pdf", Some(88_000))
                .with_detail("chunks: 12")
                .with_detail("fullTextPath: /tmp/thesis.txt"),
        ];

        super::repair_pending_document_generation_tool_uses(
            &mut message,
            &mut pending,
            "请依据附件中的学位论文生成30分钟答辩PPT，要求图片、表格和公式。",
            &attachments,
            Some("output/extracted-assets/thesis/assets.manifest.json"),
        );

        let repaired: Value =
            serde_json::from_str(&pending[0].2).expect("repaired MCP wrapper input should be json");
        let args = &repaired["arguments"];
        assert_eq!(args["path"], "output/wrapped.pptx");
        assert_eq!(args["theme"], "ocean");
        assert_eq!(args["strict_quality"], true);
        assert_eq!(
            args["asset_manifest_path"],
            "output/extracted-assets/thesis/assets.manifest.json"
        );
        assert_eq!(args["document_spec"]["blocks"][0]["type"], "heading");
        assert_eq!(args["document_spec"]["blocks"][1]["type"], "bullets");
        assert_eq!(
            args["document_spec"]["generation_contract"]["expected_slide_count"],
            24
        );
        assert_eq!(
            args["document_spec"]["generation_contract"]["required_assets"]["figures"],
            4
        );
        assert_eq!(
            super::observed_document_generation_tool_name("MCPTool", &pending[0].2),
            Some("mcp__doc-service__generate_pptx".to_string())
        );
        match &message.blocks[0] {
            ContentBlock::ToolUse { input, .. } => assert_eq!(input, &pending[0].2),
            _ => unreachable!(),
        }
    }

    #[test]
    fn document_quality_gate_blocks_zero_slide_and_strict_quality_error() {
        let attachments = vec![super::AttachmentEvidence::text(
            "/tmp/thesis.pdf",
            Some(88_000),
        )];
        let request =
            "请依据附件中的学位论文生成30分钟答辩PPT，要求图文并茂，插入必要的图片、表格和公式。";
        let zero_slide = r#"{"path":"output/thin.pptx","manifestPath":"output/thin.pptx.manifest.json","quality":{"slide_count":0,"image_count":1,"table_count":1,"formula_count":1,"chart_count":1,"quality_level":"final"}}"#;
        let summary =
            super::document_quality_blocking_summary_for_request(zero_slide, request, &attachments)
                .expect("zero-slide presentation should be blocked");
        assert!(summary.contains("slide count 0"), "{summary}");

        let strict_error = r#"{"path":"output/degraded.pptx","manifestPath":"output/degraded.pptx.manifest.json","quality":{"slide_count":24,"warning_count":1,"quality_level":"degraded"},"error":"strict_quality enabled and quality issues were produced"}"#;
        let summary = super::document_quality_blocking_summary_for_request(
            strict_error,
            request,
            &attachments,
        )
        .expect("strict quality error should be blocked");
        assert!(summary.contains("strict_quality"), "{summary}");
    }

    #[test]
    fn document_generation_followup_reuses_workspace_attachment_manifest() {
        struct FollowupDocumentApiClient {
            call_count: usize,
        }

        impl ApiClient for FollowupDocumentApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.call_count += 1;
                match self.call_count {
                    1 => {
                        let prompt = request.system_prompt.join("\n");
                        assert!(
                            prompt.contains("集群对抗下的多层复杂网络建模与瓦解方法.pdf"),
                            "expected recovered attachment in prompt, got: {prompt}"
                        );
                        assert!(
                            prompt.contains("fullTextPath:"),
                            "expected recovered fullTextPath in prompt, got: {prompt}"
                        );
                        Ok(vec![
                            AssistantEvent::TextDelta("我会继续生成 PPT。".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    2 => {
                        let user_text = request
                            .messages
                            .iter()
                            .filter(|message| message.role == MessageRole::User)
                            .flat_map(|message| {
                                message.blocks.iter().filter_map(|block| match block {
                                    ContentBlock::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        assert!(user_text.contains("文档生成完成门禁"));
                        assert!(user_text.contains("fullTextPath:"));
                        assert!(user_text.contains("不要要求用户再次上传附件"));
                        Ok(vec![
                            AssistantEvent::ToolUse {
                                id: "generate-followup".to_string(),
                                name: "generate_file".to_string(),
                                input: r#"{"path":"output/followup.pptx","format":"pptx","document_spec":{"title":"集群对抗下的多层复杂网络建模与瓦解方法","blocks":[{"type":"heading","text":"答辩汇报"}]}}"#.to_string(),
                            },
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => Ok(vec![
                        AssistantEvent::TextDelta(
                            "已生成常规可编辑 PPT：output/followup.pptx".to_string(),
                        ),
                        AssistantEvent::MessageStop,
                    ]),
                }
            }
        }

        let root = std::env::temp_dir().join(format!(
            "himalaya-docgen-followup-{}-{}",
            std::process::id(),
            super::current_time_millis()
        ));
        let attachments = root.join(".Himalayad").join("attachments");
        fs::create_dir_all(&attachments).expect("attachments dir");
        fs::write(
            attachments.join("attachment-test.manifest.json"),
            r#"{
                "sourceName": "集群对抗下的多层复杂网络建模与瓦解方法.pdf",
                "sourcePath": "/tmp/集群对抗下的多层复杂网络建模与瓦解方法.pdf",
                "fullTextPath": "/tmp/attachment-test.extracted.txt",
                "extractedChars": 230581,
                "chunkCount": 12
            }"#,
        )
        .expect("manifest");

        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(true)
                .with_max_parallelism(2),
        );
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new().with_workspace_root(&root),
            FollowupDocumentApiClient { call_count: 0 },
            StaticToolExecutor::new().register("generate_file", |_input| {
                Ok(r#"{"filePath":"output/followup.pptx","format":"pptx","manifestPath":"output/followup.pptx.manifest.json","quality":{"warnings":[],"slideCount":18}}"#.to_string())
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        );

        let summary = runtime
            .run_turn("尝试用常规方式生成高质量 PPT", None)
            .expect("follow-up document generation should reuse workspace attachment manifest");

        assert!(summary.tool_results.iter().any(|message| matches!(
            &message.blocks[0],
            ContentBlock::ToolResult {
                tool_name,
                is_error: false,
                ..
            } if tool_name == "generate_file"
        )));
        let plan = runtime
            .task_registry()
            .list(None)
            .into_iter()
            .next()
            .and_then(|task| task.plan)
            .expect("decisioning plan should be recorded");
        assert!(
            plan.dag.nodes.iter().any(|node| node
                .candidate_tools
                .iter()
                .any(|tool| tool == "generate_file")),
            "document generation plan should include generate_file, got: {:?}",
            plan.dag.nodes
        );
        fs::remove_dir_all(root).expect("cleanup workspace");
    }

    #[test]
    fn empty_assistant_response_marks_runtime_task_failed() {
        struct EmptyApiClient;

        impl ApiClient for EmptyApiClient {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![AssistantEvent::MessageStop])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            EmptyApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::ReadOnly),
            vec!["system".to_string()],
        );

        let error = runtime
            .run_turn("你好", None)
            .expect_err("empty assistant response should fail the turn");

        assert!(error
            .to_string()
            .contains("assistant stream produced no content"));
        let task = runtime
            .task_registry()
            .list(None)
            .into_iter()
            .next()
            .expect("runtime task should be recorded");
        assert_eq!(task.status, crate::TaskStatus::Failed);
    }

    #[test]
    fn document_generation_detector_avoids_code_quality_review_requests() {
        assert!(user_requests_document_generation(
            "请依据附件中的学位论文，整理生成一份答辩用高质量的ppt电子文档。"
        ));
        assert!(user_requests_document_generation(
            "Create an Excel workbook with formulas and charts from this data."
        ));
        assert!(!user_requests_document_generation(
            "请阅读这篇论文并总结主要观点。"
        ));
        assert!(!user_requests_document_generation(
            "请检查文档生成代码是否完善。"
        ));
    }

    #[test]
    fn super_ppt_skill_discussion_is_not_delivery_gated() {
        struct SkillDiscussionApiClient;

        impl ApiClient for SkillDiscussionApiClient {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta(
                        "GordenSuperPPTSkill 的问题在于完成态缺少产物校验。".to_string(),
                    ),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SkillDiscussionApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("请分析 GordenSuperPPTSkill 的实现问题", None)
            .expect("skill discussion should not require PPT artifacts");

        assert_eq!(summary.iterations, 1);
        assert!(summary.tool_results.is_empty());
    }

    #[test]
    fn super_ppt_long_task_prompt_estimates_imagegen_work() {
        let prompt = document_generation_long_task_prompt(
            "请生成 15 页答辩 PPT。Use the local Himalaya skill `GordenSuperPPTSkill`.",
            &[],
            Some("Chinese"),
        );

        assert!(prompt.contains("SuperPPT"));
        assert!(prompt.contains("预估页数：15 页"));
        assert!(prompt.contains("阶段 1 至少 15 次 imagegen"));
        assert!(prompt.contains("阶段 2 至少 45 次"));
        assert!(prompt.contains("确认 imagegen 可用"));
        assert!(prompt.contains("不能用代码绘图兜底"));
    }

    #[test]
    fn super_ppt_imagegen_blocker_is_not_redriven_to_generate_file() {
        struct SuperPptBlockedApiClient;

        impl ApiClient for SuperPptBlockedApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                assert!(request
                    .system_prompt
                    .iter()
                    .any(|item| item.contains("SuperPPT")));
                Ok(vec![
                    AssistantEvent::TextDelta(
                        "当前状态：阻塞。缺少必需的 imagegen 图像生成工具，无法执行 GordenSuperPPTSkill。"
                            .to_string(),
                    ),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SuperPptBlockedApiClient,
            StaticToolExecutor::new().register("generate_file", |_input| {
                Ok(r#"{"filePath":"output/fallback.pptx"}"#.to_string())
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn(
                "请依据附件生成答辩 PPT。Use the local Himalaya skill `GordenSuperPPTSkill`.",
                None,
            )
            .expect("SuperPPT imagegen blocker should be a valid skill-level stop");

        assert_eq!(summary.iterations, 1);
        assert!(
            summary.tool_results.is_empty(),
            "SuperPPT blocker must not be converted into generate_file fallback"
        );
        let final_text = summary
            .assistant_messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(final_text.contains("imagegen"));
        assert!(final_text.contains("阻塞"));
        let task = runtime
            .task_registry()
            .list(None)
            .into_iter()
            .next()
            .expect("runtime task should be recorded");
        assert_eq!(task.status, crate::TaskStatus::Blocked);
        assert!(task
            .execution_reports
            .last()
            .is_some_and(|report| report.blocked && !report.completed));
    }

    #[test]
    fn super_ppt_progress_only_response_is_redriven_to_blocker_or_artifacts() {
        struct SuperPptProgressApiClient {
            call_count: usize,
        }

        impl ApiClient for SuperPptProgressApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.call_count += 1;
                match self.call_count {
                    1 => {
                        assert!(request
                            .system_prompt
                            .iter()
                            .any(|item| item.contains("SuperPPT")));
                        Ok(vec![
                            AssistantEvent::TextDelta("请稍等，我正在处理中...".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    2 => {
                        let user_text = request
                            .messages
                            .iter()
                            .filter(|message| message.role == MessageRole::User)
                            .flat_map(|message| {
                                message.blocks.iter().filter_map(|block| match block {
                                    ContentBlock::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        assert!(
                            user_text.contains("SuperPPT 完成门禁")
                                || user_text.contains("SuperPPT completion gate")
                        );
                        assert!(user_text.contains("imagegen-manifest"));
                        assert!(
                            user_text.contains("不要要求用户重新上传附件")
                                || user_text.contains("Do not ask the user to re-upload")
                        );
                        Ok(vec![
                            AssistantEvent::TextDelta(
                                "当前状态：阻塞。imagegen 不可用，无法执行 GordenSuperPPTSkill。"
                                    .to_string(),
                            ),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("SuperPPT progress gate should settle after redrive"),
                }
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SuperPptProgressApiClient { call_count: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn(
                "请依据附件生成答辩 PPT。Use the local Himalaya skill `GordenSuperPPTSkill`.",
                None,
            )
            .expect("SuperPPT progress-only text should be redriven");

        assert_eq!(summary.iterations, 2);
        let final_text = summary
            .assistant_messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(final_text.contains("imagegen"));
        assert!(final_text.contains("阻塞"));
        assert!(summary.tool_results.is_empty());
        let task = runtime
            .task_registry()
            .list(None)
            .into_iter()
            .next()
            .expect("runtime task should be recorded");
        assert_eq!(task.status, crate::TaskStatus::Blocked);
        assert!(task
            .execution_reports
            .last()
            .is_some_and(|report| report.blocked && !report.completed));
    }

    #[test]
    fn super_ppt_artifact_evidence_allows_completion_without_generate_file() {
        struct SuperPptArtifactApiClient;

        impl ApiClient for SuperPptArtifactApiClient {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta(
                        "已生成图片型 PPT：out/image-deck.pptx；可编辑 PPTX：out/editable-deck.pptx；imagegen-manifest.json：out/imagegen-manifest.json；slides：out/slides/slide-01.png。"
                            .to_string(),
                    ),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SuperPptArtifactApiClient,
            StaticToolExecutor::new().register("generate_file", |_input| {
                Ok(r#"{"filePath":"output/fallback.pptx"}"#.to_string())
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn(
                "请依据附件生成答辩 PPT。Use the local Himalaya skill `GordenSuperPPTSkill`.",
                None,
            )
            .expect("SuperPPT artifact evidence should satisfy the skill gate");

        assert_eq!(summary.iterations, 1);
        assert!(
            summary.tool_results.is_empty(),
            "SuperPPT artifact delivery must not be converted into generate_file fallback"
        );
        let task = runtime
            .task_registry()
            .list(None)
            .into_iter()
            .next()
            .expect("runtime task should be recorded");
        assert_eq!(task.status, crate::TaskStatus::Completed);
        assert!(task
            .execution_reports
            .last()
            .is_some_and(|report| !report.blocked && report.completed));
    }

    #[test]
    fn super_ppt_repeated_progress_only_response_fails_instead_of_completing() {
        struct SuperPptRepeatedProgressApiClient {
            call_count: usize,
        }

        impl ApiClient for SuperPptRepeatedProgressApiClient {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.call_count += 1;
                Ok(vec![
                    AssistantEvent::TextDelta("请稍等，我正在处理中...".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SuperPptRepeatedProgressApiClient { call_count: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let error = runtime
            .run_turn(
                "请依据附件生成答辩 PPT。Use the local Himalaya skill `GordenSuperPPTSkill`.",
                None,
            )
            .expect_err("repeated progress-only SuperPPT text should not complete");

        assert!(error.to_string().contains("SuperPPT request required PPTX"));
    }

    struct UnsupportedToolApiClient {
        call_count: usize,
    }

    impl ApiClient for UnsupportedToolApiClient {
        fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.call_count += 1;
            if self.call_count == 1 {
                return Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "bad-1".to_string(),
                        name: "functionern_api".to_string(),
                        input: "{}".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ]);
            }

            let last_message = request.messages.last().expect("tool result should exist");
            assert!(matches!(
                &last_message.blocks[0],
                ContentBlock::ToolResult { tool_name, output, is_error: true, .. }
                    if tool_name == "functionern_api" && output.contains("unsupported tool")
            ));
            Ok(vec![
                AssistantEvent::TextDelta("recovered".to_string()),
                AssistantEvent::MessageStop,
            ])
        }
    }

    struct PanicPrompter;

    impl PermissionPrompter for PanicPrompter {
        fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
            panic!("unsupported tools should not request permission")
        }
    }

    #[test]
    fn unsupported_model_tool_names_skip_permission_prompts() {
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            UnsupportedToolApiClient { call_count: 0 },
            StaticToolExecutor::new().register("WebSearch", |_input| Ok("unused".to_string())),
            PermissionPolicy::new(PermissionMode::ReadOnly)
                .with_tool_requirement("WebSearch", PermissionMode::ReadOnly),
            vec!["system".to_string()],
        );
        let mut prompter = PanicPrompter;

        let summary = runtime
            .run_turn("use bad tool", Some(&mut prompter))
            .expect("unsupported tool should be returned to model as an error result");

        assert_eq!(summary.tool_results.len(), 1);
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult { tool_name, output, is_error: true, .. }
                if tool_name == "functionern_api" && output.contains("unsupported tool")
        ));
    }

    struct AttachmentMergeApiClient;

    impl ApiClient for AttachmentMergeApiClient {
        fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            assert_eq!(request.messages.len(), 1);
            let message = request.messages.first().expect("user message should exist");
            assert_eq!(message.role, MessageRole::User);
            assert_eq!(message.blocks.len(), 2);
            assert!(matches!(
                &message.blocks[0],
                ContentBlock::Text { text } if text == "summarize the paper"
            ));
            assert!(matches!(
                &message.blocks[1],
                ContentBlock::Text { text } if text.contains("[File: paper.pdf]")
            ));
            Ok(vec![
                AssistantEvent::TextDelta("summary".to_string()),
                AssistantEvent::MessageStop,
            ])
        }
    }

    #[test]
    fn injected_user_blocks_merge_into_next_prompt_message() {
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            AttachmentMergeApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::ReadOnly),
            vec!["system".to_string()],
        );

        runtime
            .inject_user_blocks(vec![ContentBlock::Text {
                text: "[File: paper.pdf]\npaper body".to_string(),
            }])
            .expect("file blocks should queue");
        runtime
            .run_turn("summarize the paper", None)
            .expect("turn should run");

        assert_eq!(runtime.session().messages.len(), 2);
        let user_message = &runtime.session().messages[0];
        assert_eq!(user_message.role, MessageRole::User);
        assert_eq!(user_message.blocks.len(), 2);
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
                    kind: MemoryKind::General,
                    topic: "older-topic".to_string(),
                    note: "older entry".to_string(),
                    confidence: 0.30,
                    ts_ms: 10,
                },
                MemoryEntry {
                    kind: MemoryKind::General,
                    topic: "fresh-topic".to_string(),
                    note: "higher confidence".to_string(),
                    confidence: 0.90,
                    ts_ms: 20,
                },
                MemoryEntry {
                    kind: MemoryKind::General,
                    topic: "fresh-topic".to_string(),
                    note: "duplicate with lower confidence".to_string(),
                    confidence: 0.70,
                    ts_ms: 30,
                },
                MemoryEntry {
                    kind: MemoryKind::General,
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
    fn workspace_memory_writes_to_workspace_path_even_without_existing_file() {
        let root = std::env::temp_dir().join(format!(
            "himalaya-workspace-memory-path-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("workspace should be created");

        let mut memory = LongTermMemory::load_for_workspace(Some(&root));
        let expected_path = root.join(".Himalaya").join("long_term_memory.json");
        assert_eq!(memory.path, expected_path);
        memory.add_typed_entry(
            MemoryKind::LanguagePreference,
            "language_preference",
            "Chinese",
            0.98,
        );

        let saved = fs::read_to_string(&expected_path).expect("workspace memory should be saved");
        assert!(saved.contains("language_preference"));
        assert!(saved.contains("Chinese"));
        fs::remove_dir_all(root).expect("cleanup workspace");
    }
    #[test]
    fn detect_input_language_defaults_to_input_script() {
        // Chinese input → Chinese.
        assert_eq!(
            super::detect_input_language("请分析这个项目的源代码结构"),
            Some("Chinese".to_string())
        );
        // English input → None (model default).
        assert_eq!(super::detect_input_language("analyze this project"), None);
        // Mostly-English with a single quoted Chinese identifier → stays None.
        assert_eq!(
            super::detect_input_language("rename the 文件 variable to file in main.rs"),
            None
        );
        // Empty / whitespace → None.
        assert_eq!(super::detect_input_language("   "), None);
    }

    #[test]
    fn detect_input_language_recognizes_other_scripts() {
        // Japanese kana → Japanese (even mixed with Han).
        assert_eq!(
            super::detect_input_language("このプロジェクトを分析してください"),
            Some("Japanese".to_string())
        );
        // Korean Hangul → Korean.
        assert_eq!(
            super::detect_input_language("이 프로젝트를 분석해 주세요"),
            Some("Korean".to_string())
        );
        // Cyrillic → Russian.
        assert_eq!(
            super::detect_input_language("проанализируйте этот проект"),
            Some("Russian".to_string())
        );
        // Arabic → Arabic.
        assert_eq!(
            super::detect_input_language("حلل هذا المشروع"),
            Some("Arabic".to_string())
        );
    }

    #[test]
    fn plain_chinese_input_injects_language_contract_without_explicit_preference() {
        // A Chinese prompt with NO explicit "请用中文" preference and no stored
        // memory must still default the output language to Chinese.
        struct LangProbeApi {
            saw_chinese_contract: std::rc::Rc<std::cell::Cell<bool>>,
        }
        impl ApiClient for LangProbeApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let system = request.system_prompt.join("\n");
                if system.contains("Output-language contract: respond to the user in Chinese") {
                    self.saw_chinese_contract.set(true);
                }
                Ok(vec![
                    AssistantEvent::TextDelta("好的".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        // Isolated workspace so no stored language memory leaks in.
        let root = std::env::temp_dir().join(format!(
            "himalaya-lang-default-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".Himalaya")).expect("workspace");
        fs::write(root.join(".Himalaya/long_term_memory.json"), "[]").expect("empty memory");

        let saw = std::rc::Rc::new(std::cell::Cell::new(false));
        let mut runtime = ConversationRuntime::new(
            Session::new().with_workspace_root(root.clone()),
            LangProbeApi {
                saw_chinese_contract: saw.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        let _ = runtime.run_turn("帮我把这个函数重构一下", None);
        assert!(
            saw.get(),
            "plain Chinese input should inject the Chinese output-language contract"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn current_chinese_input_overrides_stored_english_language_preference() {
        struct LangProbeApi {
            saw_chinese_contract: std::rc::Rc<std::cell::Cell<bool>>,
        }
        impl ApiClient for LangProbeApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let system = request.system_prompt.join("\n");
                if system.contains("Output-language contract: respond to the user in Chinese") {
                    self.saw_chinese_contract.set(true);
                }
                Ok(vec![
                    AssistantEvent::TextDelta("好的".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let root = std::env::temp_dir().join(format!(
            "himalaya-lang-current-wins-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".Himalaya")).expect("workspace");
        let mut memory = LongTermMemory::load_for_workspace(Some(&root));
        memory.add_typed_entry(
            MemoryKind::LanguagePreference,
            "language_preference",
            "English",
            0.99,
        );

        let saw = std::rc::Rc::new(std::cell::Cell::new(false));
        let mut runtime = ConversationRuntime::new(
            Session::new().with_workspace_root(root.clone()),
            LangProbeApi {
                saw_chinese_contract: saw.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        let _ = runtime.run_turn(
            "请依据附件中的学位论文，整理生成一份答辩用高质量的ppt电子文档。",
            None,
        );
        assert!(
            saw.get(),
            "current Chinese input should override stale stored English preference"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn language_preference_persists_as_output_contract_across_turns() {
        struct InspectingLanguageApi {
            call_count: usize,
        }

        impl ApiClient for InspectingLanguageApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.call_count += 1;
                let system = request.system_prompt.join("\n");
                assert!(system.contains("Output-language contract: respond to the user in Chinese"));
                assert!(system.contains("prose headings and explanations must be Chinese"));

                let is_workspace_analysis_turn = request.messages.iter().any(|message| {
                    message.role == MessageRole::User
                        && message.blocks.iter().any(|block| match block {
                            ContentBlock::Text { text } => {
                                text.contains("请分析当前工程")
                                    || text.contains("Workspace analysis requires local evidence")
                            }
                            _ => false,
                        })
                });
                if is_workspace_analysis_turn {
                    assert!(system.contains("# Output language (MANDATORY)"));
                    assert!(system.contains("- Output language: Chinese"));
                    assert!(system.contains("Task class: current workspace/source analysis"));
                }
                Ok(vec![
                    AssistantEvent::TextDelta("好的。".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let root = std::env::temp_dir().join(format!(
            "himalaya-language-contract-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).expect("workspace src dir");
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n")
            .expect("manifest should be written");
        fs::write(root.join("src").join("main.rs"), "fn main() {}\n")
            .expect("source should be written");
        fs::create_dir_all(root.join(".Himalaya")).expect("memory dir should exist");
        fs::write(root.join(".Himalaya").join("long_term_memory.json"), "[]")
            .expect("memory should be isolated");

        let session = Session::new().with_workspace_root(&root);
        let mut runtime = ConversationRuntime::new(
            session,
            InspectingLanguageApi { call_count: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::ReadOnly),
            vec!["system".to_string()],
        );
        runtime
            .run_turn("你好，请用中文和我交流", None)
            .expect("language preference turn should succeed");
        runtime
            .run_turn("请分析当前工程下的源代码目录结构及源代码", None)
            .expect("later workspace turn should retain language contract");
        fs::remove_dir_all(root).expect("cleanup workspace");
    }

    #[test]
    fn extracts_explicit_chinese_identity_memory() {
        let facts = super::extract_user_memory_facts("记住，我叫沐沐，你叫拉雅，以后用中文交流。");

        assert!(facts.iter().any(|(kind, topic, note)| {
            *kind == MemoryKind::UserIdentity && topic == "user_identity" && note == "沐沐"
        }));
        assert!(facts.iter().any(|(kind, topic, note)| {
            *kind == MemoryKind::AssistantIdentity
                && topic == "assistant_identity"
                && note == "拉雅"
        }));
        assert!(facts.iter().any(|(kind, topic, note)| {
            *kind == MemoryKind::LanguagePreference
                && topic == "language_preference"
                && note == "Chinese"
        }));

        let prefixed = super::extract_user_memory_facts(
            "[Persistent interaction language: respond to the user in Chinese unless they explicitly change language.]\n\n请生成一份PPT。",
        );
        assert!(prefixed.iter().any(|(kind, topic, note)| {
            *kind == MemoryKind::LanguagePreference
                && topic == "language_preference"
                && note == "Chinese"
        }));
    }

    #[test]
    fn extracts_explicit_english_identity_memory() {
        let facts = super::extract_user_memory_facts(
            "Remember, my name is Mumu and your name is Raya. Use Chinese from now on.",
        );

        assert!(facts.iter().any(|(kind, topic, note)| {
            *kind == MemoryKind::UserIdentity && topic == "user_identity" && note == "Mumu"
        }));
        assert!(facts.iter().any(|(kind, topic, note)| {
            *kind == MemoryKind::AssistantIdentity
                && topic == "assistant_identity"
                && note == "Raya"
        }));
        assert!(facts.iter().any(|(kind, topic, note)| {
            *kind == MemoryKind::LanguagePreference
                && topic == "language_preference"
                && note == "Chinese"
        }));
    }

    #[test]
    fn legacy_memory_json_defaults_to_general_kind() {
        let entries = serde_json::from_str::<Vec<MemoryEntry>>(
            r#"[{"topic":"older-topic","note":"older entry","confidence":0.7,"ts_ms":10}]"#,
        )
        .expect("legacy memory should deserialize");

        assert_eq!(entries[0].kind, MemoryKind::General);
        assert_eq!(entries[0].topic, "older-topic");
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
            path: workspace_root
                .join(".Himalaya")
                .join("long_term_memory.json"),
            entries: Vec::new(),
        };

        let mut chain = ChainOfThought::new();
        chain.add_step(ReasoningStep::Analysis {
            content: "Need shell access to inspect workspace write behavior".to_string(),
            confidence: Some(0.30),
            signature: None,
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

        assert!(memory
            .entries
            .iter()
            .any(|entry| entry.topic == "tool_failure"));
        assert!(memory.entries.iter().any(|entry| entry.topic == "shell"));
        assert!(memory
            .entries
            .iter()
            .any(|entry| entry.topic == "reflection_summary"));
        assert!(memory
            .entries
            .iter()
            .any(|entry| entry.topic == "low_confidence_decision"));
        assert!(memory
            .entries
            .iter()
            .any(|entry| entry.topic == "turn_summary"));
    }

    #[test]
    fn reflection_does_not_store_assistant_identity_claims() {
        let workspace_root = std::env::temp_dir().join(format!(
            "himalaya-reflection-identity-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after UNIX_EPOCH")
                .as_millis()
        ));
        let mut memory = LongTermMemory {
            path: workspace_root
                .join(".Himalaya")
                .join("long_term_memory.json"),
            entries: Vec::new(),
        };
        let summary = TurnSummary {
            assistant_messages: vec![ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "My name is Nemotron, I was created by NVIDIA.".to_string(),
            }])],
            tool_results: Vec::new(),
            prompt_cache_events: Vec::new(),
            iterations: 1,
            usage: TokenUsage::default(),
            auto_compaction: None,
        };

        super::record_reflection_memory(&mut memory, None, &summary);

        assert!(!memory
            .entries
            .iter()
            .any(|entry| entry.note.contains("Nemotron")));
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

        let workspace_root = std::env::temp_dir().join(format!(
            "himalaya-decisioning-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after UNIX_EPOCH")
                .as_millis()
        ));
        let memory_dir = workspace_root.join(".Himalaya");
        fs::create_dir_all(&memory_dir).expect("workspace memory dir should be created");
        fs::write(memory_dir.join("long_term_memory.json"), "[]")
            .expect("workspace memory should be isolated");

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new().with_workspace_root(workspace_root),
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
    fn initial_decisioning_plan_is_recorded_before_first_model_call() {
        struct InspectInitialPlanApi;

        impl ApiClient for InspectInitialPlanApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                assert!(request
                    .system_prompt
                    .iter()
                    .any(|part| part.contains("# Advisory task plan")));
                assert!(request
                    .system_prompt
                    .iter()
                    .any(|part| part.contains("Preferred tools")));
                Ok(vec![
                    AssistantEvent::TextDelta("planned".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(true)
                .with_max_parallelism(2),
        );
        struct RecordingRuntimeReporter {
            events: Arc<Mutex<Vec<crate::RuntimeEvent>>>,
        }
        impl crate::RuntimeEventReporter for RecordingRuntimeReporter {
            fn emit_runtime_event(&self, event: &crate::RuntimeEvent) {
                self.events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(event.clone());
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            InspectInitialPlanApi,
            StaticToolExecutor::new()
                .register("read_file", |_input| Ok("contents".to_string()))
                .register("grep_search", |_input| Ok("matches".to_string()))
                .register("edit_file", |_input| Ok("edited".to_string())),
            PermissionPolicy::new(PermissionMode::ReadOnly),
            vec!["system".to_string()],
            &feature_config,
        )
        .with_runtime_event_reporter(RecordingRuntimeReporter {
            events: events.clone(),
        });

        runtime
            .run_turn("please plan a refactor", None)
            .expect("initial decisioning turn should complete");

        assert!(runtime
            .task_registry()
            .list(None)
            .iter()
            .any(|task| task.plan.is_some()));
        let events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let first_model_route = events
            .iter()
            .position(|event| matches!(event, crate::RuntimeEvent::ModelRoute(_)))
            .expect("model route should be emitted");
        let first_decisioning = events
            .iter()
            .position(|event| matches!(event, crate::RuntimeEvent::Decisioning(_)))
            .expect("initial decisioning should be emitted");
        assert!(first_decisioning < first_model_route);
    }

    #[test]
    fn runtime_reporter_emits_long_task_lifecycle_events() {
        struct ToolApiClient {
            call_count: usize,
        }

        impl ApiClient for ToolApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                assert!(request.model_route.is_some());
                self.call_count += 1;
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("done".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "read_file".to_string(),
                        input: r#"{"path":"fixture.txt"}"#.to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        struct RecordingRuntimeReporter {
            events: Arc<Mutex<Vec<crate::RuntimeEvent>>>,
        }

        impl crate::RuntimeEventReporter for RecordingRuntimeReporter {
            fn emit_runtime_event(&self, event: &crate::RuntimeEvent) {
                self.events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(event.clone());
            }
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(true)
                .with_max_parallelism(2),
        );
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            ToolApiClient { call_count: 0 },
            StaticToolExecutor::new().register("read_file", |_input| Ok("contents".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        )
        .with_runtime_event_reporter(RecordingRuntimeReporter {
            events: events.clone(),
        });

        runtime
            .run_turn("read the fixture", None)
            .expect("runtime turn should succeed");

        let events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::TaskLedger(entry)
                if entry.event == "created" && entry.status == crate::TaskStatus::Created
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::TaskLedger(entry)
                if entry.event == "status_changed" && entry.status == crate::TaskStatus::Completed
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::ModelRoute(route) if route.phase == crate::ModelRoutePhase::Coding
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::ModelRoute(route) if route.phase == crate::ModelRoutePhase::Verification
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::PlanExecution(plan_event)
                if plan_event.kind == crate::PlanExecutionEventKind::NodeStarted
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::PlanExecution(plan_event)
                if plan_event.kind == crate::PlanExecutionEventKind::NodeSucceeded
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::TeamExecution(team_event)
                if team_event.kind == crate::TeamExecutionEventKind::VerificationPassed
        )));
    }

    #[test]
    fn packet_verification_failure_emits_recovery_events() {
        struct FinalTextApiClient;

        impl ApiClient for FinalTextApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                assert!(request.model_route.is_some());
                Ok(vec![
                    AssistantEvent::TextDelta("implemented".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        struct RecordingRuntimeReporter {
            events: Arc<Mutex<Vec<crate::RuntimeEvent>>>,
        }

        impl crate::RuntimeEventReporter for RecordingRuntimeReporter {
            fn emit_runtime_event(&self, event: &crate::RuntimeEvent) {
                self.events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(event.clone());
            }
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            FinalTextApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_runtime_event_reporter(RecordingRuntimeReporter {
            events: events.clone(),
        });
        let packet = serde_json::json!({
            "objective": "Ship verified work",
            "scope": "runtime verification",
            "repo": "Himalaya",
            "branch_policy": "no branch change",
            "acceptance_tests": ["rustc --definitely-not-a-real-flag"],
            "commit_policy": "no commit",
            "reporting_contract": "report verification result",
            "escalation_policy": "manual"
        })
        .to_string();

        let error = runtime
            .run_turn(packet, None)
            .expect_err("failing acceptance test should fail the turn");
        assert!(error.to_string().contains("verification command"));

        let events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::TaskLedger(entry)
                if entry.event == "verification_recorded" && entry.status == crate::TaskStatus::Running
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::TaskLedger(entry)
                if entry.event == "status_changed" && entry.status == crate::TaskStatus::Recovering
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::TaskLedger(entry)
                if entry.event == "status_changed" && entry.status == crate::TaskStatus::Blocked
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::TaskLedger(entry)
                if entry.event == "recovery_recorded" && entry.status == crate::TaskStatus::Recovering
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::TaskLedger(entry)
                if entry.event == "route_feedback_recorded"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::TeamExecution(team_event)
                if team_event.kind == crate::TeamExecutionEventKind::VerificationFailed
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::RuntimeEvent::Recovery(crate::RecoveryEvent::RecoveryAttempted { scenario, .. })
                if *scenario == crate::FailureScenario::CompileRedCrossCrate
        )));
    }

    #[test]
    fn bounded_recovery_redrive_lets_a_second_attempt_pass_verification() {
        // A scripted client that fails verification on the first pass (no
        // marker file yet), then on the re-drive calls a tool that creates the
        // marker so the acceptance script passes on re-verification.
        struct RedriveApiClient {
            calls: usize,
            marker: std::path::PathBuf,
        }

        impl ApiClient for RedriveApiClient {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    // First pass: claim done without satisfying the acceptance
                    // test, so verification will fail and trigger recovery.
                    1 => Ok(vec![
                        AssistantEvent::TextDelta("first attempt".to_string()),
                        AssistantEvent::MessageStop,
                    ]),
                    // Re-drive pass: create the marker via a tool call.
                    2 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: format!("fix-{}", self.calls),
                            name: "create_marker".to_string(),
                            input: format!(
                                "{{\"path\":{}}}",
                                serde_json::to_string(&self.marker.to_string_lossy().to_string())
                                    .expect("marker path should serialize")
                            ),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    // After the fix, stop requesting tools so verification runs
                    // again — this time the marker exists and it passes.
                    _ => Ok(vec![
                        AssistantEvent::TextDelta("fixed and verified".to_string()),
                        AssistantEvent::MessageStop,
                    ]),
                }
            }
        }

        let workspace_root = std::env::temp_dir().join(format!(
            "himalaya-redrive-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after UNIX_EPOCH")
                .as_nanos()
        ));
        fs::create_dir_all(&workspace_root).expect("workspace should be created");
        let marker = workspace_root.join("verified.marker");
        // Acceptance test: a shell script that succeeds only once the marker exists.
        let script_path = workspace_root.join("acceptance.sh");
        fs::write(
            &script_path,
            format!("#!/bin/sh\ntest -f {}\n", marker.display()),
        )
        .expect("acceptance script should be written");

        let packet = serde_json::json!({
            "objective": "Create the marker and verify",
            "scope": "runtime recovery",
            "repo": "Himalaya",
            "branch_policy": "no branch change",
            "acceptance_tests": [format!("sh {}", script_path.display())],
            "commit_policy": "no commit",
            "reporting_contract": "report verification result",
            "escalation_policy": "manual"
        })
        .to_string();

        let marker_for_tool = marker.clone();
        let mut runtime = ConversationRuntime::new(
            Session::new().with_workspace_root(workspace_root.clone()),
            RedriveApiClient {
                calls: 0,
                marker: marker.clone(),
            },
            StaticToolExecutor::new().register("create_marker", move |_input| {
                fs::write(&marker_for_tool, "ok").expect("marker write should succeed");
                Ok("marker created".to_string())
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn(packet, None)
            .expect("bounded re-drive should let the second attempt pass verification");

        // The turn re-drove at least once (more than one model call) and the
        // marker the recovery pass created is present.
        assert!(summary.iterations >= 2, "expected a re-drive iteration");
        assert!(marker.exists(), "re-drive should have created the marker");

        let _ = fs::remove_dir_all(&workspace_root);
    }

    #[test]
    fn decisioning_keeps_workspace_analysis_in_evidence_stage() {
        struct WorkspaceStageApiClient {
            call_count: usize,
        }

        impl ApiClient for WorkspaceStageApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.call_count += 1;
                let tool_results = request
                    .messages
                    .iter()
                    .filter(|message| message.role == MessageRole::Tool)
                    .count();

                match self.call_count {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-final".to_string(),
                            name: "final_summary".to_string(),
                            input: "{}".to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        assert_eq!(tool_results, 1);
                        Ok(vec![
                            AssistantEvent::ToolUse {
                                id: "tool-search".to_string(),
                                name: "grep_search".to_string(),
                                input: r#"{"pattern":"main"}"#.to_string(),
                            },
                            AssistantEvent::ToolUse {
                                id: "tool-read-1".to_string(),
                                name: "read_file".to_string(),
                                input: r#"{"path":"Cargo.toml"}"#.to_string(),
                            },
                            AssistantEvent::ToolUse {
                                id: "tool-read-2".to_string(),
                                name: "read_file".to_string(),
                                input: r#"{"path":"src/lib.rs"}"#.to_string(),
                            },
                            AssistantEvent::ToolUse {
                                id: "tool-read-3".to_string(),
                                name: "read_file".to_string(),
                                input: r#"{"path":"src/main.rs"}"#.to_string(),
                            },
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => {
                        assert!(tool_results >= 5);
                        Ok(vec![
                            AssistantEvent::TextDelta("final answer from evidence".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                }
            }
        }

        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(false)
                .with_max_parallelism(2),
        );
        let workspace_root = std::env::temp_dir().join(format!(
            "himalaya-decisioning-evidence-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after UNIX_EPOCH")
                .as_millis()
        ));
        fs::create_dir_all(workspace_root.join("src")).expect("workspace src should be created");
        fs::write(
            workspace_root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .expect("manifest should be written");
        fs::write(workspace_root.join("src/lib.rs"), "pub fn lib() {}\n")
            .expect("lib source should be written");
        fs::write(workspace_root.join("src/main.rs"), "fn main() {}\n")
            .expect("main source should be written");
        fs::create_dir_all(workspace_root.join(".Himalaya")).expect("memory dir should exist");
        fs::write(workspace_root.join(".Himalaya/long_term_memory.json"), "[]")
            .expect("memory should be isolated");

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new().with_workspace_root(workspace_root),
            WorkspaceStageApiClient { call_count: 0 },
            StaticToolExecutor::new()
                .register("final_summary", |_input| {
                    panic!("final_summary should be blocked before evidence is complete")
                })
                .register(
                    "grep_search",
                    |_input| Ok("src/main.rs:fn main".to_string()),
                )
                .register("read_file", |input| Ok(format!("read {input}"))),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        );

        let summary = runtime
            .run_turn("请分析当前工程的源代码目录结构和功能模块关系", None)
            .expect("workspace analysis should recover by gathering evidence");

        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult {
                tool_name,
                is_error: true,
                output,
                ..
            } if tool_name == "final_summary" && output.contains("evidence stage")
        ));
        assert_eq!(summary.tool_results.len(), 5);
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
                        input: "delete secret token credential network shell overwrite remove"
                            .to_string(),
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
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("blocked tool should not execute when permission is denied")
            }),
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
    fn auto_compaction_uses_model_summarization_route_when_available() {
        use std::sync::{Arc, Mutex};

        // Records the phase of each request so we can assert a Summarization
        // request was issued during auto-compaction, and returns a recognizable
        // summary for that phase.
        #[derive(Clone)]
        struct PhaseRecordingApi {
            phases: Arc<Mutex<Vec<Option<String>>>>,
        }
        impl ApiClient for PhaseRecordingApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let phase = request
                    .model_route
                    .as_ref()
                    .map(|route| format!("{:?}", route.phase));
                self.phases
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(phase.clone());
                if phase.as_deref() == Some("Summarization") {
                    return Ok(vec![
                        AssistantEvent::TextDelta(
                            "MODEL_SUMMARY: prior work captured.".to_string(),
                        ),
                        AssistantEvent::MessageStop,
                    ]);
                }
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

        let phases = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = ConversationRuntime::new(
            session,
            PhaseRecordingApi {
                phases: phases.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");

        assert!(summary.auto_compaction.is_some(), "compaction should occur");
        // A Summarization-phase request was issued during compaction.
        let recorded = phases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            recorded
                .iter()
                .any(|phase| phase.as_deref() == Some("Summarization")),
            "expected a Summarization-route request, got: {recorded:?}"
        );
        // The model summary made it into the compacted system message.
        let system_text = runtime.session().messages[0]
            .blocks
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        assert!(
            system_text.contains("MODEL_SUMMARY: prior work captured."),
            "expected model summary in continuation, got: {system_text}"
        );
    }

    #[test]
    fn difficulty_gate_invokes_planning_route_for_complex_tasks() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct PlanPhaseApi {
            phases: Arc<Mutex<Vec<Option<String>>>>,
        }
        impl ApiClient for PlanPhaseApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let phase = request
                    .model_route
                    .as_ref()
                    .map(|route| format!("{:?}", route.phase));
                self.phases
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(phase.clone());
                if phase.as_deref() == Some("Planning") {
                    return Ok(vec![
                        AssistantEvent::TextDelta(
                            "PLAN: 1. inspect 2. implement 3. test".to_string(),
                        ),
                        AssistantEvent::MessageStop,
                    ]);
                }
                // Coding phase: answer with final text (no tools) so the turn ends.
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(false)
                .with_planning_complexity_threshold(4),
        );
        let phases = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            PlanPhaseApi {
                phases: phases.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        );

        // A multi-capability request (implement + edit + test + verify) clamps
        // to high complexity without triggering the workspace-analysis gate.
        let _ = runtime
            .run_turn(
                "implement, refactor, write, test and verify the new payment feature",
                None,
            )
            .expect("turn should succeed");

        let recorded = phases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            recorded
                .iter()
                .any(|phase| phase.as_deref() == Some("Planning")),
            "expected a Planning-route request for a complex task, got: {recorded:?}"
        );
    }

    #[test]
    fn difficulty_gate_skips_planning_for_simple_tasks() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct PlanPhaseApi {
            phases: Arc<Mutex<Vec<Option<String>>>>,
        }
        impl ApiClient for PlanPhaseApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let phase = request
                    .model_route
                    .as_ref()
                    .map(|route| format!("{:?}", route.phase));
                self.phases
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(phase);
                Ok(vec![
                    AssistantEvent::TextDelta("ok".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(false)
                .with_planning_complexity_threshold(4),
        );
        let phases = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            PlanPhaseApi {
                phases: phases.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        );

        let _ = runtime.run_turn("hi", None).expect("turn should succeed");

        let recorded = phases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            !recorded
                .iter()
                .any(|phase| phase.as_deref() == Some("Planning")),
            "simple task should not trigger the Planning route, got: {recorded:?}"
        );
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
                signature: None,
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
        assert!(
            result.is_ok(),
            "Reasoning steps should not break message building"
        );

        let (message, _, _, chain_opt) = result.unwrap();
        assert_eq!(message.blocks.len(), 3);
        assert!(matches!(message.blocks[0], ContentBlock::Thinking { .. }));
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

    #[test]
    fn extract_json_object_recovers_object_from_prose_and_fences() {
        let fenced = "Here is the plan:\n```json\n{\"steps\":[{\"id\":\"a\"}]}\n```\nDone.";
        let extracted = super::extract_json_object(fenced).expect("json");
        assert_eq!(extracted, "{\"steps\":[{\"id\":\"a\"}]}");
        // Braces inside strings must not confuse the balance scan.
        let tricky = "{\"title\":\"a } b\",\"n\":1}";
        assert_eq!(super::extract_json_object(tricky).unwrap(), tricky);
        assert!(super::extract_json_object("no object here").is_none());
    }

    #[test]
    fn structured_execution_builds_model_dag_when_enabled() {
        use std::sync::{Arc, Mutex};

        // Returns a JSON structured plan on the Planning route; finishes with
        // plain text on the Coding route so the turn completes.
        #[derive(Clone)]
        struct StructuredPlanApi {
            saw_structured_request: Arc<Mutex<bool>>,
        }
        impl ApiClient for StructuredPlanApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let is_planning = request
                    .model_route
                    .as_ref()
                    .is_some_and(|route| format!("{:?}", route.phase) == "Planning");
                if is_planning {
                    *self
                        .saw_structured_request
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                    return Ok(vec![
                        AssistantEvent::TextDelta(
                            "{\"steps\":[{\"id\":\"design\",\"title\":\"Design\"},{\"id\":\"impl\",\"title\":\"Implement\",\"depends_on\":[\"design\"],\"acceptance\":[\"python3 --version\"]}],\"notes\":[\"plan\"]}".to_string(),
                        ),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(false)
                .with_structured_execution_threshold(3),
        );
        let saw = Arc::new(Mutex::new(false));
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            StructuredPlanApi {
                saw_structured_request: saw.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        );

        // High-complexity multi-capability prompt clears the threshold.
        let _ = runtime
            .run_turn(
                "implement, refactor, write, test and verify the new billing module",
                None,
            )
            .expect("turn should succeed");

        assert!(
            *saw.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            "structured plan request should have been issued on the Planning route"
        );
        // The structured DAG was recorded for the task.
        assert!(runtime
            .task_registry()
            .list(None)
            .iter()
            .any(|task| task.plan.is_some()));
    }

    #[test]
    fn structured_execution_dispatches_each_node_through_the_model() {
        use std::sync::{Arc, Mutex};

        // Records the step ids that get dispatched as node sub-turns (their
        // prompts contain "Focus only on this step:\n- id: <id>").
        #[derive(Clone)]
        struct NodeDispatchApi {
            node_ids: Arc<Mutex<Vec<String>>>,
        }
        impl ApiClient for NodeDispatchApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let phase = request
                    .model_route
                    .as_ref()
                    .map(|r| format!("{:?}", r.phase))
                    .unwrap_or_default();
                if phase == "Planning" {
                    return Ok(vec![
                        AssistantEvent::TextDelta(
                            "{\"steps\":[{\"id\":\"first\",\"title\":\"First\"},{\"id\":\"second\",\"title\":\"Second\",\"depends_on\":[\"first\"],\"acceptance\":[\"python3 --version\"]}]}".to_string(),
                        ),
                        AssistantEvent::MessageStop,
                    ]);
                }
                // Node sub-turn? Capture the step id from the prompt.
                let user_text = request
                    .messages
                    .iter()
                    .flat_map(|m| m.blocks.iter())
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if let Some(idx) = user_text.find("- id: ") {
                    let id: String = user_text[idx + 6..]
                        .chars()
                        .take_while(|c| !c.is_whitespace())
                        .collect();
                    self.node_ids
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(id);
                }
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(false)
                .with_structured_execution_threshold(3),
        );
        let node_ids = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            NodeDispatchApi {
                node_ids: node_ids.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        );

        let _ = runtime
            .run_turn(
                "implement, refactor, write, test and verify the new billing module",
                None,
            )
            .expect("turn should succeed");

        let dispatched = node_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // Both nodes ran, dependency before dependent.
        assert_eq!(dispatched, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn structured_node_redrives_until_acceptance_passes() {
        use std::sync::{Arc, Mutex};

        let workspace_root = std::env::temp_dir().join(format!(
            "himalaya-node-redrive-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&workspace_root).expect("workspace");
        let marker = workspace_root.join("node.marker");
        let script = workspace_root.join("accept.sh");
        fs::write(
            &script,
            format!("#!/bin/sh\ntest -f {}\n", marker.display()),
        )
        .expect("script");

        // Planning returns a small chain with a leaf acceptance command. The
        // build node creates the marker only on its SECOND invocation, so the
        // first acceptance check fails and the node must re-drive.
        #[derive(Clone)]
        struct NodeRedriveApi {
            node_calls: Arc<Mutex<usize>>,
            marker: std::path::PathBuf,
            accept_cmd: String,
        }
        impl ApiClient for NodeRedriveApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let phase = request
                    .model_route
                    .as_ref()
                    .map(|r| format!("{:?}", r.phase))
                    .unwrap_or_default();
                if phase == "Planning" {
                    let json = format!(
                        "{{\"steps\":[{{\"id\":\"analyze\",\"title\":\"Analyze\"}},{{\"id\":\"build\",\"title\":\"Build\",\"depends_on\":[\"analyze\"],\"acceptance\":[{}]}}]}}",
                        serde_json::to_string(&self.accept_cmd).unwrap()
                    );
                    return Ok(vec![
                        AssistantEvent::TextDelta(json),
                        AssistantEvent::MessageStop,
                    ]);
                }
                let user_text = request
                    .messages
                    .iter()
                    .flat_map(|m| m.blocks.iter())
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if user_text.contains("Focus only on this step")
                    && user_text.contains("- id: build")
                {
                    let mut calls = self
                        .node_calls
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *calls += 1;
                    if *calls >= 2 {
                        let _ = fs::write(&self.marker, "ok");
                    }
                    return Ok(vec![
                        AssistantEvent::TextDelta("step output".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(false)
                .with_structured_execution_threshold(3),
        );
        let node_calls = Arc::new(Mutex::new(0usize));
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new().with_workspace_root(workspace_root.clone()),
            NodeRedriveApi {
                node_calls: node_calls.clone(),
                marker: marker.clone(),
                accept_cmd: format!("sh {}", script.display()),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        );

        let _ = runtime
            .run_turn(
                "implement, refactor, write, test and verify the build pipeline",
                None,
            )
            .expect("turn should succeed");

        // The node ran at least twice (initial + one re-drive) and the marker
        // its re-drive created exists.
        assert!(
            *node_calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                >= 2,
            "node should have re-driven at least once"
        );
        assert!(marker.exists(), "re-drive should have satisfied acceptance");

        let _ = fs::remove_dir_all(&workspace_root);
    }

    #[test]
    fn structured_node_routes_high_effort_to_quality_model() {
        use std::sync::{Arc, Mutex};

        // Records the model id used for each node sub-turn so we can assert the
        // high-effort node selected the high-quality route.
        #[derive(Clone)]
        struct NodeRouteApi {
            node_models: Arc<Mutex<Vec<String>>>,
        }
        impl ApiClient for NodeRouteApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let phase = request
                    .model_route
                    .as_ref()
                    .map(|r| format!("{:?}", r.phase))
                    .unwrap_or_default();
                if phase == "Planning" {
                    // High estimated_effort on the leaf node.
                    return Ok(vec![
                        AssistantEvent::TextDelta(
                            "{\"steps\":[{\"id\":\"prep\",\"title\":\"Prep\"},{\"id\":\"hard\",\"title\":\"Hard step\",\"depends_on\":[\"prep\"],\"estimated_effort\":5,\"acceptance\":[\"python3 --version\"]}]}".to_string(),
                        ),
                        AssistantEvent::MessageStop,
                    ]);
                }
                let user_text = request
                    .messages
                    .iter()
                    .flat_map(|m| m.blocks.iter())
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if user_text.contains("Focus only on this step") {
                    if let Some(model) = request.model_route.as_ref().map(|r| r.model.clone()) {
                        self.node_models
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(model);
                    }
                }
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(false)
                .with_structured_execution_threshold(3),
        );
        // Two Coding routes: a cheap/fast one and a high-quality one.
        let policy = crate::MoERoutingPolicy::new(
            "default",
            vec![
                crate::ModelRoute::new(crate::ModelRoutePhase::Coding, "cheap-fast")
                    .with_weights(5, 5, 2),
                crate::ModelRoute::new(crate::ModelRoutePhase::Coding, "high-quality")
                    .with_weights(1, 1, 5),
                crate::ModelRoute::new(crate::ModelRoutePhase::Planning, "planner"),
            ],
        );
        let node_models = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            NodeRouteApi {
                node_models: node_models.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        )
        .with_model_router(crate::ModelRouter::new(policy));

        let _ = runtime
            .run_turn(
                "implement, refactor, write, test and verify the hard module",
                None,
            )
            .expect("turn should succeed");

        let models = node_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            models.iter().any(|m| m == "high-quality"),
            "high-effort node should route to the high-quality model, got: {models:?}"
        );
    }

    #[test]
    fn team_convergence_runs_three_roles_and_redrives_executor_on_rejection() {
        use std::sync::{Arc, Mutex};

        // Records each role sub-turn by phase, and makes the reviewer reject
        // once before approving so the executor re-drives.
        #[derive(Clone)]
        struct TeamApi {
            phases: Arc<Mutex<Vec<String>>>,
            reviews: Arc<Mutex<usize>>,
        }
        impl ApiClient for TeamApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let phase = request
                    .model_route
                    .as_ref()
                    .map(|r| format!("{:?}", r.phase))
                    .unwrap_or_default();
                let user_text = request
                    .messages
                    .iter()
                    .flat_map(|m| m.blocks.iter())
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();

                // Planning is used for the structured plan AND the architect.
                // Distinguish by the architect role marker in the prompt.
                if phase == "Planning" && !user_text.contains("Role: architect") {
                    // Structured plan: a prep step followed by one high-effort
                    // leaf node.
                    return Ok(vec![
                        AssistantEvent::TextDelta(
                            "{\"steps\":[{\"id\":\"prep\",\"title\":\"Prep\"},{\"id\":\"core\",\"title\":\"Core work\",\"depends_on\":[\"prep\"],\"estimated_effort\":5,\"acceptance\":[\"python3 --version\"]}]}".to_string(),
                        ),
                        AssistantEvent::MessageStop,
                    ]);
                }

                // Record role phases for the node convergence.
                if user_text.contains("Role: architect") {
                    self.phases
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push("architect".to_string());
                    return Ok(vec![
                        AssistantEvent::TextDelta("brief: do X".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                if user_text.contains("Role: executor") {
                    self.phases
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push("executor".to_string());
                    return Ok(vec![
                        AssistantEvent::TextDelta("implemented X".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                if user_text.contains("Role: reviewer") {
                    self.phases
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push("reviewer".to_string());
                    let mut reviews = self
                        .reviews
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *reviews += 1;
                    let verdict = if *reviews < 2 {
                        "REQUEST_CHANGES: tighten the edge case"
                    } else {
                        "APPROVE"
                    };
                    return Ok(vec![
                        AssistantEvent::TextDelta(verdict.to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }

                // Main loop completion.
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(false)
                .with_structured_execution_threshold(3)
                .with_team_convergence_threshold(4),
        );
        let phases = Arc::new(Mutex::new(Vec::new()));
        let reviews = Arc::new(Mutex::new(0usize));
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TeamApi {
                phases: phases.clone(),
                reviews: reviews.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        );

        let _ = runtime
            .run_turn(
                "implement, refactor, write, test and verify the core engine",
                None,
            )
            .expect("turn should succeed");

        let recorded = phases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // All three roles were invoked.
        assert!(
            recorded.contains(&"architect".to_string()),
            "architect ran: {recorded:?}"
        );
        assert!(
            recorded.contains(&"reviewer".to_string()),
            "reviewer ran: {recorded:?}"
        );
        // Executor ran at least twice (initial + re-drive after rejection).
        let executor_runs = recorded.iter().filter(|r| *r == "executor").count();
        assert!(
            executor_runs >= 2,
            "executor should re-drive after rejection, ran {executor_runs} times: {recorded:?}"
        );
    }

    #[test]
    fn team_convergence_records_role_dialogue_in_event_stream() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct TeamApi;
        impl ApiClient for TeamApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let phase = request
                    .model_route
                    .as_ref()
                    .map(|r| format!("{:?}", r.phase))
                    .unwrap_or_default();
                let user_text = request
                    .messages
                    .iter()
                    .flat_map(|m| m.blocks.iter())
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if phase == "Planning" && !user_text.contains("Role: architect") {
                    return Ok(vec![
                        AssistantEvent::TextDelta(
                            "{\"steps\":[{\"id\":\"prep\",\"title\":\"Prep\"},{\"id\":\"core\",\"title\":\"Core\",\"depends_on\":[\"prep\"],\"estimated_effort\":5,\"acceptance\":[\"python3 --version\"]}]}".to_string(),
                        ),
                        AssistantEvent::MessageStop,
                    ]);
                }
                let reply = if user_text.contains("Role: reviewer") {
                    "APPROVE"
                } else {
                    "ok"
                };
                Ok(vec![
                    AssistantEvent::TextDelta(reply.to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        struct TeamEventRecorder {
            events: Arc<Mutex<Vec<crate::TeamExecutionEvent>>>,
        }
        impl crate::RuntimeEventReporter for TeamEventRecorder {
            fn emit_runtime_event(&self, event: &crate::RuntimeEvent) {
                if let crate::RuntimeEvent::TeamExecution(team_event) = event {
                    self.events
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(team_event.clone());
                }
            }
        }

        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(false)
                .with_structured_execution_threshold(3)
                .with_team_convergence_threshold(4),
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TeamApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        )
        .with_runtime_event_reporter(TeamEventRecorder {
            events: events.clone(),
        });

        let _ = runtime
            .run_turn(
                "implement, refactor, write, test and verify the core engine",
                None,
            )
            .expect("turn should succeed");

        let team_events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // The three role roles appear in the recorded team-execution dialogue.
        assert!(
            team_events
                .iter()
                .any(|e| e.role == crate::TeamRole::Planner
                    && e.message
                        .as_deref()
                        .is_some_and(|m| m.contains("architect brief"))),
            "architect brief should be recorded: {team_events:?}"
        );
        assert!(
            team_events
                .iter()
                .any(|e| e.role == crate::TeamRole::Implementer),
            "executor output should be recorded"
        );
        assert!(
            team_events
                .iter()
                .any(|e| e.role == crate::TeamRole::Reviewer),
            "reviewer verdict should be recorded"
        );
    }

    #[test]
    fn team_convergence_escalates_executor_route_after_repeated_rejections() {
        use std::sync::{Arc, Mutex};

        // The reviewer rejects the first two executor attempts, then approves.
        // With a two-route Coding policy, the escalation feedback should move a
        // later executor attempt onto the high-quality route.
        #[derive(Clone)]
        struct EscalateApi {
            exec_models: Arc<Mutex<Vec<String>>>,
            reviews: Arc<Mutex<usize>>,
        }
        impl ApiClient for EscalateApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let phase = request
                    .model_route
                    .as_ref()
                    .map(|r| format!("{:?}", r.phase))
                    .unwrap_or_default();
                let model = request
                    .model_route
                    .as_ref()
                    .map(|r| r.model.clone())
                    .unwrap_or_default();
                let user_text = request
                    .messages
                    .iter()
                    .flat_map(|m| m.blocks.iter())
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if phase == "Planning" && !user_text.contains("Role: architect") {
                    return Ok(vec![
                        AssistantEvent::TextDelta(
                            "{\"steps\":[{\"id\":\"prep\",\"title\":\"Prep\"},{\"id\":\"core\",\"title\":\"Core\",\"depends_on\":[\"prep\"],\"estimated_effort\":5,\"acceptance\":[\"python3 --version\"]}]}".to_string(),
                        ),
                        AssistantEvent::MessageStop,
                    ]);
                }
                if user_text.contains("Role: executor") {
                    self.exec_models
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(model);
                    return Ok(vec![
                        AssistantEvent::TextDelta("attempt".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                if user_text.contains("Role: reviewer") {
                    let mut reviews = self
                        .reviews
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *reviews += 1;
                    let verdict = if *reviews < 3 {
                        "REQUEST_CHANGES: not yet"
                    } else {
                        "APPROVE"
                    };
                    return Ok(vec![
                        AssistantEvent::TextDelta(verdict.to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::TextDelta("ok".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let feature_config = RuntimeFeatureConfig::default().with_decisioning(
            DecisioningConfig::default()
                .with_enabled(true)
                .with_emit_events(false)
                .with_structured_execution_threshold(3)
                .with_team_convergence_threshold(4),
        );
        // Two Coding routes; Verification + Planning routes for the other roles.
        let policy = crate::MoERoutingPolicy::new(
            "default",
            vec![
                crate::ModelRoute::new(crate::ModelRoutePhase::Coding, "cheap-fast")
                    .with_weights(5, 5, 5),
                crate::ModelRoute::new(crate::ModelRoutePhase::Coding, "high-quality")
                    .with_weights(1, 1, 4),
                crate::ModelRoute::new(crate::ModelRoutePhase::Planning, "planner"),
                crate::ModelRoute::new(crate::ModelRoutePhase::Verification, "reviewer-model"),
            ],
        )
        .with_adaptive(true, 1, 50);
        let exec_models = Arc::new(Mutex::new(Vec::new()));
        let reviews = Arc::new(Mutex::new(0usize));
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            EscalateApi {
                exec_models: exec_models.clone(),
                reviews: reviews.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &feature_config,
        )
        .with_model_router(crate::ModelRouter::new(policy))
        .with_max_recovery_attempts(4);

        let _ = runtime
            .run_turn(
                "implement, refactor, write, test and verify the core engine",
                None,
            )
            .expect("turn should succeed");

        let models = exec_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // After repeated rejections, an executor attempt escalated to the
        // high-quality route.
        assert!(
            models.iter().any(|m| m == "high-quality"),
            "executor should escalate to high-quality after rejections, got: {models:?}"
        );
    }
}
