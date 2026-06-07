use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{
    AppliedRoutingPolicy, AutonomousEvaluationReport, AutonomousPolicyAction,
    RoutingPolicyProposal, RoutingPolicyProposalStatus, RoutingPolicySafetyLevel,
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
    Proposed,
    Approved,
    Applied,
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
        fs::create_dir_all(&self.dir)?;
        review.ledger_path = self.ledger_path();
        let line = serde_json::to_string(&review.ledger_entry)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.ledger_path())?;
        writeln!(file, "{line}")?;
        Ok(review)
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
    proposals
        .iter()
        .map(|proposal| {
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
                references: vec![PolicyReference {
                    label: "route_policy_proposal".to_string(),
                    path: None,
                    id: Some(proposal.id.clone()),
                }],
                gates,
            }
        })
        .collect()
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
        RoutingEvaluationReport, RoutingOptimizerReplayReport,
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
}
