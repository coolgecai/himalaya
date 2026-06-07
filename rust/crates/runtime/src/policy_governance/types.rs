use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    AppliedRoutingPolicy, AutonomousEvaluationReport, AutonomousPolicyAction,
    RoutingPolicyApplyReport, RoutingPolicyProposal, RoutingPolicyRollbackReport,
    SchedulerDaemonState, SchedulerDaemonStatus,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDomain {
    Routing,
    AutonomousRun,
    Scheduler,
    Memory,
    Recovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyLedgerStatus {
    Observed,
    Planned,
    Proposed,
    Approved,
    DryRunPassed,
    Applied,
    ApplyBlocked,
    RollbackPlanned,
    RollbackBlocked,
    Blocked,
    RolledBack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyReference {
    pub label: String,
    pub path: Option<PathBuf>,
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyGate {
    pub domain: PolicyDomain,
    pub name: String,
    pub passed: bool,
    pub severity: PolicyRiskLevel,
    pub blocks_apply: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyProposal {
    pub id: String,
    pub domain: PolicyDomain,
    pub action: String,
    pub status: PolicyLedgerStatus,
    pub risk: PolicyRiskLevel,
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_fingerprint: String,
    pub references: Vec<PolicyReference>,
    pub gates: Vec<PolicyGate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub domain: PolicyDomain,
    pub action: String,
    pub status: PolicyLedgerStatus,
    pub risk: PolicyRiskLevel,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyConflict {
    pub domains: Vec<PolicyDomain>,
    pub severity: PolicyRiskLevel,
    pub blocks_apply: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyReviewSummary {
    pub status: PolicyLedgerStatus,
    pub proposal_count: usize,
    pub decision_count: usize,
    pub gate_count: usize,
    pub failed_gate_count: usize,
    pub conflict_count: usize,
    pub blocking_conflict_count: usize,
    pub active_routing_policy: bool,
    pub routing_proposal_count: usize,
    pub scheduler_status: Option<SchedulerDaemonStatus>,
    pub autonomous_action: Option<AutonomousPolicyAction>,
    pub autonomous_review_required: bool,
    pub memory_reuse_score: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyLedgerEntry {
    pub version: u32,
    pub id: String,
    pub timestamp: u64,
    pub status: PolicyLedgerStatus,
    pub domains: Vec<PolicyDomain>,
    pub proposals: Vec<PolicyProposal>,
    pub decisions: Vec<PolicyDecision>,
    pub gates: Vec<PolicyGate>,
    pub conflicts: Vec<PolicyConflict>,
    pub recommendations: Vec<String>,
    pub summary: PolicyReviewSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyGovernanceReview {
    pub version: u32,
    pub reviewed_at: u64,
    pub ledger_path: PathBuf,
    pub ledger_entry: PolicyLedgerEntry,
}

#[derive(Debug, Clone)]
pub struct PolicyGovernanceInput {
    pub autonomous_evaluation: Option<AutonomousEvaluationReport>,
    pub routing_proposals: Vec<RoutingPolicyProposal>,
    pub applied_routing_policy: Option<AppliedRoutingPolicy>,
    pub scheduler_state: Option<SchedulerDaemonState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyLedgerWarning {
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyLedgerLoad {
    pub ledger_path: PathBuf,
    pub entries: Vec<PolicyLedgerEntry>,
    pub malformed_lines: usize,
    pub warnings: Vec<PolicyLedgerWarning>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyApplyOperation {
    Apply,
    Rollback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyBlockerKind {
    GateFailed,
    ConflictBlocked,
    StalePlan,
    UnsupportedDomain,
    AdapterRejected,
    MissingTarget,
    AmbiguousTarget,
    MissingProposal,
    InvalidOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyBlocker {
    pub kind: PolicyBlockerKind,
    pub domain: Option<PolicyDomain>,
    pub proposal_id: Option<String>,
    pub severity: PolicyRiskLevel,
    pub reason: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyApplyAction {
    pub domain: PolicyDomain,
    pub operation: PolicyApplyOperation,
    pub action: String,
    pub proposal_id: Option<String>,
    pub status: PolicyLedgerStatus,
    pub risk: PolicyRiskLevel,
    pub executable: bool,
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structured_blockers: Vec<PolicyBlocker>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_fingerprint: String,
    pub reason: String,
    pub references: Vec<PolicyReference>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyApplyPlan {
    pub version: u32,
    pub id: String,
    pub planned_at: u64,
    pub operation: PolicyApplyOperation,
    pub dry_run: bool,
    pub status: PolicyLedgerStatus,
    pub review_status: PolicyLedgerStatus,
    pub domain_filter: Option<PolicyDomain>,
    pub proposal_id: Option<String>,
    pub actions: Vec<PolicyApplyAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adapters: Vec<PolicyAdapterDescriptor>,
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structured_blockers: Vec<PolicyBlocker>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
    pub recommendations: Vec<String>,
    pub review: PolicyGovernanceReview,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyActionReceipt {
    pub version: u32,
    pub id: String,
    pub plan_id: String,
    pub plan_fingerprint: String,
    pub operation: PolicyApplyOperation,
    pub dry_run: bool,
    pub domain: Option<PolicyDomain>,
    pub proposal_id: Option<String>,
    pub status: PolicyLedgerStatus,
    pub executed: bool,
    pub before_status: Option<PolicyLedgerStatus>,
    pub after_status: Option<PolicyLedgerStatus>,
    pub adapter: Option<String>,
    pub adapter_report_id: Option<String>,
    pub ledger_entry_id: Option<String>,
    pub blockers: Vec<PolicyBlocker>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyApplyReport {
    pub version: u32,
    pub id: String,
    pub executed_at: u64,
    pub dry_run: bool,
    pub status: PolicyLedgerStatus,
    pub applied: bool,
    pub plan: PolicyApplyPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_report: Option<PolicyAdapterReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_report: Option<RoutingPolicyApplyReport>,
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structured_blockers: Vec<PolicyBlocker>,
    pub receipt: PolicyActionReceipt,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyRollbackReport {
    pub version: u32,
    pub id: String,
    pub executed_at: u64,
    pub status: PolicyLedgerStatus,
    pub rolled_back: bool,
    pub plan: PolicyApplyPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_report: Option<PolicyAdapterReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_report: Option<RoutingPolicyRollbackReport>,
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structured_blockers: Vec<PolicyBlocker>,
    pub receipt: PolicyActionReceipt,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyLifecycleEvent {
    pub entry_id: String,
    pub timestamp: u64,
    pub status: PolicyLedgerStatus,
    pub action: String,
    pub operation: Option<PolicyApplyOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyLifecycle {
    pub domain: PolicyDomain,
    pub proposal_id: String,
    pub current_status: PolicyLedgerStatus,
    pub first_seen_at: u64,
    pub last_seen_at: u64,
    pub events: Vec<PolicyLifecycleEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyReplayAnomaly {
    pub entry_id: String,
    pub proposal_id: Option<String>,
    pub kind: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyLifecycleReplaySummary {
    pub lifecycle_count: usize,
    pub event_count: usize,
    pub anomaly_count: usize,
    pub malformed_lines: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub domain_counts: BTreeMap<PolicyDomain, usize>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub action_counts: BTreeMap<String, usize>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub anomaly_kind_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyLifecycleReplay {
    pub version: u32,
    pub ledger_path: PathBuf,
    pub entries_considered: usize,
    pub malformed_lines: usize,
    pub lifecycles: Vec<PolicyLifecycle>,
    pub anomalies: Vec<PolicyReplayAnomaly>,
    pub summary: PolicyLifecycleReplaySummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyAdapterDescriptor {
    pub name: String,
    pub domain: PolicyDomain,
    pub supports_apply: bool,
    pub supports_persistent_apply: bool,
    pub supports_dry_run: bool,
    pub supports_rollback: bool,
    pub planned_only: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerPolicyDryRunReport {
    pub proposal_id: String,
    pub dry_run: bool,
    pub status: PolicyLedgerStatus,
    pub scheduler_status: Option<SchedulerDaemonStatus>,
    pub blockers: Vec<PolicyBlocker>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryPolicyDryRunReport {
    pub proposal_id: String,
    pub dry_run: bool,
    pub status: PolicyLedgerStatus,
    pub task_count: usize,
    pub task_memory_entries: usize,
    pub memory_reuse_score: f32,
    pub blockers: Vec<PolicyBlocker>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryPolicyDryRunReport {
    pub proposal_id: String,
    pub dry_run: bool,
    pub status: PolicyLedgerStatus,
    pub recovery_triggered_tasks: usize,
    pub recovered_tasks: usize,
    pub recovery_quality_score: f32,
    pub blockers: Vec<PolicyBlocker>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "report", rename_all = "snake_case")]
pub enum PolicyAdapterReport {
    RoutingApply(RoutingPolicyApplyReport),
    RoutingRollback(RoutingPolicyRollbackReport),
    SchedulerDryRun(SchedulerPolicyDryRunReport),
    MemoryDryRun(MemoryPolicyDryRunReport),
    RecoveryDryRun(RecoveryPolicyDryRunReport),
}
