use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::adapter::{PolicyAdapterContext, PolicyAdapterRegistry};
use super::review::routing_policy_proposal;
use super::*;
use crate::{
    AutonomousEvaluationCounters, AutonomousEvaluationReport, AutonomousEvaluationScores,
    AutonomousPolicyAction, AutonomousRunHistorySummary, AutonomousRunStatusCounts,
    AutonomousTraceReplayReport, MoERoutingPolicy, RoutingEvaluationReport,
    RoutingOptimizerReplayReport, RoutingPolicyProposal, RoutingPolicyProposalSnapshot,
    RoutingPolicyProposalStatus, RoutingPolicyProposalStore, SchedulerDaemonState,
    SchedulerDaemonStatus,
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

fn policy_entry(
    id: &str,
    timestamp: u64,
    status: PolicyLedgerStatus,
    proposal: PolicyProposal,
    decision_action: &str,
) -> PolicyLedgerEntry {
    PolicyLedgerEntry {
        version: POLICY_GOVERNANCE_VERSION,
        id: id.to_string(),
        timestamp,
        status,
        domains: vec![proposal.domain],
        proposals: vec![proposal.clone()],
        decisions: vec![PolicyDecision {
            domain: proposal.domain,
            action: decision_action.to_string(),
            status,
            risk: proposal.risk,
            reason: "test entry".to_string(),
        }],
        gates: Vec::new(),
        conflicts: Vec::new(),
        recommendations: Vec::new(),
        summary: PolicyReviewSummary {
            status,
            proposal_count: 1,
            decision_count: 1,
            gate_count: 0,
            failed_gate_count: 0,
            conflict_count: 0,
            blocking_conflict_count: 0,
            active_routing_policy: false,
            routing_proposal_count: usize::from(proposal.domain == PolicyDomain::Routing),
            scheduler_status: None,
            autonomous_action: None,
            autonomous_review_required: false,
            memory_reuse_score: None,
        },
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
    assert!(plan.adapters.iter().any(|adapter| {
        adapter.domain == PolicyDomain::Memory
            && adapter.supports_dry_run
            && !adapter.supports_persistent_apply
            && !adapter.planned_only
    }));
    assert!(plan.adapters.iter().any(|adapter| {
        adapter.domain == PolicyDomain::Recovery
            && adapter.supports_dry_run
            && !adapter.supports_persistent_apply
            && !adapter.planned_only
    }));
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
    let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir))
        .with_scheduler_state(Some(scheduler_state(SchedulerDaemonStatus::Running, 3)));
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
    let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir))
        .with_scheduler_state(Some(scheduler_state(SchedulerDaemonStatus::Running, 3)));
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
fn coordinator_blocks_stale_scheduler_policy_plan() {
    let dir =
        std::env::temp_dir().join(format!("himalaya-policy-scheduler-stale-{}", now_millis()));
    let review = review_policy_governance(PolicyGovernanceInput {
        autonomous_evaluation: None,
        routing_proposals: Vec::new(),
        applied_routing_policy: None,
        scheduler_state: Some(scheduler_state(SchedulerDaemonStatus::Running, 3)),
    });
    let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir))
        .with_scheduler_state(Some(scheduler_state(SchedulerDaemonStatus::Running, 4)));
    let plan = coordinator.plan_apply(
        review,
        Some(PolicyDomain::Scheduler),
        Some("scheduler-policy-3"),
        true,
    );

    let report = coordinator.apply(&plan).expect("scheduler stale report");

    assert_eq!(report.status, PolicyLedgerStatus::ApplyBlocked);
    assert!(!report.applied);
    assert!(report
        .structured_blockers
        .iter()
        .any(|blocker| blocker.kind == PolicyBlockerKind::StalePlan));
}

#[test]
fn review_creates_memory_and_recovery_policy_proposals() {
    let evaluation = autonomous_evaluation(false);
    let review = review_policy_governance(PolicyGovernanceInput {
        autonomous_evaluation: Some(evaluation),
        routing_proposals: Vec::new(),
        applied_routing_policy: None,
        scheduler_state: None,
    });

    let memory = review
        .ledger_entry
        .proposals
        .iter()
        .find(|proposal| proposal.domain == PolicyDomain::Memory)
        .expect("memory proposal should exist");
    let recovery = review
        .ledger_entry
        .proposals
        .iter()
        .find(|proposal| proposal.domain == PolicyDomain::Recovery)
        .expect("recovery proposal should exist");

    assert_eq!(memory.id, "memory-policy-coverage");
    assert_eq!(memory.status, PolicyLedgerStatus::Proposed);
    assert_eq!(recovery.id, "recovery-policy-quality");
    assert_eq!(recovery.status, PolicyLedgerStatus::Proposed);
    assert!(!memory.source_fingerprint.is_empty());
    assert!(!recovery.source_fingerprint.is_empty());
}

#[test]
fn coordinator_dry_runs_memory_policy_adapter() {
    let dir = std::env::temp_dir().join(format!("himalaya-policy-memory-{}", now_millis()));
    let evaluation = autonomous_evaluation(false);
    let review = review_policy_governance(PolicyGovernanceInput {
        autonomous_evaluation: Some(evaluation.clone()),
        routing_proposals: Vec::new(),
        applied_routing_policy: None,
        scheduler_state: None,
    });
    let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir))
        .with_autonomous_evaluation(Some(evaluation));
    let plan = coordinator.plan_apply(
        review,
        Some(PolicyDomain::Memory),
        Some("memory-policy-coverage"),
        true,
    );

    let report = coordinator.apply(&plan).expect("memory dry run report");

    assert_eq!(plan.status, PolicyLedgerStatus::Planned);
    assert_eq!(report.status, PolicyLedgerStatus::DryRunPassed);
    assert!(!report.applied);
    assert_eq!(report.receipt.adapter.as_deref(), Some("memory_policy"));
    match report.adapter_report {
        Some(PolicyAdapterReport::MemoryDryRun(report)) => {
            assert!(report.dry_run);
            assert_eq!(report.task_count, 1);
            assert_eq!(report.task_memory_entries, 0);
            assert_eq!(report.memory_reuse_score, 0.0);
        }
        other => panic!("expected memory dry run adapter report, got {other:?}"),
    }
}

#[test]
fn coordinator_blocks_memory_persistent_apply() {
    let dir = std::env::temp_dir().join(format!("himalaya-policy-memory-block-{}", now_millis()));
    let evaluation = autonomous_evaluation(false);
    let review = review_policy_governance(PolicyGovernanceInput {
        autonomous_evaluation: Some(evaluation.clone()),
        routing_proposals: Vec::new(),
        applied_routing_policy: None,
        scheduler_state: None,
    });
    let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir))
        .with_autonomous_evaluation(Some(evaluation));
    let plan = coordinator.plan_apply(
        review,
        Some(PolicyDomain::Memory),
        Some("memory-policy-coverage"),
        false,
    );

    let report = coordinator.apply(&plan).expect("memory apply report");

    assert_eq!(report.status, PolicyLedgerStatus::ApplyBlocked);
    assert!(!report.applied);
    assert_eq!(report.receipt.adapter.as_deref(), Some("memory_policy"));
    assert!(report
        .structured_blockers
        .iter()
        .any(|blocker| blocker.kind == PolicyBlockerKind::UnsupportedDomain));
}

#[test]
fn coordinator_dry_runs_recovery_policy_adapter() {
    let dir = std::env::temp_dir().join(format!("himalaya-policy-recovery-{}", now_millis()));
    let evaluation = autonomous_evaluation(false);
    let review = review_policy_governance(PolicyGovernanceInput {
        autonomous_evaluation: Some(evaluation.clone()),
        routing_proposals: Vec::new(),
        applied_routing_policy: None,
        scheduler_state: None,
    });
    let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir))
        .with_autonomous_evaluation(Some(evaluation));
    let plan = coordinator.plan_apply(
        review,
        Some(PolicyDomain::Recovery),
        Some("recovery-policy-quality"),
        true,
    );

    let report = coordinator.apply(&plan).expect("recovery dry run report");

    assert_eq!(plan.status, PolicyLedgerStatus::Planned);
    assert_eq!(report.status, PolicyLedgerStatus::DryRunPassed);
    assert!(!report.applied);
    assert_eq!(report.receipt.adapter.as_deref(), Some("recovery_policy"));
    match report.adapter_report {
        Some(PolicyAdapterReport::RecoveryDryRun(report)) => {
            assert!(report.dry_run);
            assert_eq!(report.recovery_triggered_tasks, 1);
            assert_eq!(report.recovered_tasks, 0);
            assert_eq!(report.recovery_quality_score, 0.0);
        }
        other => panic!("expected recovery dry run adapter report, got {other:?}"),
    }
}

#[test]
fn coordinator_blocks_stale_memory_policy_plan() {
    let dir = std::env::temp_dir().join(format!("himalaya-policy-memory-stale-{}", now_millis()));
    let evaluation = autonomous_evaluation(false);
    let review = review_policy_governance(PolicyGovernanceInput {
        autonomous_evaluation: Some(evaluation.clone()),
        routing_proposals: Vec::new(),
        applied_routing_policy: None,
        scheduler_state: None,
    });
    let mut changed = evaluation;
    changed.counters.task_memory_entries = 1;
    changed.scores.memory_reuse_score = 1.0;
    let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir))
        .with_autonomous_evaluation(Some(changed));
    let plan = coordinator.plan_apply(
        review,
        Some(PolicyDomain::Memory),
        Some("memory-policy-coverage"),
        true,
    );

    let report = coordinator.apply(&plan).expect("memory stale report");

    assert_eq!(report.status, PolicyLedgerStatus::ApplyBlocked);
    assert!(report
        .structured_blockers
        .iter()
        .any(|blocker| blocker.kind == PolicyBlockerKind::StalePlan));
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

#[test]
fn json_contract_accepts_legacy_policy_governance_payloads() {
    let dir = std::env::temp_dir().join(format!("himalaya-policy-json-{}", now_millis()));
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

    let mut legacy_plan = serde_json::to_value(&plan).expect("plan serializes");
    let plan_object = legacy_plan.as_object_mut().expect("plan should be object");
    plan_object.remove("adapters");
    plan_object.insert("unknown_legacy_field".to_string(), json!(true));
    let decoded_plan: PolicyApplyPlan =
        serde_json::from_value(legacy_plan).expect("legacy apply plan should deserialize");
    assert!(decoded_plan.adapters.is_empty());

    let report = coordinator.apply(&plan).expect("apply report");
    let mut legacy_report = serde_json::to_value(&report).expect("report serializes");
    let report_object = legacy_report
        .as_object_mut()
        .expect("apply report should be object");
    report_object.remove("adapter_report");
    report_object.insert("unknown_legacy_field".to_string(), json!("ignored"));
    let decoded_report: PolicyApplyReport =
        serde_json::from_value(legacy_report).expect("legacy apply report should deserialize");
    assert!(decoded_report.adapter_report.is_none());
    assert!(decoded_report.routing_report.is_some());

    let mut legacy_entry =
        serde_json::to_value(policy_apply_plan_ledger_entry(&plan)).expect("entry serializes");
    legacy_entry
        .as_object_mut()
        .expect("entry should be object")
        .insert("unknown_legacy_field".to_string(), json!({"ignored": true}));
    legacy_entry["proposals"][0]
        .as_object_mut()
        .expect("proposal should be object")
        .remove("source_fingerprint");
    let decoded_entry: PolicyLedgerEntry =
        serde_json::from_value(legacy_entry).expect("legacy ledger entry should deserialize");
    assert!(decoded_entry.proposals[0].source_fingerprint.is_empty());

    let legacy_summary = json!({
        "lifecycle_count": 1,
        "event_count": 1,
        "anomaly_count": 0,
        "malformed_lines": 0,
        "unknown_legacy_field": "ignored"
    });
    let decoded_summary: PolicyLifecycleReplaySummary =
        serde_json::from_value(legacy_summary).expect("legacy replay summary should deserialize");
    assert!(decoded_summary.domain_counts.is_empty());
    assert!(decoded_summary.action_counts.is_empty());
    assert!(decoded_summary.anomaly_kind_counts.is_empty());
}

#[test]
fn adapter_registry_contract_exposes_registered_domains_only() {
    let dir = std::env::temp_dir().join(format!("himalaya-policy-registry-{}", now_millis()));
    let store = RoutingPolicyProposalStore::new(&dir);
    let scheduler = scheduler_state(SchedulerDaemonStatus::Running, 3);
    let evaluation = autonomous_evaluation(false);
    let registry = PolicyAdapterRegistry::new(PolicyAdapterContext {
        routing_store: &store,
        scheduler_state: Some(&scheduler),
        autonomous_evaluation: Some(&evaluation),
    });

    for (domain, expected_name, persistent, rollback) in [
        (PolicyDomain::Routing, "routing_policy", true, true),
        (PolicyDomain::Scheduler, "scheduler_policy", false, false),
        (PolicyDomain::Memory, "memory_policy", false, false),
        (PolicyDomain::Recovery, "recovery_policy", false, false),
    ] {
        let adapter = registry
            .adapter_for_domain(domain)
            .expect("domain should have an executable adapter");
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.domain, domain);
        assert_eq!(descriptor.name, expected_name);
        assert!(descriptor.supports_apply);
        assert!(descriptor.supports_dry_run);
        assert_eq!(descriptor.supports_persistent_apply, persistent);
        assert_eq!(descriptor.supports_rollback, rollback);
        assert!(!descriptor.planned_only);
    }
    assert!(registry
        .adapter_for_domain(PolicyDomain::AutonomousRun)
        .is_none());

    let descriptors = policy_adapter_descriptors(Some(SchedulerDaemonStatus::Running));
    let autonomous = descriptors
        .iter()
        .find(|descriptor| descriptor.domain == PolicyDomain::AutonomousRun)
        .expect("autonomous run descriptor should exist");
    assert_eq!(autonomous.name, "autonomous_run_policy");
    assert!(autonomous.planned_only);
    assert!(!autonomous.supports_apply);
    assert!(!autonomous.supports_persistent_apply);
    assert!(!autonomous.supports_dry_run);
    assert!(!autonomous.supports_rollback);
}

#[test]
fn coordinator_blocks_recovery_persistent_apply() {
    let dir = std::env::temp_dir().join(format!("himalaya-policy-recovery-block-{}", now_millis()));
    let evaluation = autonomous_evaluation(false);
    let review = review_policy_governance(PolicyGovernanceInput {
        autonomous_evaluation: Some(evaluation.clone()),
        routing_proposals: Vec::new(),
        applied_routing_policy: None,
        scheduler_state: None,
    });
    let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir))
        .with_autonomous_evaluation(Some(evaluation));
    let plan = coordinator.plan_apply(
        review,
        Some(PolicyDomain::Recovery),
        Some("recovery-policy-quality"),
        false,
    );

    let report = coordinator.apply(&plan).expect("recovery apply report");

    assert_eq!(report.status, PolicyLedgerStatus::ApplyBlocked);
    assert!(!report.applied);
    assert_eq!(report.receipt.adapter.as_deref(), Some("recovery_policy"));
    assert!(report
        .structured_blockers
        .iter()
        .any(|blocker| blocker.kind == PolicyBlockerKind::UnsupportedDomain));
}

#[test]
fn coordinator_blocks_stale_recovery_policy_plan() {
    let dir = std::env::temp_dir().join(format!("himalaya-policy-recovery-stale-{}", now_millis()));
    let evaluation = autonomous_evaluation(false);
    let review = review_policy_governance(PolicyGovernanceInput {
        autonomous_evaluation: Some(evaluation.clone()),
        routing_proposals: Vec::new(),
        applied_routing_policy: None,
        scheduler_state: None,
    });
    let mut changed = evaluation;
    changed.counters.recovered_tasks = 1;
    changed.scores.recovery_quality_score = 1.0;
    let coordinator = PolicyApplyCoordinator::new(RoutingPolicyProposalStore::new(&dir))
        .with_autonomous_evaluation(Some(changed));
    let plan = coordinator.plan_apply(
        review,
        Some(PolicyDomain::Recovery),
        Some("recovery-policy-quality"),
        true,
    );

    let report = coordinator.apply(&plan).expect("recovery stale report");

    assert_eq!(report.status, PolicyLedgerStatus::ApplyBlocked);
    assert!(report
        .structured_blockers
        .iter()
        .any(|blocker| blocker.kind == PolicyBlockerKind::StalePlan));
}

#[test]
fn replay_summary_and_filter_distinguish_apply_from_dry_run_simulations() {
    let routing = routing_policy_proposal(&routing_proposal("route-proposal-1"));
    let memory = review_policy_governance(PolicyGovernanceInput {
        autonomous_evaluation: Some(autonomous_evaluation(false)),
        routing_proposals: Vec::new(),
        applied_routing_policy: None,
        scheduler_state: None,
    })
    .ledger_entry
    .proposals
    .into_iter()
    .find(|proposal| proposal.domain == PolicyDomain::Memory)
    .expect("memory proposal should exist");
    let replay = replay_policy_lifecycle_entries(PolicyLedgerLoad {
        ledger_path: PathBuf::from("policy/ledger.jsonl"),
        entries: vec![
            policy_entry(
                "route-plan",
                1,
                PolicyLedgerStatus::Planned,
                routing.clone(),
                "governed_policy_apply",
            ),
            policy_entry(
                "route-apply",
                2,
                PolicyLedgerStatus::Applied,
                routing,
                "governed_policy_apply",
            ),
            policy_entry(
                "memory-dry-run-1",
                3,
                PolicyLedgerStatus::DryRunPassed,
                memory.clone(),
                "governed_policy_apply_dry_run",
            ),
            policy_entry(
                "memory-dry-run-2",
                4,
                PolicyLedgerStatus::DryRunPassed,
                memory,
                "governed_policy_apply_dry_run",
            ),
        ],
        malformed_lines: 0,
        warnings: Vec::new(),
    });

    assert_eq!(replay.summary.lifecycle_count, 2);
    assert_eq!(replay.summary.event_count, 4);
    assert_eq!(
        replay.summary.domain_counts.get(&PolicyDomain::Routing),
        Some(&1)
    );
    assert_eq!(
        replay.summary.domain_counts.get(&PolicyDomain::Memory),
        Some(&1)
    );
    assert_eq!(
        replay
            .summary
            .action_counts
            .get("apply_routing_policy_overlay"),
        Some(&2)
    );
    assert_eq!(
        replay
            .summary
            .action_counts
            .get("dry_run_memory_policy_coverage"),
        Some(&2)
    );
    assert_eq!(
        replay.summary.anomaly_kind_counts.get("duplicate_status"),
        Some(&1)
    );
    assert!(!replay
        .anomalies
        .iter()
        .any(|anomaly| anomaly.kind == "apply_without_plan"));

    let routing_only = filter_policy_lifecycle_replay(
        &replay,
        Some(PolicyDomain::Routing),
        Some("routing_policy_overlay"),
    );
    assert_eq!(routing_only.summary.lifecycle_count, 1);
    assert_eq!(routing_only.summary.event_count, 2);
    assert_eq!(routing_only.summary.anomaly_count, 0);
    assert_eq!(
        routing_only
            .summary
            .domain_counts
            .get(&PolicyDomain::Routing),
        Some(&1)
    );
    assert!(!routing_only
        .summary
        .domain_counts
        .contains_key(&PolicyDomain::Memory));

    let memory_only =
        filter_policy_lifecycle_replay(&replay, Some(PolicyDomain::Memory), Some("dry_run_memory"));
    assert_eq!(memory_only.summary.lifecycle_count, 1);
    assert_eq!(memory_only.summary.event_count, 2);
    assert_eq!(memory_only.summary.anomaly_count, 1);
}
