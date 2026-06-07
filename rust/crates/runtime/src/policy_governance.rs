use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    AppliedRoutingPolicy, AutonomousEvaluationReport, AutonomousPolicyAction,
    RoutingPolicyApplyReport, RoutingPolicyProposal, RoutingPolicyProposalStatus,
    RoutingPolicyProposalStore, RoutingPolicyRollbackReport, RoutingPolicySafetyLevel,
    SchedulerDaemonState, SchedulerDaemonStatus,
};

pub const POLICY_GOVERNANCE_VERSION: u32 = 1;
pub const POLICY_GOVERNANCE_LEDGER_FILE: &str = "ledger.jsonl";

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

#[derive(Debug, Clone)]
pub struct PolicyGovernanceLedger {
    dir: PathBuf,
}

impl PolicyGovernanceLedger {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    #[must_use]
    pub fn ledger_path(&self) -> PathBuf {
        policy_governance_ledger_path(&self.dir)
    }

    pub fn record_review(
        &self,
        mut review: PolicyGovernanceReview,
    ) -> io::Result<PolicyGovernanceReview> {
        review.ledger_path = self.ledger_path();
        self.record_entry(review.ledger_entry.clone())?;
        Ok(review)
    }

    pub fn record_entry(&self, entry: PolicyLedgerEntry) -> io::Result<PolicyLedgerEntry> {
        fs::create_dir_all(&self.dir)?;
        let line = serde_json::to_string(&entry)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.ledger_path())?;
        writeln!(file, "{line}")?;
        Ok(entry)
    }

    pub fn record_apply_plan(&self, plan: &PolicyApplyPlan) -> io::Result<PolicyLedgerEntry> {
        self.record_entry(policy_apply_plan_ledger_entry(plan))
    }

    pub fn record_apply_report(&self, report: &PolicyApplyReport) -> io::Result<PolicyLedgerEntry> {
        self.record_entry(policy_apply_report_ledger_entry(report))
    }

    pub fn record_rollback_report(
        &self,
        report: &PolicyRollbackReport,
    ) -> io::Result<PolicyLedgerEntry> {
        self.record_entry(policy_rollback_report_ledger_entry(report))
    }

    pub fn load(&self, limit: usize) -> io::Result<PolicyLedgerLoad> {
        load_policy_governance_ledger(&self.dir, limit)
    }
}

pub fn policy_governance_ledger_path(dir: &Path) -> PathBuf {
    dir.join(POLICY_GOVERNANCE_LEDGER_FILE)
}

pub fn load_policy_governance_ledger(dir: &Path, limit: usize) -> io::Result<PolicyLedgerLoad> {
    let ledger_path = policy_governance_ledger_path(dir);
    if !ledger_path.exists() {
        return Ok(PolicyLedgerLoad {
            ledger_path,
            entries: Vec::new(),
            malformed_lines: 0,
            warnings: Vec::new(),
        });
    }
    let contents = fs::read_to_string(&ledger_path)?;
    let mut entries = Vec::new();
    let mut malformed_lines = 0_usize;
    let mut warnings = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<PolicyLedgerEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(error) => {
                malformed_lines += 1;
                warnings.push(PolicyLedgerWarning {
                    line: index.saturating_add(1),
                    message: error.to_string(),
                });
            }
        }
    }
    if limit > 0 && entries.len() > limit {
        let start = entries.len().saturating_sub(limit);
        entries = entries.into_iter().skip(start).collect();
    }
    Ok(PolicyLedgerLoad {
        ledger_path,
        entries,
        malformed_lines,
        warnings,
    })
}

pub fn replay_policy_lifecycle(dir: &Path, limit: usize) -> io::Result<PolicyLifecycleReplay> {
    let load = load_policy_governance_ledger(dir, limit)?;
    Ok(replay_policy_lifecycle_entries(load))
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
    pub adapter_report: Option<PolicyAdapterReport>,
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
    pub adapter_report: Option<PolicyAdapterReport>,
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
#[serde(tag = "kind", content = "report", rename_all = "snake_case")]
pub enum PolicyAdapterReport {
    RoutingApply(RoutingPolicyApplyReport),
    RoutingRollback(RoutingPolicyRollbackReport),
    SchedulerDryRun(SchedulerPolicyDryRunReport),
}

#[derive(Debug, Clone)]
struct PolicyAdapterValidation {
    proposal_id: String,
    before_status: PolicyLedgerStatus,
}

#[derive(Debug, Clone)]
struct PolicyAdapterApplyResult {
    status: PolicyLedgerStatus,
    applied: bool,
    executed: bool,
    after_status: Option<PolicyLedgerStatus>,
    adapter_report_id: Option<String>,
    adapter_report: Option<PolicyAdapterReport>,
    routing_report: Option<RoutingPolicyApplyReport>,
    blockers: Vec<PolicyBlocker>,
    recommendations: Vec<String>,
}

#[derive(Debug, Clone)]
struct PolicyAdapterRollbackResult {
    status: PolicyLedgerStatus,
    rolled_back: bool,
    executed: bool,
    after_status: Option<PolicyLedgerStatus>,
    adapter_report_id: Option<String>,
    adapter_report: Option<PolicyAdapterReport>,
    routing_report: Option<RoutingPolicyRollbackReport>,
    blockers: Vec<PolicyBlocker>,
    recommendations: Vec<String>,
}

trait PolicyAdapter {
    fn descriptor(&self) -> PolicyAdapterDescriptor;

    fn validate(
        &self,
        action: &PolicyApplyAction,
        operation: PolicyApplyOperation,
    ) -> io::Result<Result<PolicyAdapterValidation, Vec<PolicyBlocker>>>;

    fn apply(
        &self,
        validation: &PolicyAdapterValidation,
        dry_run: bool,
    ) -> io::Result<PolicyAdapterApplyResult>;

    fn rollback(
        &self,
        validation: &PolicyAdapterValidation,
    ) -> io::Result<PolicyAdapterRollbackResult>;
}

#[derive(Debug, Clone)]
struct RoutingPolicyAdapter<'a> {
    store: &'a RoutingPolicyProposalStore,
}

impl<'a> RoutingPolicyAdapter<'a> {
    const NAME: &'static str = "routing_policy";

    fn new(store: &'a RoutingPolicyProposalStore) -> Self {
        Self { store }
    }

    fn descriptor_static() -> PolicyAdapterDescriptor {
        PolicyAdapterDescriptor {
            name: Self::NAME.to_string(),
            domain: PolicyDomain::Routing,
            supports_apply: true,
            supports_persistent_apply: true,
            supports_dry_run: true,
            supports_rollback: true,
            planned_only: false,
            reason: "routing policy adapter can apply and rollback MoE routing overlays"
                .to_string(),
        }
    }
}

impl PolicyAdapter for RoutingPolicyAdapter<'_> {
    fn descriptor(&self) -> PolicyAdapterDescriptor {
        Self::descriptor_static()
    }

    fn validate(
        &self,
        action: &PolicyApplyAction,
        operation: PolicyApplyOperation,
    ) -> io::Result<Result<PolicyAdapterValidation, Vec<PolicyBlocker>>> {
        let Some(proposal_id) = action.proposal_id.as_deref() else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::MissingTarget,
                Some(PolicyDomain::Routing),
                None,
                PolicyRiskLevel::High,
                "routing policy action is missing a proposal id",
                Self::NAME,
            )]));
        };
        let snapshot = self.store.load()?;
        let Some(proposal) = snapshot
            .proposals
            .iter()
            .find(|proposal| proposal.id == proposal_id)
        else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::MissingProposal,
                Some(PolicyDomain::Routing),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                format!("routing policy proposal not found: {proposal_id}"),
                Self::NAME,
            )]));
        };
        let Some(current_action) = routing_action_snapshot(proposal, operation) else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::AdapterRejected,
                Some(PolicyDomain::Routing),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                "routing policy proposal cannot be projected into a governed action",
                Self::NAME,
            )]));
        };
        if current_action.source_fingerprint != action.source_fingerprint {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::StalePlan,
                Some(PolicyDomain::Routing),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                format!(
                    "routing proposal state changed since plan creation: planned={} current={}",
                    action.source_fingerprint, current_action.source_fingerprint
                ),
                Self::NAME,
            )]));
        }
        Ok(Ok(PolicyAdapterValidation {
            proposal_id: proposal_id.to_string(),
            before_status: map_routing_status(proposal.status),
        }))
    }

    fn apply(
        &self,
        validation: &PolicyAdapterValidation,
        dry_run: bool,
    ) -> io::Result<PolicyAdapterApplyResult> {
        let routing_report = self.store.apply(&validation.proposal_id, dry_run)?;
        let mut blockers = routing_report
            .blockers
            .iter()
            .map(|blocker| {
                policy_blocker(
                    PolicyBlockerKind::AdapterRejected,
                    Some(PolicyDomain::Routing),
                    Some(validation.proposal_id.clone()),
                    PolicyRiskLevel::High,
                    blocker.clone(),
                    Self::NAME,
                )
            })
            .collect::<Vec<_>>();
        let (status, applied) = if !blockers.is_empty() {
            (PolicyLedgerStatus::ApplyBlocked, false)
        } else if dry_run {
            (PolicyLedgerStatus::DryRunPassed, false)
        } else if routing_report.applied {
            (PolicyLedgerStatus::Applied, true)
        } else {
            blockers.push(policy_blocker(
                PolicyBlockerKind::AdapterRejected,
                Some(PolicyDomain::Routing),
                Some(validation.proposal_id.clone()),
                PolicyRiskLevel::High,
                "routing policy store did not apply the proposal",
                Self::NAME,
            ));
            (PolicyLedgerStatus::ApplyBlocked, false)
        };
        let mut recommendations = routing_report.recommendations.clone();
        if status == PolicyLedgerStatus::DryRunPassed {
            recommendations.push(
                "Governance dry run passed; rerun with --domain routing --proposal-id to apply."
                    .to_string(),
            );
        }
        Ok(PolicyAdapterApplyResult {
            status,
            applied,
            executed: applied || dry_run && status == PolicyLedgerStatus::DryRunPassed,
            after_status: Some(map_routing_status(routing_report.status)),
            adapter_report_id: Some(format!(
                "routing_policy_apply:{}",
                routing_report.proposal_id
            )),
            adapter_report: Some(PolicyAdapterReport::RoutingApply(routing_report.clone())),
            routing_report: Some(routing_report),
            blockers,
            recommendations,
        })
    }

    fn rollback(
        &self,
        validation: &PolicyAdapterValidation,
    ) -> io::Result<PolicyAdapterRollbackResult> {
        let routing_report = self.store.rollback(&validation.proposal_id)?;
        let status = if routing_report.rolled_back {
            PolicyLedgerStatus::RolledBack
        } else {
            PolicyLedgerStatus::RollbackBlocked
        };
        let blockers = if routing_report.rolled_back {
            Vec::new()
        } else {
            routing_report
                .recommendations
                .iter()
                .map(|recommendation| {
                    policy_blocker(
                        PolicyBlockerKind::AdapterRejected,
                        Some(PolicyDomain::Routing),
                        Some(validation.proposal_id.clone()),
                        PolicyRiskLevel::High,
                        recommendation.clone(),
                        Self::NAME,
                    )
                })
                .collect::<Vec<_>>()
        };
        Ok(PolicyAdapterRollbackResult {
            status,
            rolled_back: routing_report.rolled_back,
            executed: routing_report.rolled_back,
            after_status: Some(map_routing_status(routing_report.status)),
            adapter_report_id: Some(format!(
                "routing_policy_rollback:{}",
                routing_report.proposal_id
            )),
            adapter_report: Some(PolicyAdapterReport::RoutingRollback(routing_report.clone())),
            routing_report: Some(routing_report.clone()),
            blockers,
            recommendations: routing_report.recommendations,
        })
    }
}

#[derive(Debug, Clone)]
struct SchedulerPolicyAdapter {
    scheduler_status: Option<SchedulerDaemonStatus>,
}

impl SchedulerPolicyAdapter {
    const NAME: &'static str = "scheduler_policy";

    fn new(scheduler_status: Option<SchedulerDaemonStatus>) -> Self {
        Self { scheduler_status }
    }

    fn descriptor_static() -> PolicyAdapterDescriptor {
        PolicyAdapterDescriptor {
            name: Self::NAME.to_string(),
            domain: PolicyDomain::Scheduler,
            supports_apply: true,
            supports_persistent_apply: false,
            supports_dry_run: true,
            supports_rollback: false,
            planned_only: false,
            reason: "scheduler policy adapter supports dry-run validation only".to_string(),
        }
    }
}

impl PolicyAdapter for SchedulerPolicyAdapter {
    fn descriptor(&self) -> PolicyAdapterDescriptor {
        Self::descriptor_static()
    }

    fn validate(
        &self,
        action: &PolicyApplyAction,
        operation: PolicyApplyOperation,
    ) -> io::Result<Result<PolicyAdapterValidation, Vec<PolicyBlocker>>> {
        let Some(proposal_id) = action.proposal_id.as_deref() else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::MissingTarget,
                Some(PolicyDomain::Scheduler),
                None,
                PolicyRiskLevel::High,
                "scheduler policy action is missing a proposal id",
                Self::NAME,
            )]));
        };
        if operation != PolicyApplyOperation::Apply {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::UnsupportedDomain,
                Some(PolicyDomain::Scheduler),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::Medium,
                "scheduler policy rollback is not executable",
                Self::NAME,
            )]));
        }
        if action.domain != PolicyDomain::Scheduler {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::UnsupportedDomain,
                Some(action.domain),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                "scheduler adapter received a non-scheduler policy action",
                Self::NAME,
            )]));
        }
        Ok(Ok(PolicyAdapterValidation {
            proposal_id: proposal_id.to_string(),
            before_status: action.status,
        }))
    }

    fn apply(
        &self,
        validation: &PolicyAdapterValidation,
        dry_run: bool,
    ) -> io::Result<PolicyAdapterApplyResult> {
        if !dry_run {
            let blocker = policy_blocker(
                PolicyBlockerKind::UnsupportedDomain,
                Some(PolicyDomain::Scheduler),
                Some(validation.proposal_id.clone()),
                PolicyRiskLevel::High,
                "scheduler policy adapter is dry-run only; persistent apply is not supported",
                Self::NAME,
            );
            return Ok(PolicyAdapterApplyResult {
                status: PolicyLedgerStatus::ApplyBlocked,
                applied: false,
                executed: false,
                after_status: Some(PolicyLedgerStatus::ApplyBlocked),
                adapter_report_id: Some(format!(
                    "scheduler_policy_apply_blocked:{}",
                    validation.proposal_id
                )),
                adapter_report: Some(PolicyAdapterReport::SchedulerDryRun(
                    SchedulerPolicyDryRunReport {
                        proposal_id: validation.proposal_id.clone(),
                        dry_run,
                        status: PolicyLedgerStatus::ApplyBlocked,
                        scheduler_status: self.scheduler_status,
                        blockers: vec![blocker.clone()],
                        recommendations: vec![
                            "Keep scheduler policy changes in governance review until a persistent adapter is available."
                                .to_string(),
                        ],
                    },
                )),
                routing_report: None,
                blockers: vec![blocker],
                recommendations: vec![
                    "Scheduler policy dry-run adapter rejected persistent apply.".to_string(),
                ],
            });
        }
        let recommendations = vec![
            "Scheduler policy dry run passed; no daemon state or scheduler configuration was changed."
                .to_string(),
        ];
        let report = SchedulerPolicyDryRunReport {
            proposal_id: validation.proposal_id.clone(),
            dry_run,
            status: PolicyLedgerStatus::DryRunPassed,
            scheduler_status: self.scheduler_status,
            blockers: Vec::new(),
            recommendations: recommendations.clone(),
        };
        Ok(PolicyAdapterApplyResult {
            status: PolicyLedgerStatus::DryRunPassed,
            applied: false,
            executed: true,
            after_status: Some(PolicyLedgerStatus::DryRunPassed),
            adapter_report_id: Some(format!(
                "scheduler_policy_dry_run:{}",
                validation.proposal_id
            )),
            adapter_report: Some(PolicyAdapterReport::SchedulerDryRun(report)),
            routing_report: None,
            blockers: Vec::new(),
            recommendations,
        })
    }

    fn rollback(
        &self,
        validation: &PolicyAdapterValidation,
    ) -> io::Result<PolicyAdapterRollbackResult> {
        let blocker = policy_blocker(
            PolicyBlockerKind::UnsupportedDomain,
            Some(PolicyDomain::Scheduler),
            Some(validation.proposal_id.clone()),
            PolicyRiskLevel::High,
            "scheduler policy rollback is not supported",
            Self::NAME,
        );
        Ok(PolicyAdapterRollbackResult {
            status: PolicyLedgerStatus::RollbackBlocked,
            rolled_back: false,
            executed: false,
            after_status: Some(PolicyLedgerStatus::RollbackBlocked),
            adapter_report_id: None,
            adapter_report: None,
            routing_report: None,
            blockers: vec![blocker],
            recommendations: vec![
                "Scheduler policy rollback remains governance-planned only.".to_string()
            ],
        })
    }
}

pub fn policy_adapter_descriptors(
    scheduler_status: Option<SchedulerDaemonStatus>,
) -> Vec<PolicyAdapterDescriptor> {
    vec![
        RoutingPolicyAdapter::descriptor_static(),
        SchedulerPolicyAdapter::new(scheduler_status).descriptor(),
        planned_only_adapter_descriptor(PolicyDomain::AutonomousRun),
        planned_only_adapter_descriptor(PolicyDomain::Memory),
        planned_only_adapter_descriptor(PolicyDomain::Recovery),
    ]
}

fn planned_only_adapter_descriptor(domain: PolicyDomain) -> PolicyAdapterDescriptor {
    let name = match domain {
        PolicyDomain::Routing => "routing_policy",
        PolicyDomain::AutonomousRun => "autonomous_run_policy",
        PolicyDomain::Scheduler => "scheduler_policy",
        PolicyDomain::Memory => "memory_policy",
        PolicyDomain::Recovery => "recovery_policy",
    };
    PolicyAdapterDescriptor {
        name: name.to_string(),
        domain,
        supports_apply: false,
        supports_persistent_apply: false,
        supports_dry_run: false,
        supports_rollback: false,
        planned_only: true,
        reason: format!(
            "{domain:?} policy remains governance-planned until an adapter is registered"
        ),
    }
}

#[derive(Debug, Clone)]
pub struct PolicyApplyCoordinator {
    routing_store: RoutingPolicyProposalStore,
}

impl PolicyApplyCoordinator {
    #[must_use]
    pub fn new(routing_store: RoutingPolicyProposalStore) -> Self {
        Self { routing_store }
    }

    #[must_use]
    pub fn plan_apply(
        &self,
        review: PolicyGovernanceReview,
        domain_filter: Option<PolicyDomain>,
        proposal_id: Option<&str>,
        dry_run: bool,
    ) -> PolicyApplyPlan {
        build_policy_apply_plan(
            review,
            PolicyApplyOperation::Apply,
            domain_filter,
            proposal_id,
            dry_run,
        )
    }

    #[must_use]
    pub fn plan_rollback(
        &self,
        review: PolicyGovernanceReview,
        domain_filter: Option<PolicyDomain>,
        proposal_id: Option<&str>,
    ) -> PolicyApplyPlan {
        build_policy_apply_plan(
            review,
            PolicyApplyOperation::Rollback,
            domain_filter,
            proposal_id,
            false,
        )
    }

    pub fn apply(&self, plan: &PolicyApplyPlan) -> io::Result<PolicyApplyReport> {
        if plan.operation != PolicyApplyOperation::Apply {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "policy apply requires an apply plan",
            ));
        }
        let mut structured_blockers = plan.structured_blockers.clone();
        structured_blockers.extend(
            plan.actions
                .iter()
                .flat_map(|action| action.structured_blockers.clone()),
        );
        if !plan.dry_run && (plan.domain_filter.is_none() || plan.proposal_id.is_none()) {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::MissingTarget,
                plan.domain_filter,
                plan.proposal_id.clone(),
                PolicyRiskLevel::High,
                "governed policy apply requires explicit --domain and --proposal-id".to_string(),
                "governance_apply",
            ));
        }
        let executable_actions = executable_policy_actions(plan);
        if executable_actions.is_empty() {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::MissingTarget,
                plan.domain_filter,
                plan.proposal_id.clone(),
                PolicyRiskLevel::High,
                "no executable policy apply action is available",
                "governance_apply",
            ));
        } else if executable_actions.len() > 1 {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::AmbiguousTarget,
                plan.domain_filter,
                None,
                PolicyRiskLevel::High,
                "multiple executable policy actions are available; specify --domain and --proposal-id"
                    .to_string(),
                "governance_apply",
            ));
        }
        if !structured_blockers.is_empty() {
            return Ok(blocked_apply_report(
                plan,
                structured_blockers,
                "Resolve governance blockers before applying policy changes.",
            ));
        }

        let action = executable_actions[0];
        let Some(adapter) = self.adapter_for_domain(action.domain, plan) else {
            return Ok(blocked_apply_report(
                plan,
                vec![policy_blocker(
                    PolicyBlockerKind::UnsupportedDomain,
                    Some(action.domain),
                    action.proposal_id.clone(),
                    PolicyRiskLevel::High,
                    format!("{:?} policy adapter is not registered", action.domain),
                    "policy_adapter_registry",
                )],
                "Register a policy adapter before applying this domain.",
            ));
        };
        let descriptor = adapter.descriptor();
        let validated = match adapter.validate(action, PolicyApplyOperation::Apply)? {
            Ok(validated) => validated,
            Err(blockers) => {
                return Ok(blocked_apply_report(
                    plan,
                    blockers,
                    "Refresh the policy plan before applying changed routing policy state.",
                ));
            }
        };
        let adapter_result = adapter.apply(&validated, plan.dry_run)?;
        let blockers = blocker_messages(&adapter_result.blockers);
        let report_id = format!("policy-apply-{}", now_millis());
        let receipt = policy_action_receipt(
            report_id.clone(),
            plan,
            adapter_result.status,
            adapter_result.executed,
            Some(validated.before_status),
            adapter_result.after_status,
            Some(descriptor.name),
            adapter_result.adapter_report_id.clone(),
            adapter_result.blockers.clone(),
            adapter_result.recommendations.clone(),
        );
        Ok(PolicyApplyReport {
            version: POLICY_GOVERNANCE_VERSION,
            id: report_id,
            executed_at: now_secs(),
            dry_run: plan.dry_run,
            status: adapter_result.status,
            applied: adapter_result.applied,
            plan: plan.clone(),
            adapter_report: adapter_result.adapter_report,
            routing_report: adapter_result.routing_report,
            blockers,
            structured_blockers: adapter_result.blockers,
            receipt,
            recommendations: adapter_result.recommendations,
        })
    }

    pub fn rollback(&self, plan: &PolicyApplyPlan) -> io::Result<PolicyRollbackReport> {
        if plan.operation != PolicyApplyOperation::Rollback {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "policy rollback requires a rollback plan",
            ));
        }
        let mut structured_blockers = plan.structured_blockers.clone();
        structured_blockers.extend(
            plan.actions
                .iter()
                .flat_map(|action| action.structured_blockers.clone()),
        );
        if plan.domain_filter != Some(PolicyDomain::Routing) || plan.proposal_id.is_none() {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::MissingTarget,
                Some(PolicyDomain::Routing),
                plan.proposal_id.clone(),
                PolicyRiskLevel::High,
                "governed policy rollback requires explicit --domain routing and --proposal-id"
                    .to_string(),
                "governance_rollback",
            ));
        }
        let executable_actions = executable_policy_actions(plan);
        if executable_actions.is_empty() {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::MissingTarget,
                plan.domain_filter,
                plan.proposal_id.clone(),
                PolicyRiskLevel::High,
                "no executable policy rollback action is available",
                "governance_rollback",
            ));
        } else if executable_actions.len() > 1 {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::AmbiguousTarget,
                plan.domain_filter,
                None,
                PolicyRiskLevel::High,
                "multiple executable policy rollback actions are available; specify --domain and --proposal-id"
                    .to_string(),
                "governance_rollback",
            ));
        }
        if !structured_blockers.is_empty() {
            return Ok(blocked_rollback_report(
                plan,
                structured_blockers,
                "Resolve governance blockers before rolling back policy changes.",
            ));
        }

        let action = executable_actions[0];
        let Some(adapter) = self.adapter_for_domain(action.domain, plan) else {
            return Ok(blocked_rollback_report(
                plan,
                vec![policy_blocker(
                    PolicyBlockerKind::UnsupportedDomain,
                    Some(action.domain),
                    action.proposal_id.clone(),
                    PolicyRiskLevel::High,
                    format!("{:?} policy adapter is not registered", action.domain),
                    "policy_adapter_registry",
                )],
                "Register a policy adapter before rolling back this domain.",
            ));
        };
        let descriptor = adapter.descriptor();
        let validated = match adapter.validate(action, PolicyApplyOperation::Rollback)? {
            Ok(validated) => validated,
            Err(blockers) => {
                return Ok(blocked_rollback_report(
                    plan,
                    blockers,
                    "Refresh the policy plan before rolling back changed routing policy state.",
                ));
            }
        };
        let adapter_result = adapter.rollback(&validated)?;
        let blockers = blocker_messages(&adapter_result.blockers);
        let report_id = format!("policy-rollback-{}", now_millis());
        let receipt = policy_action_receipt(
            report_id.clone(),
            plan,
            adapter_result.status,
            adapter_result.executed,
            Some(validated.before_status),
            adapter_result.after_status,
            Some(descriptor.name),
            adapter_result.adapter_report_id.clone(),
            adapter_result.blockers.clone(),
            adapter_result.recommendations.clone(),
        );
        Ok(PolicyRollbackReport {
            version: POLICY_GOVERNANCE_VERSION,
            id: report_id,
            executed_at: now_secs(),
            status: adapter_result.status,
            rolled_back: adapter_result.rolled_back,
            plan: plan.clone(),
            adapter_report: adapter_result.adapter_report,
            routing_report: adapter_result.routing_report,
            blockers,
            structured_blockers: adapter_result.blockers,
            receipt,
            recommendations: adapter_result.recommendations,
        })
    }

    fn adapter_for_domain(
        &self,
        domain: PolicyDomain,
        plan: &PolicyApplyPlan,
    ) -> Option<Box<dyn PolicyAdapter + '_>> {
        match domain {
            PolicyDomain::Routing => Some(Box::new(RoutingPolicyAdapter::new(&self.routing_store))),
            PolicyDomain::Scheduler => Some(Box::new(SchedulerPolicyAdapter::new(
                plan.review.ledger_entry.summary.scheduler_status,
            ))),
            PolicyDomain::AutonomousRun | PolicyDomain::Memory | PolicyDomain::Recovery => None,
        }
    }
}

pub fn policy_apply_plan_ledger_entry(plan: &PolicyApplyPlan) -> PolicyLedgerEntry {
    policy_apply_ledger_entry(
        plan.id.clone(),
        plan.planned_at,
        plan.status,
        plan,
        plan.blockers.clone(),
        plan.recommendations.clone(),
    )
}

pub fn policy_apply_report_ledger_entry(report: &PolicyApplyReport) -> PolicyLedgerEntry {
    policy_apply_ledger_entry(
        report.id.clone(),
        report.executed_at,
        report.status,
        &report.plan,
        report.blockers.clone(),
        report.recommendations.clone(),
    )
}

pub fn policy_rollback_report_ledger_entry(report: &PolicyRollbackReport) -> PolicyLedgerEntry {
    policy_apply_ledger_entry(
        report.id.clone(),
        report.executed_at,
        report.status,
        &report.plan,
        report.blockers.clone(),
        report.recommendations.clone(),
    )
}

fn build_policy_apply_plan(
    review: PolicyGovernanceReview,
    operation: PolicyApplyOperation,
    domain_filter: Option<PolicyDomain>,
    proposal_id: Option<&str>,
    dry_run: bool,
) -> PolicyApplyPlan {
    let proposal_id = proposal_id.map(str::to_string);
    let adapters = policy_adapter_descriptors(review.ledger_entry.summary.scheduler_status);
    let mut structured_blockers = Vec::new();
    if operation == PolicyApplyOperation::Apply {
        structured_blockers.extend(
            review
                .ledger_entry
                .gates
                .iter()
                .filter(|gate| !gate.passed && gate.blocks_apply)
                .map(|gate| {
                    policy_blocker(
                        PolicyBlockerKind::GateFailed,
                        Some(gate.domain),
                        None,
                        gate.severity,
                        format!("{} gate failed: {}", gate.name, gate.reason),
                        "governance_gate",
                    )
                }),
        );
        structured_blockers.extend(
            review
                .ledger_entry
                .conflicts
                .iter()
                .filter(|conflict| conflict.blocks_apply)
                .map(|conflict| {
                    policy_blocker(
                        PolicyBlockerKind::ConflictBlocked,
                        conflict.domains.first().copied(),
                        None,
                        conflict.severity,
                        format!("blocking policy conflict: {}", conflict.reason),
                        "governance_conflict",
                    )
                }),
        );
    }

    let mut actions = Vec::new();
    for proposal in &review.ledger_entry.proposals {
        if !domain_filter.map_or(true, |domain| domain == proposal.domain) {
            continue;
        }
        if !proposal_id
            .as_deref()
            .map_or(true, |wanted| wanted == proposal.id)
        {
            continue;
        }
        actions.push(policy_apply_action_from_proposal(proposal, operation));
    }

    if operation == PolicyApplyOperation::Apply {
        for decision in &review.ledger_entry.decisions {
            if !matches!(
                decision.status,
                PolicyLedgerStatus::Proposed | PolicyLedgerStatus::Approved
            ) {
                continue;
            }
            if !domain_filter.map_or(true, |domain| domain == decision.domain) {
                continue;
            }
            actions.push(policy_apply_action_from_decision(decision, operation));
        }
    }

    if let Some(wanted) = &proposal_id {
        if !actions
            .iter()
            .any(|action| action.proposal_id.as_deref() == Some(wanted.as_str()))
        {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::MissingProposal,
                domain_filter,
                Some(wanted.clone()),
                PolicyRiskLevel::High,
                format!("policy proposal not found in governance review: {wanted}"),
                "governance_review",
            ));
        }
    }

    let blockers = blocker_messages(&structured_blockers);
    let has_executable = actions
        .iter()
        .any(|action| action.executable && action.blockers.is_empty());
    let status = if !blockers.is_empty() {
        match operation {
            PolicyApplyOperation::Apply => PolicyLedgerStatus::ApplyBlocked,
            PolicyApplyOperation::Rollback => PolicyLedgerStatus::RollbackBlocked,
        }
    } else if has_executable || !actions.is_empty() {
        match operation {
            PolicyApplyOperation::Apply => PolicyLedgerStatus::Planned,
            PolicyApplyOperation::Rollback => PolicyLedgerStatus::RollbackPlanned,
        }
    } else {
        PolicyLedgerStatus::Observed
    };

    let mut recommendations = Vec::new();
    if blockers.is_empty() {
        match operation {
            PolicyApplyOperation::Apply if has_executable && dry_run => recommendations.push(
                "Governed dry run is ready; no policy overlay will be persisted.".to_string(),
            ),
            PolicyApplyOperation::Apply if has_executable => {
                recommendations.push("Governed policy apply is ready for execution.".to_string())
            }
            PolicyApplyOperation::Rollback if has_executable => {
                recommendations.push("Governed policy rollback is ready for execution.".to_string())
            }
            _ => {
                recommendations.push("No executable governed policy action is pending.".to_string())
            }
        }
    } else {
        recommendations
            .push("Resolve apply blockers before executing governed policy actions.".to_string());
    }

    let mut plan = PolicyApplyPlan {
        version: POLICY_GOVERNANCE_VERSION,
        id: format!("policy-plan-{}", now_millis()),
        planned_at: now_secs(),
        operation,
        dry_run,
        status,
        review_status: review.ledger_entry.status,
        domain_filter,
        proposal_id,
        actions,
        adapters,
        blockers,
        structured_blockers,
        fingerprint: String::new(),
        recommendations,
        review,
    };
    plan.fingerprint = policy_plan_fingerprint(&plan);
    plan
}

fn policy_apply_action_from_proposal(
    proposal: &PolicyProposal,
    operation: PolicyApplyOperation,
) -> PolicyApplyAction {
    let mut structured_blockers = Vec::new();
    if operation == PolicyApplyOperation::Apply {
        structured_blockers.extend(
            proposal
                .gates
                .iter()
                .filter(|gate| !gate.passed && gate.blocks_apply)
                .map(|gate| {
                    policy_blocker(
                        PolicyBlockerKind::GateFailed,
                        Some(gate.domain),
                        Some(proposal.id.clone()),
                        gate.severity,
                        format!("{} gate failed: {}", gate.name, gate.reason),
                        "proposal_gate",
                    )
                }),
        );
    }

    let (action, status, executable) = match operation {
        PolicyApplyOperation::Apply => {
            if !matches!(
                proposal.status,
                PolicyLedgerStatus::Proposed | PolicyLedgerStatus::Approved
            ) {
                structured_blockers.push(policy_blocker(
                    PolicyBlockerKind::AdapterRejected,
                    Some(proposal.domain),
                    Some(proposal.id.clone()),
                    PolicyRiskLevel::High,
                    format!("proposal status {:?} cannot be applied", proposal.status),
                    "proposal_status",
                ));
            }
            let executable = policy_operation_supports_domain(operation, proposal.domain);
            (
                proposal.action.clone(),
                if structured_blockers.is_empty() && executable {
                    PolicyLedgerStatus::Planned
                } else {
                    PolicyLedgerStatus::ApplyBlocked
                },
                executable,
            )
        }
        PolicyApplyOperation::Rollback => {
            if proposal.status != PolicyLedgerStatus::Applied {
                structured_blockers.push(policy_blocker(
                    PolicyBlockerKind::AdapterRejected,
                    Some(proposal.domain),
                    Some(proposal.id.clone()),
                    PolicyRiskLevel::High,
                    format!(
                        "proposal status {:?} cannot be rolled back",
                        proposal.status
                    ),
                    "proposal_status",
                ));
            }
            let executable = policy_operation_supports_domain(operation, proposal.domain);
            (
                "rollback_routing_policy_overlay".to_string(),
                if structured_blockers.is_empty() && executable {
                    PolicyLedgerStatus::RollbackPlanned
                } else {
                    PolicyLedgerStatus::RollbackBlocked
                },
                executable,
            )
        }
    };

    if !executable {
        structured_blockers.push(policy_blocker(
            PolicyBlockerKind::UnsupportedDomain,
            Some(proposal.domain),
            Some(proposal.id.clone()),
            PolicyRiskLevel::Medium,
            format!(
                "{:?} policy action is governance-planned only for {:?}",
                proposal.domain, operation
            ),
            "policy_adapter",
        ));
    }

    let blockers = blocker_messages(&structured_blockers);
    PolicyApplyAction {
        domain: proposal.domain,
        operation,
        action,
        proposal_id: Some(proposal.id.clone()),
        status,
        risk: proposal.risk,
        executable,
        blockers,
        structured_blockers,
        source_fingerprint: if proposal.source_fingerprint.is_empty() {
            policy_proposal_fingerprint(proposal)
        } else {
            proposal.source_fingerprint.clone()
        },
        reason: proposal.summary.clone(),
        references: proposal.references.clone(),
    }
}

fn policy_apply_action_from_decision(
    decision: &PolicyDecision,
    operation: PolicyApplyOperation,
) -> PolicyApplyAction {
    let status = match operation {
        PolicyApplyOperation::Apply => PolicyLedgerStatus::ApplyBlocked,
        PolicyApplyOperation::Rollback => PolicyLedgerStatus::RollbackBlocked,
    };
    let structured_blockers = vec![policy_blocker(
        PolicyBlockerKind::UnsupportedDomain,
        Some(decision.domain),
        None,
        decision.risk,
        format!(
            "{:?} decision '{}' is governance-planned only; no executable adapter is registered",
            decision.domain, decision.action
        ),
        "policy_adapter",
    )];
    PolicyApplyAction {
        domain: decision.domain,
        operation,
        action: decision.action.clone(),
        proposal_id: None,
        status,
        risk: decision.risk,
        executable: false,
        blockers: blocker_messages(&structured_blockers),
        structured_blockers,
        source_fingerprint: stable_hash_json(&json!({
            "domain": decision.domain,
            "action": decision.action,
            "status": decision.status,
            "reason": decision.reason,
        })),
        reason: decision.reason.clone(),
        references: Vec::new(),
    }
}

fn policy_operation_supports_domain(operation: PolicyApplyOperation, domain: PolicyDomain) -> bool {
    matches!(
        (operation, domain),
        (PolicyApplyOperation::Apply, PolicyDomain::Routing)
            | (PolicyApplyOperation::Apply, PolicyDomain::Scheduler)
            | (PolicyApplyOperation::Rollback, PolicyDomain::Routing)
    )
}

fn executable_policy_actions(plan: &PolicyApplyPlan) -> Vec<&PolicyApplyAction> {
    plan.actions
        .iter()
        .filter(|action| action.executable && action.blockers.is_empty())
        .collect()
}

fn blocked_apply_report(
    plan: &PolicyApplyPlan,
    structured_blockers: Vec<PolicyBlocker>,
    recommendation: &str,
) -> PolicyApplyReport {
    let report_id = format!("policy-apply-{}", now_millis());
    let blockers = blocker_messages(&structured_blockers);
    let recommendations = vec![recommendation.to_string()];
    let receipt = policy_action_receipt(
        report_id.clone(),
        plan,
        PolicyLedgerStatus::ApplyBlocked,
        false,
        None,
        None,
        None,
        None,
        structured_blockers.clone(),
        recommendations.clone(),
    );
    PolicyApplyReport {
        version: POLICY_GOVERNANCE_VERSION,
        id: report_id,
        executed_at: now_secs(),
        dry_run: plan.dry_run,
        status: PolicyLedgerStatus::ApplyBlocked,
        applied: false,
        plan: plan.clone(),
        adapter_report: None,
        routing_report: None,
        blockers,
        structured_blockers,
        receipt,
        recommendations,
    }
}

fn blocked_rollback_report(
    plan: &PolicyApplyPlan,
    structured_blockers: Vec<PolicyBlocker>,
    recommendation: &str,
) -> PolicyRollbackReport {
    let report_id = format!("policy-rollback-{}", now_millis());
    let blockers = blocker_messages(&structured_blockers);
    let recommendations = vec![recommendation.to_string()];
    let receipt = policy_action_receipt(
        report_id.clone(),
        plan,
        PolicyLedgerStatus::RollbackBlocked,
        false,
        None,
        None,
        None,
        None,
        structured_blockers.clone(),
        recommendations.clone(),
    );
    PolicyRollbackReport {
        version: POLICY_GOVERNANCE_VERSION,
        id: report_id,
        executed_at: now_secs(),
        status: PolicyLedgerStatus::RollbackBlocked,
        rolled_back: false,
        plan: plan.clone(),
        adapter_report: None,
        routing_report: None,
        blockers,
        structured_blockers,
        receipt,
        recommendations,
    }
}

fn policy_action_receipt(
    id: String,
    plan: &PolicyApplyPlan,
    status: PolicyLedgerStatus,
    executed: bool,
    before_status: Option<PolicyLedgerStatus>,
    after_status: Option<PolicyLedgerStatus>,
    adapter: Option<String>,
    adapter_report_id: Option<String>,
    blockers: Vec<PolicyBlocker>,
    recommendations: Vec<String>,
) -> PolicyActionReceipt {
    PolicyActionReceipt {
        version: POLICY_GOVERNANCE_VERSION,
        id: format!("{id}-receipt"),
        plan_id: plan.id.clone(),
        plan_fingerprint: plan.fingerprint.clone(),
        operation: plan.operation,
        dry_run: plan.dry_run,
        domain: plan.domain_filter,
        proposal_id: plan.proposal_id.clone(),
        status,
        executed,
        before_status,
        after_status,
        adapter,
        adapter_report_id,
        ledger_entry_id: Some(id),
        blockers,
        recommendations,
    }
}

fn policy_blocker(
    kind: PolicyBlockerKind,
    domain: Option<PolicyDomain>,
    proposal_id: Option<String>,
    severity: PolicyRiskLevel,
    reason: impl Into<String>,
    source: impl Into<String>,
) -> PolicyBlocker {
    PolicyBlocker {
        kind,
        domain,
        proposal_id,
        severity,
        reason: reason.into(),
        source: source.into(),
    }
}

fn blocker_messages(blockers: &[PolicyBlocker]) -> Vec<String> {
    blockers
        .iter()
        .map(|blocker| blocker.reason.clone())
        .collect()
}

fn policy_plan_fingerprint(plan: &PolicyApplyPlan) -> String {
    stable_hash_json(&json!({
        "version": plan.version,
        "operation": plan.operation,
        "dry_run": plan.dry_run,
        "review_id": plan.review.ledger_entry.id,
        "review_status": plan.review_status,
        "domain_filter": plan.domain_filter,
        "proposal_id": plan.proposal_id,
        "actions": plan.actions.iter().map(|action| {
            json!({
                "domain": action.domain,
                "operation": action.operation,
                "proposal_id": action.proposal_id,
                "status": action.status,
                "source_fingerprint": action.source_fingerprint,
            })
        }).collect::<Vec<_>>(),
        "adapters": plan.adapters,
        "blockers": plan.structured_blockers,
    }))
}

fn policy_proposal_fingerprint(proposal: &PolicyProposal) -> String {
    stable_hash_json(&json!({
        "id": proposal.id,
        "domain": proposal.domain,
        "action": proposal.action,
        "status": proposal.status,
        "risk": proposal.risk,
        "summary": proposal.summary,
        "gates": proposal.gates,
        "references": proposal.references,
    }))
}

fn routing_proposal_fingerprint(proposal: &RoutingPolicyProposal) -> String {
    stable_hash_json(&json!({
        "id": proposal.id,
        "status": proposal.status,
        "updated_at": proposal.updated_at,
        "changes": proposal.changes,
        "gates": proposal.gates,
        "estimated_success_delta": proposal.estimated_success_delta,
        "recovery_reduction_estimate": proposal.recovery_reduction_estimate,
        "applied_at": proposal.applied_at,
        "rolled_back_at": proposal.rolled_back_at,
    }))
}

fn stable_hash_json(value: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn routing_action_snapshot(
    proposal: &RoutingPolicyProposal,
    operation: PolicyApplyOperation,
) -> Option<PolicyApplyAction> {
    let policy_proposal = routing_policy_proposal(proposal);
    Some(policy_apply_action_from_proposal(
        &policy_proposal,
        operation,
    ))
}

fn policy_apply_ledger_entry(
    id: String,
    timestamp: u64,
    status: PolicyLedgerStatus,
    plan: &PolicyApplyPlan,
    outcome_blockers: Vec<String>,
    recommendations: Vec<String>,
) -> PolicyLedgerEntry {
    let proposals = plan
        .actions
        .iter()
        .enumerate()
        .map(|(index, action)| PolicyProposal {
            id: action
                .proposal_id
                .clone()
                .unwrap_or_else(|| format!("{}-action-{}", plan.id, index.saturating_add(1))),
            domain: action.domain,
            action: action.action.clone(),
            status: action.status,
            risk: action.risk,
            summary: action.reason.clone(),
            source_fingerprint: action.source_fingerprint.clone(),
            references: action.references.clone(),
            gates: Vec::new(),
        })
        .collect::<Vec<_>>();
    let decisions = vec![PolicyDecision {
        domain: plan.domain_filter.unwrap_or(PolicyDomain::Routing),
        action: match plan.operation {
            PolicyApplyOperation::Apply if plan.dry_run => "governed_policy_apply_dry_run",
            PolicyApplyOperation::Apply => "governed_policy_apply",
            PolicyApplyOperation::Rollback => "governed_policy_rollback",
        }
        .to_string(),
        status,
        risk: plan
            .actions
            .iter()
            .map(|action| action.risk)
            .max()
            .unwrap_or(PolicyRiskLevel::Low),
        reason: if outcome_blockers.is_empty() {
            "governed policy coordinator recorded the action outcome".to_string()
        } else {
            outcome_blockers.join("; ")
        },
    }];
    let mut domains = plan
        .actions
        .iter()
        .map(|action| action.domain)
        .collect::<Vec<_>>();
    if let Some(domain) = plan.domain_filter {
        domains.push(domain);
    }
    domains.extend(
        plan.review
            .ledger_entry
            .conflicts
            .iter()
            .flat_map(|conflict| conflict.domains.clone()),
    );
    domains.sort();
    domains.dedup();
    let failed_gate_count = plan
        .review
        .ledger_entry
        .gates
        .iter()
        .filter(|gate| !gate.passed)
        .count();
    let blocking_conflict_count = plan
        .review
        .ledger_entry
        .conflicts
        .iter()
        .filter(|conflict| conflict.blocks_apply)
        .count();
    PolicyLedgerEntry {
        version: POLICY_GOVERNANCE_VERSION,
        id,
        timestamp,
        status,
        domains,
        proposals,
        decisions,
        gates: plan.review.ledger_entry.gates.clone(),
        conflicts: plan.review.ledger_entry.conflicts.clone(),
        recommendations,
        summary: PolicyReviewSummary {
            status,
            proposal_count: plan.actions.len(),
            decision_count: 1,
            gate_count: plan.review.ledger_entry.gates.len(),
            failed_gate_count,
            conflict_count: plan.review.ledger_entry.conflicts.len(),
            blocking_conflict_count,
            active_routing_policy: plan.review.ledger_entry.summary.active_routing_policy,
            routing_proposal_count: plan.review.ledger_entry.summary.routing_proposal_count,
            scheduler_status: plan.review.ledger_entry.summary.scheduler_status,
            autonomous_action: plan.review.ledger_entry.summary.autonomous_action,
            autonomous_review_required: plan.review.ledger_entry.summary.autonomous_review_required,
            memory_reuse_score: plan.review.ledger_entry.summary.memory_reuse_score,
        },
    }
}

fn replay_policy_lifecycle_entries(load: PolicyLedgerLoad) -> PolicyLifecycleReplay {
    let mut lifecycles = BTreeMap::<(PolicyDomain, String), PolicyLifecycle>::new();
    let mut anomalies = Vec::new();
    let mut event_count = 0_usize;

    for entry in &load.entries {
        let operation = policy_entry_operation(entry);
        for proposal in &entry.proposals {
            let key = (proposal.domain, proposal.id.clone());
            let lifecycle = lifecycles
                .entry(key.clone())
                .or_insert_with(|| PolicyLifecycle {
                    domain: proposal.domain,
                    proposal_id: proposal.id.clone(),
                    current_status: entry.status,
                    first_seen_at: entry.timestamp,
                    last_seen_at: entry.timestamp,
                    events: Vec::new(),
                });
            if entry.timestamp < lifecycle.last_seen_at {
                anomalies.push(PolicyReplayAnomaly {
                    entry_id: entry.id.clone(),
                    proposal_id: Some(proposal.id.clone()),
                    kind: "out_of_order".to_string(),
                    reason: "ledger entry timestamp is older than the previous lifecycle event"
                        .to_string(),
                });
            }
            if !lifecycle.events.is_empty() && lifecycle.current_status == entry.status {
                anomalies.push(PolicyReplayAnomaly {
                    entry_id: entry.id.clone(),
                    proposal_id: Some(proposal.id.clone()),
                    kind: "duplicate_status".to_string(),
                    reason: format!("status {:?} repeated without transition", entry.status),
                });
            }
            let seen = lifecycle
                .events
                .iter()
                .map(|event| event.status)
                .collect::<Vec<_>>();
            if matches!(
                entry.status,
                PolicyLedgerStatus::Applied | PolicyLedgerStatus::DryRunPassed
            ) && !seen.iter().any(|status| {
                matches!(
                    status,
                    PolicyLedgerStatus::Planned
                        | PolicyLedgerStatus::Approved
                        | PolicyLedgerStatus::Proposed
                )
            }) {
                anomalies.push(PolicyReplayAnomaly {
                    entry_id: entry.id.clone(),
                    proposal_id: Some(proposal.id.clone()),
                    kind: "apply_without_plan".to_string(),
                    reason: "apply-like event appeared before an observed plan/proposal"
                        .to_string(),
                });
            }
            if entry.status == PolicyLedgerStatus::RolledBack
                && !seen
                    .iter()
                    .any(|status| *status == PolicyLedgerStatus::Applied)
            {
                anomalies.push(PolicyReplayAnomaly {
                    entry_id: entry.id.clone(),
                    proposal_id: Some(proposal.id.clone()),
                    kind: "rollback_without_applied".to_string(),
                    reason: "rollback appeared before an applied lifecycle state".to_string(),
                });
            }
            if lifecycle.current_status == PolicyLedgerStatus::RolledBack
                && matches!(
                    entry.status,
                    PolicyLedgerStatus::Applied
                        | PolicyLedgerStatus::Planned
                        | PolicyLedgerStatus::DryRunPassed
                )
            {
                anomalies.push(PolicyReplayAnomaly {
                    entry_id: entry.id.clone(),
                    proposal_id: Some(proposal.id.clone()),
                    kind: "reopened_after_rollback".to_string(),
                    reason: "proposal lifecycle continued after rollback".to_string(),
                });
            }
            lifecycle.current_status = entry.status;
            lifecycle.last_seen_at = lifecycle.last_seen_at.max(entry.timestamp);
            lifecycle.events.push(PolicyLifecycleEvent {
                entry_id: entry.id.clone(),
                timestamp: entry.timestamp,
                status: entry.status,
                action: proposal.action.clone(),
                operation,
            });
            event_count += 1;
        }
    }

    let mut lifecycles = lifecycles.into_values().collect::<Vec<_>>();
    lifecycles.sort_by(|left, right| {
        left.domain
            .cmp(&right.domain)
            .then_with(|| left.proposal_id.cmp(&right.proposal_id))
    });
    PolicyLifecycleReplay {
        version: POLICY_GOVERNANCE_VERSION,
        ledger_path: load.ledger_path,
        entries_considered: load.entries.len(),
        malformed_lines: load.malformed_lines,
        summary: PolicyLifecycleReplaySummary {
            lifecycle_count: lifecycles.len(),
            event_count,
            anomaly_count: anomalies.len(),
            malformed_lines: load.malformed_lines,
        },
        lifecycles,
        anomalies,
    }
}

fn policy_entry_operation(entry: &PolicyLedgerEntry) -> Option<PolicyApplyOperation> {
    entry.decisions.iter().find_map(|decision| {
        if decision.action.contains("rollback") {
            Some(PolicyApplyOperation::Rollback)
        } else if decision.action.contains("apply") {
            Some(PolicyApplyOperation::Apply)
        } else {
            None
        }
    })
}

pub fn review_policy_governance(input: PolicyGovernanceInput) -> PolicyGovernanceReview {
    let mut proposals = routing_policy_proposals(&input.routing_proposals);
    let mut decisions = Vec::new();
    let mut gates = proposals
        .iter()
        .flat_map(|proposal| proposal.gates.clone())
        .collect::<Vec<_>>();

    if let Some(applied) = &input.applied_routing_policy {
        decisions.push(PolicyDecision {
            domain: PolicyDomain::Routing,
            action: "routing_policy_overlay_applied".to_string(),
            status: PolicyLedgerStatus::Applied,
            risk: if applied.changes.len() > 1 {
                PolicyRiskLevel::High
            } else {
                PolicyRiskLevel::Medium
            },
            reason: format!(
                "routing overlay from proposal {} has {} change(s)",
                applied.proposal_id,
                applied.changes.len()
            ),
        });
    }

    if let Some(evaluation) = &input.autonomous_evaluation {
        decisions.extend(autonomous_policy_decisions(evaluation));
        gates.extend(autonomous_policy_gates(evaluation));
        gates.extend(memory_policy_gates(evaluation));
        decisions.extend(recovery_policy_decisions(evaluation));
    }

    if let Some(state) = &input.scheduler_state {
        let proposal = scheduler_policy_proposal(state);
        gates.extend(proposal.gates.clone());
        proposals.push(proposal);
        decisions.push(PolicyDecision {
            domain: PolicyDomain::Scheduler,
            action: "scheduler_state_observed".to_string(),
            status: if state.status == SchedulerDaemonStatus::Blocked {
                PolicyLedgerStatus::Blocked
            } else {
                PolicyLedgerStatus::Observed
            },
            risk: if state.status == SchedulerDaemonStatus::Blocked {
                PolicyRiskLevel::High
            } else {
                PolicyRiskLevel::Low
            },
            reason: format!("scheduler daemon status is {:?}", state.status),
        });
        gates.push(PolicyGate {
            domain: PolicyDomain::Scheduler,
            name: "scheduler_not_blocked".to_string(),
            passed: state.status != SchedulerDaemonStatus::Blocked,
            severity: PolicyRiskLevel::High,
            blocks_apply: state.status == SchedulerDaemonStatus::Blocked,
            reason: format!("scheduler daemon status is {:?}", state.status),
        });
    }

    proposals.sort_by(|left, right| left.id.cmp(&right.id));
    let conflicts = policy_conflicts(&input, &proposals, &decisions);
    let recommendations = policy_recommendations(&proposals, &decisions, &gates, &conflicts);
    let status = review_status(&proposals, &gates, &conflicts);
    let domains = review_domains(&proposals, &decisions, &gates, &conflicts);
    let summary = PolicyReviewSummary {
        status,
        proposal_count: proposals.len(),
        decision_count: decisions.len(),
        gate_count: gates.len(),
        failed_gate_count: gates.iter().filter(|gate| !gate.passed).count(),
        conflict_count: conflicts.len(),
        blocking_conflict_count: conflicts
            .iter()
            .filter(|conflict| conflict.blocks_apply)
            .count(),
        active_routing_policy: input.applied_routing_policy.is_some(),
        routing_proposal_count: input.routing_proposals.len(),
        scheduler_status: input.scheduler_state.as_ref().map(|state| state.status),
        autonomous_action: input
            .autonomous_evaluation
            .as_ref()
            .map(|evaluation| evaluation.trace_replay.policy_recommendation.action),
        autonomous_review_required: input.autonomous_evaluation.as_ref().is_some_and(
            |evaluation| {
                evaluation
                    .trace_replay
                    .policy_recommendation
                    .review_required
            },
        ),
        memory_reuse_score: input
            .autonomous_evaluation
            .as_ref()
            .map(|evaluation| evaluation.scores.memory_reuse_score),
    };
    let reviewed_at = now_secs();
    let entry = PolicyLedgerEntry {
        version: POLICY_GOVERNANCE_VERSION,
        id: format!("policy-review-{}", now_millis()),
        timestamp: reviewed_at,
        status,
        domains,
        proposals,
        decisions,
        gates,
        conflicts,
        recommendations,
        summary,
    };

    PolicyGovernanceReview {
        version: POLICY_GOVERNANCE_VERSION,
        reviewed_at,
        ledger_path: PathBuf::new(),
        ledger_entry: entry,
    }
}

fn routing_policy_proposals(proposals: &[RoutingPolicyProposal]) -> Vec<PolicyProposal> {
    proposals.iter().map(routing_policy_proposal).collect()
}

fn routing_policy_proposal(proposal: &RoutingPolicyProposal) -> PolicyProposal {
    let gates = proposal
        .gates
        .iter()
        .map(|gate| PolicyGate {
            domain: PolicyDomain::Routing,
            name: gate.name.clone(),
            passed: gate.passed,
            severity: match gate.level {
                RoutingPolicySafetyLevel::Info => PolicyRiskLevel::Low,
                RoutingPolicySafetyLevel::Warning => PolicyRiskLevel::Medium,
                RoutingPolicySafetyLevel::Blocker => PolicyRiskLevel::High,
            },
            blocks_apply: !gate.passed && gate.level == RoutingPolicySafetyLevel::Blocker,
            reason: gate.reason.clone(),
        })
        .collect::<Vec<_>>();
    let risk = if gates.iter().any(|gate| gate.blocks_apply) {
        PolicyRiskLevel::High
    } else if proposal.changes.len() > 1 {
        PolicyRiskLevel::High
    } else if proposal.changes.is_empty() {
        PolicyRiskLevel::Low
    } else {
        PolicyRiskLevel::Medium
    };
    PolicyProposal {
        id: proposal.id.clone(),
        domain: PolicyDomain::Routing,
        action: "apply_routing_policy_overlay".to_string(),
        status: map_routing_status(proposal.status),
        risk,
        summary: format!(
            "{} route change(s), estimated success delta {:.0}%",
            proposal.changes.len(),
            proposal.estimated_success_delta * 100.0
        ),
        source_fingerprint: routing_proposal_fingerprint(proposal),
        references: vec![PolicyReference {
            label: "route_policy_proposal".to_string(),
            path: None,
            id: Some(proposal.id.clone()),
        }],
        gates,
    }
}

fn scheduler_policy_proposal(state: &SchedulerDaemonState) -> PolicyProposal {
    let needs_adjustment = state.status == SchedulerDaemonStatus::Blocked || state.tick_count >= 3;
    let status = if needs_adjustment {
        PolicyLedgerStatus::Proposed
    } else {
        PolicyLedgerStatus::Observed
    };
    let risk = match state.status {
        SchedulerDaemonStatus::Blocked => PolicyRiskLevel::High,
        SchedulerDaemonStatus::Running if state.tick_count >= 3 => PolicyRiskLevel::Medium,
        SchedulerDaemonStatus::Running => PolicyRiskLevel::Low,
        SchedulerDaemonStatus::Idle | SchedulerDaemonStatus::Stopped => PolicyRiskLevel::Low,
    };
    let summary = if state.status == SchedulerDaemonStatus::Blocked {
        format!(
            "scheduler daemon is blocked after {} tick(s): {}",
            state.tick_count, state.message
        )
    } else if state.tick_count >= 3 {
        format!(
            "scheduler daemon has run {} tick(s); dry-run policy adaptation before persistent changes",
            state.tick_count
        )
    } else {
        format!("scheduler daemon status is {:?}", state.status)
    };
    let gates = vec![PolicyGate {
        domain: PolicyDomain::Scheduler,
        name: "scheduler_adapter_dry_run_only".to_string(),
        passed: true,
        severity: PolicyRiskLevel::Medium,
        blocks_apply: false,
        reason:
            "scheduler policy adapter can validate proposed changes without mutating daemon state"
                .to_string(),
    }];
    PolicyProposal {
        id: format!("scheduler-policy-{}", state.tick_count),
        domain: PolicyDomain::Scheduler,
        action: "dry_run_scheduler_policy_adjustment".to_string(),
        status,
        risk,
        summary,
        source_fingerprint: stable_hash_json(&json!({
            "version": state.version,
            "status": state.status,
            "tick_count": state.tick_count,
            "updated_at": state.updated_at,
            "message": state.message,
            "last_tick": state.last_tick,
        })),
        references: vec![PolicyReference {
            label: "scheduler_daemon_state".to_string(),
            path: Some(state.lock_path.clone()),
            id: Some(format!("tick-{}", state.tick_count)),
        }],
        gates,
    }
}

fn autonomous_policy_decisions(evaluation: &AutonomousEvaluationReport) -> Vec<PolicyDecision> {
    let recommendation = &evaluation.trace_replay.policy_recommendation;
    let risk = if recommendation.cool_down {
        PolicyRiskLevel::Critical
    } else if recommendation.review_required {
        PolicyRiskLevel::High
    } else if recommendation.action != AutonomousPolicyAction::Continue {
        PolicyRiskLevel::Medium
    } else {
        PolicyRiskLevel::Low
    };
    vec![PolicyDecision {
        domain: PolicyDomain::AutonomousRun,
        action: recommendation.action_label().to_string(),
        status: if recommendation.review_required {
            PolicyLedgerStatus::Blocked
        } else if recommendation.action == AutonomousPolicyAction::Continue {
            PolicyLedgerStatus::Observed
        } else {
            PolicyLedgerStatus::Proposed
        },
        risk,
        reason: recommendation.reasons.join("; "),
    }]
}

fn autonomous_policy_gates(evaluation: &AutonomousEvaluationReport) -> Vec<PolicyGate> {
    let recommendation = &evaluation.trace_replay.policy_recommendation;
    vec![
        PolicyGate {
            domain: PolicyDomain::AutonomousRun,
            name: "autonomous_review_not_required".to_string(),
            passed: !recommendation.review_required,
            severity: if recommendation.cool_down {
                PolicyRiskLevel::Critical
            } else {
                PolicyRiskLevel::High
            },
            blocks_apply: recommendation.review_required,
            reason: recommendation.reasons.join("; "),
        },
        PolicyGate {
            domain: PolicyDomain::AutonomousRun,
            name: "autonomous_success_score".to_string(),
            passed: evaluation.scores.autonomous_success_score >= 0.5,
            severity: PolicyRiskLevel::Medium,
            blocks_apply: false,
            reason: format!(
                "autonomous_success_score={:.0}%",
                evaluation.scores.autonomous_success_score * 100.0
            ),
        },
    ]
}

fn memory_policy_gates(evaluation: &AutonomousEvaluationReport) -> Vec<PolicyGate> {
    vec![PolicyGate {
        domain: PolicyDomain::Memory,
        name: "memory_reuse_score".to_string(),
        passed: evaluation.scores.memory_reuse_score >= 0.4,
        severity: PolicyRiskLevel::Medium,
        blocks_apply: false,
        reason: format!(
            "memory_reuse_score={:.0}% with {} memory entrie(s)",
            evaluation.scores.memory_reuse_score * 100.0,
            evaluation.counters.task_memory_entries
        ),
    }]
}

fn recovery_policy_decisions(evaluation: &AutonomousEvaluationReport) -> Vec<PolicyDecision> {
    let action = if evaluation.counters.recovery_triggered_tasks == 0 {
        "recovery_policy_observed"
    } else if evaluation.counters.recovered_tasks < evaluation.counters.recovery_triggered_tasks {
        "improve_recovery_policy"
    } else {
        "recovery_policy_effective"
    };
    let status = if action == "improve_recovery_policy" {
        PolicyLedgerStatus::Proposed
    } else {
        PolicyLedgerStatus::Observed
    };
    vec![PolicyDecision {
        domain: PolicyDomain::Recovery,
        action: action.to_string(),
        status,
        risk: if status == PolicyLedgerStatus::Proposed {
            PolicyRiskLevel::Medium
        } else {
            PolicyRiskLevel::Low
        },
        reason: format!(
            "{} recovery-triggered task(s), {} recovered",
            evaluation.counters.recovery_triggered_tasks, evaluation.counters.recovered_tasks
        ),
    }]
}

fn policy_conflicts(
    input: &PolicyGovernanceInput,
    proposals: &[PolicyProposal],
    _decisions: &[PolicyDecision],
) -> Vec<PolicyConflict> {
    let mut conflicts = Vec::new();
    let autonomous_recommendation = input
        .autonomous_evaluation
        .as_ref()
        .map(|evaluation| &evaluation.trace_replay.policy_recommendation);
    let routing_has_changes = input.applied_routing_policy.is_some()
        || input
            .routing_proposals
            .iter()
            .any(|proposal| !proposal.changes.is_empty());
    if let Some(recommendation) = autonomous_recommendation {
        if recommendation.review_required && routing_has_changes {
            conflicts.push(PolicyConflict {
                domains: vec![PolicyDomain::AutonomousRun, PolicyDomain::Routing],
                severity: if recommendation.cool_down {
                    PolicyRiskLevel::Critical
                } else {
                    PolicyRiskLevel::High
                },
                blocks_apply: true,
                reason:
                    "autonomous policy requires review while routing policy changes are pending or active"
                        .to_string(),
            });
        }
    }
    if input
        .scheduler_state
        .as_ref()
        .is_some_and(|state| state.status == SchedulerDaemonStatus::Blocked)
        && input
            .autonomous_evaluation
            .as_ref()
            .is_some_and(|evaluation| evaluation.scores.memory_reuse_score < 0.4)
    {
        conflicts.push(PolicyConflict {
            domains: vec![PolicyDomain::Scheduler, PolicyDomain::Memory],
            severity: PolicyRiskLevel::Medium,
            blocks_apply: false,
            reason: "scheduler is blocked while memory reuse score is low".to_string(),
        });
    }
    if let Some(applied) = &input.applied_routing_policy {
        if proposals.iter().any(|proposal| {
            proposal.domain == PolicyDomain::Routing
                && matches!(
                    proposal.status,
                    PolicyLedgerStatus::Proposed | PolicyLedgerStatus::Approved
                )
                && proposal.id != applied.proposal_id
        }) {
            conflicts.push(PolicyConflict {
                domains: vec![PolicyDomain::Routing],
                severity: PolicyRiskLevel::Medium,
                blocks_apply: false,
                reason: format!(
                    "routing overlay {} is active while another routing proposal is open",
                    applied.proposal_id
                ),
            });
        }
    }
    conflicts
}

fn policy_recommendations(
    proposals: &[PolicyProposal],
    decisions: &[PolicyDecision],
    gates: &[PolicyGate],
    conflicts: &[PolicyConflict],
) -> Vec<String> {
    let mut recommendations = Vec::new();
    if conflicts.iter().any(|conflict| conflict.blocks_apply) {
        recommendations.push(
            "Resolve blocking policy conflicts before applying additional autonomous changes."
                .to_string(),
        );
    }
    let failed_blocking_gates = gates
        .iter()
        .filter(|gate| !gate.passed && gate.blocks_apply)
        .count();
    if failed_blocking_gates > 0 {
        recommendations.push(format!(
            "{failed_blocking_gates} blocking governance gate(s) failed; keep policy changes in review."
        ));
    }
    if proposals.iter().any(|proposal| {
        proposal.domain == PolicyDomain::Routing && proposal.status == PolicyLedgerStatus::Proposed
    }) {
        recommendations.push(
            "Review routing proposals through the governance ledger before applying overlays."
                .to_string(),
        );
    }
    if proposals.iter().any(|proposal| {
        proposal.domain == PolicyDomain::Scheduler
            && proposal.status == PolicyLedgerStatus::Proposed
    }) {
        recommendations.push(
            "Validate scheduler policy proposals with a governed dry run before changing daemon behavior."
                .to_string(),
        );
    }
    if decisions.iter().any(|decision| {
        decision.domain == PolicyDomain::AutonomousRun
            && decision.status == PolicyLedgerStatus::Blocked
    }) {
        recommendations.push(
            "Autonomous run policy is conservative; inspect daemon history before increasing autonomy."
                .to_string(),
        );
    }
    if recommendations.is_empty() {
        recommendations.push("No cross-policy governance action is required.".to_string());
    }
    recommendations
}

fn review_status(
    proposals: &[PolicyProposal],
    gates: &[PolicyGate],
    conflicts: &[PolicyConflict],
) -> PolicyLedgerStatus {
    if conflicts.iter().any(|conflict| conflict.blocks_apply)
        || gates.iter().any(|gate| !gate.passed && gate.blocks_apply)
    {
        return PolicyLedgerStatus::Blocked;
    }
    if proposals
        .iter()
        .any(|proposal| proposal.status == PolicyLedgerStatus::Applied)
    {
        return PolicyLedgerStatus::Applied;
    }
    if proposals
        .iter()
        .any(|proposal| proposal.status == PolicyLedgerStatus::Proposed)
    {
        return PolicyLedgerStatus::Proposed;
    }
    PolicyLedgerStatus::Observed
}

fn review_domains(
    proposals: &[PolicyProposal],
    decisions: &[PolicyDecision],
    gates: &[PolicyGate],
    conflicts: &[PolicyConflict],
) -> Vec<PolicyDomain> {
    let mut domains = Vec::new();
    domains.extend(proposals.iter().map(|proposal| proposal.domain));
    domains.extend(decisions.iter().map(|decision| decision.domain));
    domains.extend(gates.iter().map(|gate| gate.domain));
    domains.extend(
        conflicts
            .iter()
            .flat_map(|conflict| conflict.domains.clone()),
    );
    domains.sort();
    domains.dedup();
    domains
}

fn map_routing_status(status: RoutingPolicyProposalStatus) -> PolicyLedgerStatus {
    match status {
        RoutingPolicyProposalStatus::Draft => PolicyLedgerStatus::Proposed,
        RoutingPolicyProposalStatus::Approved => PolicyLedgerStatus::Approved,
        RoutingPolicyProposalStatus::Applied => PolicyLedgerStatus::Applied,
        RoutingPolicyProposalStatus::Rejected => PolicyLedgerStatus::Blocked,
        RoutingPolicyProposalStatus::RolledBack => PolicyLedgerStatus::RolledBack,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AutonomousEvaluationCounters, AutonomousEvaluationScores, AutonomousRunHistorySummary,
        AutonomousRunStatusCounts, AutonomousTraceReplayReport, MoERoutingPolicy,
        RoutingEvaluationReport, RoutingOptimizerReplayReport, RoutingPolicyProposalSnapshot,
    };

    fn autonomous_evaluation(review_required: bool) -> AutonomousEvaluationReport {
        AutonomousEvaluationReport {
            version: 1,
            evaluated_at: 1,
            counters: AutonomousEvaluationCounters {
                tasks: 1,
                completed_tasks: 0,
                blocked_tasks: 1,
                failed_tasks: 0,
                recovery_triggered_tasks: 1,
                recovered_tasks: 0,
                task_memory_entries: 0,
                route_feedback_entries: 0,
                route_failures: 0,
                route_recovery_triggered: 0,
                autonomous_runs: 1,
                malformed_run_lines: 0,
                scheduler_ticks: 1,
                productive_scheduler_ticks: 0,
                worker_dispatches: 0,
                worker_completions: 0,
                policy_lifecycle_events: 0,
                policy_replay_anomalies: 0,
            },
            scores: AutonomousEvaluationScores {
                task_completion_score: 0.0,
                recovery_quality_score: 0.0,
                scheduler_efficiency_score: 0.0,
                memory_reuse_score: 0.0,
                routing_adaptation_score: 0.0,
                autonomous_success_score: 0.0,
                total_score: 0.0,
            },
            run_summary: AutonomousRunHistorySummary {
                runs_path: PathBuf::from("runs.jsonl"),
                considered_runs: 1,
                malformed_lines: 0,
                latest_run_id: Some("run-1".to_string()),
                status_counts: AutonomousRunStatusCounts {
                    idle: 0,
                    running: 0,
                    blocked: 1,
                },
                idle_rate: 0.0,
                running_rate: 0.0,
                blocked_rate: 1.0,
                average_ticks: 1.0,
                average_ticks_to_idle: None,
                average_ticks_to_blocked: Some(1.0),
                consecutive_blocked_runs: if review_required { 2 } else { 0 },
                repeated_blocked_actions: Vec::new(),
                repeated_blocked_risks: Vec::new(),
                repeated_blocked_reasons: Vec::new(),
                repeated_task_types: Vec::new(),
            },
            route_summaries: Vec::new(),
            task_memory_summaries: Vec::new(),
            trace_replay: AutonomousTraceReplayReport {
                considered_runs: 1,
                changed_decisions: 0,
                policy_recommendation: crate::AutonomousPolicyRecommendation {
                    requested_max_ticks: 3,
                    recommended_max_ticks: if review_required { 1 } else { 3 },
                    permission_mode: "default".to_string(),
                    conservative_permission_mode: "read_only".to_string(),
                    action: if review_required {
                        AutonomousPolicyAction::RequestReview
                    } else {
                        AutonomousPolicyAction::Continue
                    },
                    review_required,
                    cool_down: review_required,
                    reasons: vec!["test policy".to_string()],
                },
                decisions: Vec::new(),
                recommendations: Vec::new(),
            },
            recommendations: Vec::new(),
        }
    }

    fn routing_proposal(id: &str) -> RoutingPolicyProposal {
        RoutingPolicyProposal {
            version: 1,
            id: id.to_string(),
            created_at: 1,
            updated_at: 1,
            status: RoutingPolicyProposalStatus::Draft,
            min_samples: 2,
            switch_failure_threshold: 0.5,
            feedback_count: 3,
            estimated_success_delta: 0.5,
            recovery_reduction_estimate: 0.5,
            gates: Vec::new(),
            changes: vec![crate::RoutingPolicyProposalChange {
                phase: crate::ModelRoutePhase::Coding,
                current_model: "weak".to_string(),
                current_provider: None,
                proposed_model: "strong".to_string(),
                proposed_provider: None,
                fallback_model: Some("strong".to_string()),
                estimated_success_delta: 0.5,
                recovery_reduction_estimate: 0.5,
                reason: "test".to_string(),
            }],
            baseline_policy: MoERoutingPolicy::balanced("weak"),
            proposed_policy: MoERoutingPolicy::balanced("strong"),
            previous_policy: None,
            applied_at: None,
            rolled_back_at: None,
            evaluation: RoutingEvaluationReport {
                version: 1,
                feedback_count: 3,
                min_samples: 2,
                switch_failure_threshold: 0.5,
                health: Vec::new(),
                candidates: Vec::new(),
                summaries: Vec::new(),
                recommendations: Vec::new(),
            },
            replay: RoutingOptimizerReplayReport {
                version: 1,
                feedback_count: 3,
                evaluated_phases: Vec::new(),
                current_decisions: Vec::new(),
                changed_routes: Vec::new(),
                estimated_success_delta: 0.5,
                recovery_reduction_estimate: 0.5,
                cost_latency_tradeoff: "test".to_string(),
                recommendations: Vec::new(),
            },
            audit: Vec::new(),
        }
    }

    fn scheduler_state(status: SchedulerDaemonStatus, tick_count: u64) -> SchedulerDaemonState {
        SchedulerDaemonState {
            version: 1,
            status,
            pid: 0,
            started_at: 1,
            updated_at: tick_count.max(1),
            tick_count,
            lock_path: PathBuf::from("scheduler.lock"),
            last_tick: None,
            message: format!("test scheduler status {status:?}"),
        }
    }

    fn write_routing_snapshot(dir: &Path, proposals: Vec<RoutingPolicyProposal>) {
        fs::create_dir_all(dir).expect("routing policy dir should exist");
        fs::write(
            dir.join("policy-proposals.json"),
            serde_json::to_string(&RoutingPolicyProposalSnapshot { proposals })
                .expect("snapshot should serialize"),
        )
        .expect("snapshot should write");
    }

    #[test]
    fn review_detects_autonomous_routing_conflict() {
        let review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: Some(autonomous_evaluation(true)),
            routing_proposals: vec![routing_proposal("route-proposal-1")],
            applied_routing_policy: None,
            scheduler_state: None,
        });

        assert_eq!(review.ledger_entry.status, PolicyLedgerStatus::Blocked);
        assert!(review
            .ledger_entry
            .conflicts
            .iter()
            .any(|conflict| conflict.blocks_apply));
    }

    #[test]
    fn ledger_appends_and_loads_entries() {
        let dir = std::env::temp_dir().join(format!("himalaya-policy-ledger-{}", now_millis()));
        let ledger = PolicyGovernanceLedger::new(&dir);
        let review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: Some(autonomous_evaluation(false)),
            routing_proposals: Vec::new(),
            applied_routing_policy: None,
            scheduler_state: None,
        });

        let recorded = ledger.record_review(review).expect("record review");
        let load = ledger.load(10).expect("load ledger");

        assert_eq!(load.entries.len(), 1);
        assert_eq!(load.entries[0].id, recorded.ledger_entry.id);
        assert_eq!(load.malformed_lines, 0);
    }

    #[test]
    fn coordinator_blocks_apply_on_governance_conflict() {
        let dir = std::env::temp_dir().join(format!("himalaya-policy-block-{}", now_millis()));
        let review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: Some(autonomous_evaluation(true)),
            routing_proposals: vec![routing_proposal("route-proposal-1")],
            applied_routing_policy: None,
            scheduler_state: None,
        });
        let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir));
        let plan = coordinator.plan_apply(
            review,
            Some(PolicyDomain::Routing),
            Some("route-proposal-1"),
            false,
        );

        let report = coordinator.apply(&plan).expect("blocked apply report");

        assert_eq!(plan.status, PolicyLedgerStatus::ApplyBlocked);
        assert_eq!(report.status, PolicyLedgerStatus::ApplyBlocked);
        assert!(!report.applied);
        assert!(report.routing_report.is_none());
        assert!(report
            .blockers
            .iter()
            .any(|blocker| blocker.contains("blocking policy conflict")));
    }

    #[test]
    fn coordinator_dry_run_delegates_to_routing_store() {
        let dir = std::env::temp_dir().join(format!("himalaya-policy-dry-run-{}", now_millis()));
        let proposal = routing_proposal("route-proposal-1");
        write_routing_snapshot(&dir, vec![proposal.clone()]);
        let review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: None,
            routing_proposals: vec![proposal],
            applied_routing_policy: None,
            scheduler_state: None,
        });
        let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir));
        let plan = coordinator.plan_apply(
            review,
            Some(PolicyDomain::Routing),
            Some("route-proposal-1"),
            true,
        );

        let report = coordinator.apply(&plan).expect("dry run report");

        assert_eq!(plan.status, PolicyLedgerStatus::Planned);
        assert_eq!(report.status, PolicyLedgerStatus::DryRunPassed);
        assert!(!report.applied);
        let routing_report = report
            .routing_report
            .expect("routing dry run report should exist");
        assert!(routing_report.dry_run);
        assert!(routing_report.blockers.is_empty());
        assert!(crate::load_applied_routing_policy(&dir)
            .expect("applied policy lookup should succeed")
            .is_none());
    }

    #[test]
    fn apply_plan_exposes_cross_domain_adapter_descriptors() {
        let dir = std::env::temp_dir().join(format!("himalaya-policy-adapters-{}", now_millis()));
        let review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: None,
            routing_proposals: Vec::new(),
            applied_routing_policy: None,
            scheduler_state: Some(scheduler_state(SchedulerDaemonStatus::Running, 3)),
        });
        let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir));
        let plan = coordinator.plan_apply(review, None, None, true);

        assert!(plan.adapters.iter().any(|adapter| {
            adapter.domain == PolicyDomain::Routing
                && adapter.supports_persistent_apply
                && adapter.supports_rollback
        }));
        assert!(plan.adapters.iter().any(|adapter| {
            adapter.domain == PolicyDomain::Scheduler
                && adapter.supports_dry_run
                && !adapter.supports_persistent_apply
        }));
        assert!(plan
            .adapters
            .iter()
            .any(|adapter| adapter.domain == PolicyDomain::Memory && adapter.planned_only));
    }

    #[test]
    fn review_creates_scheduler_policy_proposal() {
        let review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: None,
            routing_proposals: Vec::new(),
            applied_routing_policy: None,
            scheduler_state: Some(scheduler_state(SchedulerDaemonStatus::Running, 3)),
        });

        let proposal = review
            .ledger_entry
            .proposals
            .iter()
            .find(|proposal| proposal.domain == PolicyDomain::Scheduler)
            .expect("scheduler proposal should exist");
        assert_eq!(proposal.id, "scheduler-policy-3");
        assert_eq!(proposal.status, PolicyLedgerStatus::Proposed);
        assert_eq!(proposal.action, "dry_run_scheduler_policy_adjustment");
        assert!(!proposal.source_fingerprint.is_empty());
    }

    #[test]
    fn coordinator_dry_runs_scheduler_policy_adapter() {
        let dir = std::env::temp_dir().join(format!("himalaya-policy-scheduler-{}", now_millis()));
        let review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: None,
            routing_proposals: Vec::new(),
            applied_routing_policy: None,
            scheduler_state: Some(scheduler_state(SchedulerDaemonStatus::Running, 3)),
        });
        let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir));
        let plan = coordinator.plan_apply(
            review,
            Some(PolicyDomain::Scheduler),
            Some("scheduler-policy-3"),
            true,
        );

        let report = coordinator.apply(&plan).expect("scheduler dry run report");

        assert_eq!(plan.status, PolicyLedgerStatus::Planned);
        assert_eq!(report.status, PolicyLedgerStatus::DryRunPassed);
        assert!(!report.applied);
        assert!(report.routing_report.is_none());
        assert_eq!(report.receipt.adapter.as_deref(), Some("scheduler_policy"));
        match report.adapter_report {
            Some(PolicyAdapterReport::SchedulerDryRun(report)) => {
                assert!(report.dry_run);
                assert_eq!(report.status, PolicyLedgerStatus::DryRunPassed);
                assert_eq!(
                    report.scheduler_status,
                    Some(SchedulerDaemonStatus::Running)
                );
                assert!(report.blockers.is_empty());
            }
            other => panic!("expected scheduler dry run adapter report, got {other:?}"),
        }
    }

    #[test]
    fn coordinator_blocks_scheduler_persistent_apply() {
        let dir =
            std::env::temp_dir().join(format!("himalaya-policy-scheduler-block-{}", now_millis()));
        let review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: None,
            routing_proposals: Vec::new(),
            applied_routing_policy: None,
            scheduler_state: Some(scheduler_state(SchedulerDaemonStatus::Running, 3)),
        });
        let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir));
        let plan = coordinator.plan_apply(
            review,
            Some(PolicyDomain::Scheduler),
            Some("scheduler-policy-3"),
            false,
        );

        let report = coordinator.apply(&plan).expect("scheduler apply report");

        assert_eq!(report.status, PolicyLedgerStatus::ApplyBlocked);
        assert!(!report.applied);
        assert_eq!(report.receipt.adapter.as_deref(), Some("scheduler_policy"));
        assert!(report
            .structured_blockers
            .iter()
            .any(|blocker| blocker.kind == PolicyBlockerKind::UnsupportedDomain));
        match report.adapter_report {
            Some(PolicyAdapterReport::SchedulerDryRun(report)) => {
                assert!(!report.dry_run);
                assert_eq!(report.status, PolicyLedgerStatus::ApplyBlocked);
            }
            other => panic!("expected scheduler dry run adapter report, got {other:?}"),
        }
    }

    #[test]
    fn coordinator_blocks_stale_policy_plan() {
        let dir = std::env::temp_dir().join(format!("himalaya-policy-stale-{}", now_millis()));
        let proposal = routing_proposal("route-proposal-1");
        write_routing_snapshot(&dir, vec![proposal.clone()]);
        let review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: None,
            routing_proposals: vec![proposal.clone()],
            applied_routing_policy: None,
            scheduler_state: None,
        });
        let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir));
        let plan = coordinator.plan_apply(
            review,
            Some(PolicyDomain::Routing),
            Some("route-proposal-1"),
            false,
        );
        let mut changed = proposal;
        changed.updated_at = changed.updated_at.saturating_add(10);
        changed.audit.push("changed after plan".to_string());
        write_routing_snapshot(&dir, vec![changed]);

        let report = coordinator.apply(&plan).expect("stale apply report");

        assert_eq!(report.status, PolicyLedgerStatus::ApplyBlocked);
        assert!(!report.applied);
        assert!(report
            .structured_blockers
            .iter()
            .any(|blocker| blocker.kind == PolicyBlockerKind::StalePlan));
        assert_eq!(report.receipt.plan_fingerprint, plan.fingerprint);
        assert!(!report.receipt.executed);
    }

    #[test]
    fn coordinator_requires_explicit_target_for_persistent_apply() {
        let dir = std::env::temp_dir().join(format!("himalaya-policy-explicit-{}", now_millis()));
        let proposal = routing_proposal("route-proposal-1");
        write_routing_snapshot(&dir, vec![proposal.clone()]);
        let review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: None,
            routing_proposals: vec![proposal],
            applied_routing_policy: None,
            scheduler_state: None,
        });
        let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir));
        let plan = coordinator.plan_apply(review, None, None, false);

        let report = coordinator.apply(&plan).expect("apply should be blocked");

        assert_eq!(report.status, PolicyLedgerStatus::ApplyBlocked);
        assert!(!report.applied);
        assert!(report
            .blockers
            .iter()
            .any(|blocker| blocker.contains("requires explicit --domain and --proposal-id")));
        assert!(crate::load_applied_routing_policy(&dir)
            .expect("applied policy lookup should succeed")
            .is_none());
    }

    #[test]
    fn coordinator_applies_and_rolls_back_routing_policy() {
        let dir = std::env::temp_dir().join(format!("himalaya-policy-apply-{}", now_millis()));
        let proposal = routing_proposal("route-proposal-1");
        write_routing_snapshot(&dir, vec![proposal.clone()]);
        let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir));
        let review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: None,
            routing_proposals: vec![proposal],
            applied_routing_policy: None,
            scheduler_state: None,
        });
        let plan = coordinator.plan_apply(
            review,
            Some(PolicyDomain::Routing),
            Some("route-proposal-1"),
            false,
        );

        let apply = coordinator.apply(&plan).expect("apply report");

        assert_eq!(apply.status, PolicyLedgerStatus::Applied);
        assert!(apply.applied);
        assert!(apply.receipt.executed);
        assert_eq!(apply.receipt.adapter.as_deref(), Some("routing_policy"));
        assert_eq!(apply.receipt.plan_fingerprint, plan.fingerprint);
        assert!(crate::load_applied_routing_policy(&dir)
            .expect("applied policy should load")
            .is_some());

        let applied_policy =
            crate::load_applied_routing_policy(&dir).expect("applied policy should load");
        let applied_proposals = RoutingPolicyProposalStore::new(&dir)
            .list()
            .expect("proposals should load");
        let rollback_review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: None,
            routing_proposals: applied_proposals,
            applied_routing_policy: applied_policy,
            scheduler_state: None,
        });
        let rollback_plan = coordinator.plan_rollback(
            rollback_review,
            Some(PolicyDomain::Routing),
            Some("route-proposal-1"),
        );

        let rollback = coordinator
            .rollback(&rollback_plan)
            .expect("rollback report");

        assert_eq!(rollback_plan.status, PolicyLedgerStatus::RollbackPlanned);
        assert_eq!(rollback.status, PolicyLedgerStatus::RolledBack);
        assert!(rollback.rolled_back);
        assert!(rollback.receipt.executed);
        assert!(crate::load_applied_routing_policy(&dir)
            .expect("applied policy lookup should succeed")
            .is_none());
    }

    #[test]
    fn ledger_replay_reconstructs_policy_lifecycle() {
        let dir = std::env::temp_dir().join(format!("himalaya-policy-replay-{}", now_millis()));
        let ledger = PolicyGovernanceLedger::new(&dir);
        let proposal = routing_proposal("route-proposal-1");
        let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir));
        write_routing_snapshot(&dir, vec![proposal.clone()]);
        let review = review_policy_governance(PolicyGovernanceInput {
            autonomous_evaluation: None,
            routing_proposals: vec![proposal],
            applied_routing_policy: None,
            scheduler_state: None,
        });
        let plan = coordinator.plan_apply(
            review,
            Some(PolicyDomain::Routing),
            Some("route-proposal-1"),
            false,
        );
        ledger.record_apply_plan(&plan).expect("record plan");
        let apply = coordinator.apply(&plan).expect("apply report");
        ledger.record_apply_report(&apply).expect("record apply");

        let replay = replay_policy_lifecycle(&dir, 20).expect("replay ledger");

        assert_eq!(replay.summary.lifecycle_count, 1);
        assert_eq!(replay.summary.event_count, 2);
        assert_eq!(replay.lifecycles[0].proposal_id, "route-proposal-1");
        assert_eq!(
            replay.lifecycles[0].current_status,
            PolicyLedgerStatus::Applied
        );
        assert!(replay.lifecycles[0]
            .events
            .iter()
            .any(|event| event.status == PolicyLedgerStatus::Planned));
        assert!(replay.lifecycles[0]
            .events
            .iter()
            .any(|event| event.status == PolicyLedgerStatus::Applied));
    }
}
