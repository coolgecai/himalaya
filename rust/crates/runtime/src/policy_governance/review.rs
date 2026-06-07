use std::path::PathBuf;

use serde_json::json;

use crate::{
    AutonomousEvaluationReport, AutonomousPolicyAction, RoutingPolicyProposal,
    RoutingPolicyProposalStatus, RoutingPolicySafetyLevel, SchedulerDaemonState,
    SchedulerDaemonStatus,
};

use super::{
    now_millis, now_secs, stable_hash_json, PolicyConflict, PolicyDecision, PolicyDomain,
    PolicyGate, PolicyGovernanceInput, PolicyGovernanceReview, PolicyLedgerEntry,
    PolicyLedgerStatus, PolicyProposal, PolicyReference, PolicyReviewSummary, PolicyRiskLevel,
    POLICY_GOVERNANCE_VERSION,
};

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
        let memory_proposal = memory_policy_proposal(evaluation);
        gates.extend(memory_proposal.gates.clone());
        proposals.push(memory_proposal);
        let recovery_proposal = recovery_policy_proposal(evaluation);
        gates.extend(recovery_proposal.gates.clone());
        proposals.push(recovery_proposal);
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

pub(super) fn routing_policy_proposal(proposal: &RoutingPolicyProposal) -> PolicyProposal {
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

pub(super) fn scheduler_policy_proposal(state: &SchedulerDaemonState) -> PolicyProposal {
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

pub(super) fn memory_policy_proposal(evaluation: &AutonomousEvaluationReport) -> PolicyProposal {
    let needs_adjustment =
        evaluation.scores.memory_reuse_score < 0.5 && evaluation.counters.tasks > 0;
    let status = if needs_adjustment {
        PolicyLedgerStatus::Proposed
    } else {
        PolicyLedgerStatus::Observed
    };
    let risk = if evaluation.scores.memory_reuse_score < 0.25 && evaluation.counters.tasks > 0 {
        PolicyRiskLevel::High
    } else if needs_adjustment {
        PolicyRiskLevel::Medium
    } else {
        PolicyRiskLevel::Low
    };
    let gates = vec![PolicyGate {
        domain: PolicyDomain::Memory,
        name: "memory_adapter_dry_run_only".to_string(),
        passed: true,
        severity: PolicyRiskLevel::Medium,
        blocks_apply: false,
        reason: "memory policy adapter can simulate coverage changes without mutating task memory"
            .to_string(),
    }];
    PolicyProposal {
        id: "memory-policy-coverage".to_string(),
        domain: PolicyDomain::Memory,
        action: "dry_run_memory_policy_coverage".to_string(),
        status,
        risk,
        summary: format!(
            "memory reuse score {:.0}% across {} task(s) and {} memory entrie(s)",
            evaluation.scores.memory_reuse_score * 100.0,
            evaluation.counters.tasks,
            evaluation.counters.task_memory_entries
        ),
        source_fingerprint: memory_policy_fingerprint(evaluation),
        references: vec![PolicyReference {
            label: "autonomous_evaluation_memory".to_string(),
            path: None,
            id: Some(format!("evaluated-at-{}", evaluation.evaluated_at)),
        }],
        gates,
    }
}

pub(super) fn recovery_policy_proposal(evaluation: &AutonomousEvaluationReport) -> PolicyProposal {
    let triggered = evaluation.counters.recovery_triggered_tasks;
    let recovered = evaluation.counters.recovered_tasks;
    let needs_adjustment = triggered > 0 && recovered < triggered;
    let status = if needs_adjustment {
        PolicyLedgerStatus::Proposed
    } else {
        PolicyLedgerStatus::Observed
    };
    let risk = if triggered > 0 && recovered == 0 {
        PolicyRiskLevel::High
    } else if needs_adjustment {
        PolicyRiskLevel::Medium
    } else {
        PolicyRiskLevel::Low
    };
    let gates = vec![PolicyGate {
        domain: PolicyDomain::Recovery,
        name: "recovery_adapter_dry_run_only".to_string(),
        passed: true,
        severity: PolicyRiskLevel::Medium,
        blocks_apply: false,
        reason:
            "recovery policy adapter can simulate recovery tuning without mutating recovery recipes"
                .to_string(),
    }];
    PolicyProposal {
        id: "recovery-policy-quality".to_string(),
        domain: PolicyDomain::Recovery,
        action: "dry_run_recovery_policy_quality".to_string(),
        status,
        risk,
        summary: format!(
            "{} recovery-triggered task(s), {} recovered, quality score {:.0}%",
            triggered,
            recovered,
            evaluation.scores.recovery_quality_score * 100.0
        ),
        source_fingerprint: recovery_policy_fingerprint(evaluation),
        references: vec![PolicyReference {
            label: "autonomous_evaluation_recovery".to_string(),
            path: None,
            id: Some(format!("evaluated-at-{}", evaluation.evaluated_at)),
        }],
        gates,
    }
}

fn memory_policy_fingerprint(evaluation: &AutonomousEvaluationReport) -> String {
    stable_hash_json(&json!({
        "tasks": evaluation.counters.tasks,
        "task_memory_entries": evaluation.counters.task_memory_entries,
        "memory_reuse_score": evaluation.scores.memory_reuse_score,
        "policy_lifecycle_events": evaluation.counters.policy_lifecycle_events,
        "policy_replay_anomalies": evaluation.counters.policy_replay_anomalies,
    }))
}

fn recovery_policy_fingerprint(evaluation: &AutonomousEvaluationReport) -> String {
    stable_hash_json(&json!({
        "recovery_triggered_tasks": evaluation.counters.recovery_triggered_tasks,
        "recovered_tasks": evaluation.counters.recovered_tasks,
        "recovery_quality_score": evaluation.scores.recovery_quality_score,
        "blocked_tasks": evaluation.counters.blocked_tasks,
        "failed_tasks": evaluation.counters.failed_tasks,
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

pub(super) fn memory_policy_simulation_recommendations(
    evaluation: &AutonomousEvaluationReport,
) -> Vec<String> {
    let mut recommendations = Vec::new();
    if evaluation.counters.tasks == 0 {
        recommendations.push(
            "No tasks are available; memory policy simulation has no coverage target.".to_string(),
        );
    }
    if evaluation.scores.memory_reuse_score < 0.5 && evaluation.counters.tasks > 0 {
        recommendations.push(
            "Increase task memory coverage before using memory feedback for autonomous policy changes."
                .to_string(),
        );
    }
    if evaluation.counters.policy_replay_anomalies > 0 {
        recommendations.push(
            "Resolve policy replay anomalies before trusting memory-informed policy simulation."
                .to_string(),
        );
    }
    if recommendations.is_empty() {
        recommendations
            .push("Memory policy dry run passed; no task memory state was changed.".to_string());
    }
    recommendations
}

pub(super) fn recovery_policy_simulation_recommendations(
    evaluation: &AutonomousEvaluationReport,
) -> Vec<String> {
    let mut recommendations = Vec::new();
    if evaluation.counters.recovery_triggered_tasks == 0 {
        recommendations.push(
            "No recovery-triggered tasks are available; keep recovery policy in observation mode."
                .to_string(),
        );
    } else if evaluation.counters.recovered_tasks < evaluation.counters.recovery_triggered_tasks {
        recommendations.push(
            "Recovery dry run found unresolved recoveries; inspect failed recovery actions before persistent tuning."
                .to_string(),
        );
    }
    if evaluation.scores.recovery_quality_score < 0.5
        && evaluation.counters.recovery_triggered_tasks > 0
    {
        recommendations.push(
            "Recovery quality is low; prefer safe retries and model switches before human-gated actions."
                .to_string(),
        );
    }
    if recommendations.is_empty() {
        recommendations.push(
            "Recovery policy dry run passed; no recovery recipes or actions were changed."
                .to_string(),
        );
    }
    recommendations
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
    if proposals.iter().any(|proposal| {
        proposal.domain == PolicyDomain::Memory && proposal.status == PolicyLedgerStatus::Proposed
    }) {
        recommendations.push(
            "Validate memory policy coverage with a governed dry run before changing memory behavior."
                .to_string(),
        );
    }
    if proposals.iter().any(|proposal| {
        proposal.domain == PolicyDomain::Recovery && proposal.status == PolicyLedgerStatus::Proposed
    }) {
        recommendations.push(
            "Validate recovery policy quality with a governed dry run before changing recovery behavior."
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

pub(super) fn map_routing_status(status: RoutingPolicyProposalStatus) -> PolicyLedgerStatus {
    match status {
        RoutingPolicyProposalStatus::Draft => PolicyLedgerStatus::Proposed,
        RoutingPolicyProposalStatus::Approved => PolicyLedgerStatus::Approved,
        RoutingPolicyProposalStatus::Applied => PolicyLedgerStatus::Applied,
        RoutingPolicyProposalStatus::Rejected => PolicyLedgerStatus::Blocked,
        RoutingPolicyProposalStatus::RolledBack => PolicyLedgerStatus::RolledBack,
    }
}
