use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{
    task_registry::Task as RegistryTask, AutonomousEvaluationReport, AutonomousRunLoad,
    DurableSchedulerTaskSnapshot, PolicyDomain, PolicyLedgerLoad, PolicyLifecycleReplay,
    RouteFeedbackStore, RoutingPolicyProposal, SchedulerDaemonEvent, SchedulerDaemonState,
    TaskEventLogEntry, TaskMemoryStore, TaskStatus, Worker, WorkerStatus,
};

pub const AUTONOMOUS_INTEGRATION_REPORT_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct AutonomousIntegrationInput {
    pub tasks: Vec<RegistryTask>,
    pub task_ledger: Vec<crate::ProgressLedgerEntry>,
    pub task_events: Vec<TaskEventLogEntry>,
    pub scheduler_state: Option<SchedulerDaemonState>,
    pub scheduler_events: Vec<SchedulerDaemonEvent>,
    pub scheduler_queue: Vec<DurableSchedulerTaskSnapshot>,
    pub workers: Vec<Worker>,
    pub task_memory: TaskMemoryStore,
    pub route_feedback: RouteFeedbackStore,
    pub routing_proposals: Vec<RoutingPolicyProposal>,
    pub policy_ledger: Option<PolicyLedgerLoad>,
    pub policy_replay: Option<PolicyLifecycleReplay>,
    pub autonomous_runs: AutonomousRunLoad,
    pub evaluation: Option<AutonomousEvaluationReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomousIntegrationStatus {
    Healthy,
    Degraded,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomousIntegrationCheckStatus {
    Passed,
    Warning,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomousIntegrationSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousIntegrationSummary {
    pub task_count: usize,
    pub runnable_task_count: usize,
    pub blocked_task_count: usize,
    pub terminal_task_count: usize,
    pub task_event_count: usize,
    pub scheduler_status: Option<crate::SchedulerDaemonStatus>,
    pub scheduler_tick_count: u64,
    pub scheduler_event_count: usize,
    pub worker_count: usize,
    pub active_worker_count: usize,
    pub blocked_worker_count: usize,
    pub task_memory_entries: usize,
    pub route_feedback_entries: usize,
    pub routing_proposal_count: usize,
    pub policy_ledger_entries: usize,
    pub policy_lifecycle_count: usize,
    pub policy_anomaly_count: usize,
    pub autonomous_run_count: usize,
    pub evaluation_total_score: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomousIntegrationComponent {
    pub name: String,
    pub status: AutonomousIntegrationCheckStatus,
    pub observed: usize,
    pub expected: Option<usize>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomousIntegrationInvariant {
    pub name: String,
    pub status: AutonomousIntegrationCheckStatus,
    pub severity: AutonomousIntegrationSeverity,
    pub message: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomousIntegrationReplayStage {
    pub name: String,
    pub status: AutonomousIntegrationCheckStatus,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomousIntegrationReplaySummary {
    pub status: AutonomousIntegrationCheckStatus,
    pub stage_count: usize,
    pub passed_stage_count: usize,
    pub warning_stage_count: usize,
    pub failed_stage_count: usize,
    pub stages: Vec<AutonomousIntegrationReplayStage>,
    pub path_signature: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousIntegrationReport {
    pub version: u32,
    pub generated_at: u64,
    pub status: AutonomousIntegrationStatus,
    pub summary: AutonomousIntegrationSummary,
    pub components: Vec<AutonomousIntegrationComponent>,
    pub invariants: Vec<AutonomousIntegrationInvariant>,
    pub replay: AutonomousIntegrationReplaySummary,
    pub recommendations: Vec<String>,
}

#[must_use]
pub fn review_autonomous_integration(
    input: AutonomousIntegrationInput,
) -> AutonomousIntegrationReport {
    let summary = integration_summary(&input);
    let components = integration_components(&input, &summary);
    let invariants = integration_invariants(&input, &summary);
    let replay = integration_replay_summary(&input);
    let status = integration_status(&components, &invariants, &replay);
    let recommendations = integration_recommendations(status, &summary, &invariants, &replay);

    AutonomousIntegrationReport {
        version: AUTONOMOUS_INTEGRATION_REPORT_VERSION,
        generated_at: now_secs(),
        status,
        summary,
        components,
        invariants,
        replay,
        recommendations,
    }
}

fn integration_summary(input: &AutonomousIntegrationInput) -> AutonomousIntegrationSummary {
    let runnable_task_count = input
        .scheduler_queue
        .iter()
        .filter(|task| task.runnable)
        .count();
    let blocked_task_count = input
        .tasks
        .iter()
        .filter(|task| matches!(task.status, TaskStatus::Blocked | TaskStatus::Failed))
        .count();
    let terminal_task_count = input
        .tasks
        .iter()
        .filter(|task| {
            matches!(
                task.status,
                TaskStatus::Completed
                    | TaskStatus::Failed
                    | TaskStatus::Stopped
                    | TaskStatus::Cancelled
            )
        })
        .count();
    let active_worker_count = input
        .workers
        .iter()
        .filter(|worker| {
            matches!(
                worker.status,
                WorkerStatus::Spawning
                    | WorkerStatus::ReadyForPrompt
                    | WorkerStatus::PromptAccepted
                    | WorkerStatus::Running
            )
        })
        .count();
    let blocked_worker_count = input
        .workers
        .iter()
        .filter(|worker| {
            matches!(
                worker.status,
                WorkerStatus::TrustRequired | WorkerStatus::Failed
            )
        })
        .count();
    let policy_lifecycle_count = input
        .policy_replay
        .as_ref()
        .map_or(0, |replay| replay.summary.lifecycle_count);
    let policy_anomaly_count = input
        .policy_replay
        .as_ref()
        .map_or(0, |replay| replay.summary.anomaly_count);
    AutonomousIntegrationSummary {
        task_count: input.tasks.len(),
        runnable_task_count,
        blocked_task_count,
        terminal_task_count,
        task_event_count: input.task_events.len(),
        scheduler_status: input.scheduler_state.as_ref().map(|state| state.status),
        scheduler_tick_count: input
            .scheduler_state
            .as_ref()
            .map_or(0, |state| state.tick_count),
        scheduler_event_count: input.scheduler_events.len(),
        worker_count: input.workers.len(),
        active_worker_count,
        blocked_worker_count,
        task_memory_entries: input.task_memory.entries().len(),
        route_feedback_entries: input.route_feedback.feedback().len(),
        routing_proposal_count: input.routing_proposals.len(),
        policy_ledger_entries: input
            .policy_ledger
            .as_ref()
            .map_or(0, |ledger| ledger.entries.len()),
        policy_lifecycle_count,
        policy_anomaly_count,
        autonomous_run_count: input.autonomous_runs.reports.len(),
        evaluation_total_score: input
            .evaluation
            .as_ref()
            .map(|evaluation| evaluation.scores.total_score),
    }
}

fn integration_components(
    input: &AutonomousIntegrationInput,
    summary: &AutonomousIntegrationSummary,
) -> Vec<AutonomousIntegrationComponent> {
    vec![
        component(
            "tasks",
            if summary.task_count == 0 {
                AutonomousIntegrationCheckStatus::Warning
            } else {
                AutonomousIntegrationCheckStatus::Passed
            },
            summary.task_count,
            None,
            if summary.task_count == 0 {
                "no durable tasks are present"
            } else {
                "durable task registry is populated"
            },
        ),
        component(
            "scheduler",
            if input.scheduler_state.is_some() {
                AutonomousIntegrationCheckStatus::Passed
            } else if summary.task_count == 0 {
                AutonomousIntegrationCheckStatus::Warning
            } else {
                AutonomousIntegrationCheckStatus::Warning
            },
            summary.scheduler_tick_count as usize,
            None,
            if input.scheduler_state.is_some() {
                "scheduler daemon state is available"
            } else {
                "scheduler daemon state has not been recorded"
            },
        ),
        component(
            "workers",
            if summary.blocked_worker_count > 0 {
                AutonomousIntegrationCheckStatus::Warning
            } else {
                AutonomousIntegrationCheckStatus::Passed
            },
            summary.worker_count,
            None,
            if summary.blocked_worker_count > 0 {
                "one or more workers are blocked"
            } else {
                "worker registry is not blocked"
            },
        ),
        component(
            "memory",
            if summary.task_count > 0 && summary.task_memory_entries == 0 {
                AutonomousIntegrationCheckStatus::Warning
            } else {
                AutonomousIntegrationCheckStatus::Passed
            },
            summary.task_memory_entries,
            Some(summary.task_count),
            "task memory coverage",
        ),
        component(
            "routing",
            if summary.task_count > 0 && summary.route_feedback_entries == 0 {
                AutonomousIntegrationCheckStatus::Warning
            } else {
                AutonomousIntegrationCheckStatus::Passed
            },
            summary.route_feedback_entries,
            None,
            "route feedback coverage",
        ),
        component(
            "policy",
            if summary.policy_anomaly_count > 0 {
                AutonomousIntegrationCheckStatus::Warning
            } else {
                AutonomousIntegrationCheckStatus::Passed
            },
            summary.policy_lifecycle_count,
            None,
            if summary.policy_anomaly_count > 0 {
                "policy replay contains anomalies"
            } else {
                "policy replay has no anomalies"
            },
        ),
        component(
            "evaluation",
            if input.evaluation.is_some() {
                AutonomousIntegrationCheckStatus::Passed
            } else {
                AutonomousIntegrationCheckStatus::Warning
            },
            usize::from(input.evaluation.is_some()),
            Some(1),
            if input.evaluation.is_some() {
                "autonomous evaluation report is available"
            } else {
                "autonomous evaluation report is unavailable"
            },
        ),
    ]
}

fn component(
    name: &str,
    status: AutonomousIntegrationCheckStatus,
    observed: usize,
    expected: Option<usize>,
    message: &str,
) -> AutonomousIntegrationComponent {
    AutonomousIntegrationComponent {
        name: name.to_string(),
        status,
        observed,
        expected,
        message: message.to_string(),
    }
}

fn integration_invariants(
    input: &AutonomousIntegrationInput,
    summary: &AutonomousIntegrationSummary,
) -> Vec<AutonomousIntegrationInvariant> {
    let task_ids = input
        .tasks
        .iter()
        .map(|task| task.task_id.as_str())
        .collect::<BTreeSet<_>>();
    let worker_ids = input
        .workers
        .iter()
        .map(|worker| worker.worker_id.as_str())
        .collect::<BTreeSet<_>>();
    let task_event_ids = input
        .task_events
        .iter()
        .map(|event| event.task_id.as_str())
        .collect::<BTreeSet<_>>();
    let task_ledger_ids = input
        .task_ledger
        .iter()
        .map(|entry| entry.task_id.as_str())
        .collect::<BTreeSet<_>>();

    let missing_task_events = task_ids
        .iter()
        .filter(|task_id| {
            !task_event_ids.contains(**task_id) && !task_ledger_ids.contains(**task_id)
        })
        .map(|task_id| (*task_id).to_string())
        .collect::<Vec<_>>();
    let unknown_queue_tasks = input
        .scheduler_queue
        .iter()
        .filter(|task| !task_ids.contains(task.task_id.as_str()))
        .map(|task| task.task_id.clone())
        .collect::<Vec<_>>();
    let unknown_scheduler_event_tasks = input
        .scheduler_events
        .iter()
        .filter_map(|event| event.selected_task_id.as_deref())
        .filter(|task_id| !task_ids.contains(*task_id))
        .map(str::to_string)
        .collect::<Vec<_>>();
    let duplicate_worker_ids =
        duplicates(input.workers.iter().map(|worker| worker.worker_id.as_str()));
    let workers_without_events = input
        .workers
        .iter()
        .filter(|worker| worker.events.is_empty())
        .map(|worker| worker.worker_id.clone())
        .collect::<Vec<_>>();
    let memory_unknown_tasks = input
        .task_memory
        .entries()
        .iter()
        .filter(|entry| !task_ids.contains(entry.task_id.as_str()))
        .map(|entry| entry.task_id.clone())
        .collect::<Vec<_>>();
    let route_unknown_tasks = input
        .route_feedback
        .feedback()
        .iter()
        .filter(|feedback| !task_ids.contains(feedback.task_id.as_str()))
        .map(|feedback| feedback.task_id.clone())
        .collect::<Vec<_>>();
    let policy_unknown_routing_proposals = policy_unknown_routing_proposals(input);
    let evaluation_mismatches = evaluation_mismatches(input);
    let mut unknown_plan_worker_ids = Vec::new();
    for task in &input.tasks {
        let Some(plan) = &task.plan else {
            continue;
        };
        for node in plan.execution.nodes.values() {
            let Some(worker_id) = node.worker_id.as_deref() else {
                continue;
            };
            if !worker_ids.contains(worker_id) {
                unknown_plan_worker_ids.push(format!("{}:{worker_id}", task.task_id));
            }
        }
    }

    vec![
        invariant(
            "task_events_cover_tasks",
            missing_task_events.is_empty(),
            AutonomousIntegrationSeverity::Error,
            "every durable task should have a task event or progress ledger entry",
            missing_task_events,
        ),
        invariant(
            "scheduler_queue_tasks_exist",
            unknown_queue_tasks.is_empty(),
            AutonomousIntegrationSeverity::Error,
            "scheduler queue should only reference known tasks",
            unknown_queue_tasks,
        ),
        invariant(
            "scheduler_events_tasks_exist",
            unknown_scheduler_event_tasks.is_empty(),
            AutonomousIntegrationSeverity::Error,
            "scheduler events should only select known tasks",
            unknown_scheduler_event_tasks,
        ),
        invariant(
            "worker_ids_unique",
            duplicate_worker_ids.is_empty(),
            AutonomousIntegrationSeverity::Error,
            "worker registry should not contain duplicate worker ids",
            duplicate_worker_ids,
        ),
        invariant(
            "worker_events_present",
            workers_without_events.is_empty(),
            AutonomousIntegrationSeverity::Warning,
            "workers should retain lifecycle events for replay",
            workers_without_events,
        ),
        invariant(
            "memory_entries_reference_tasks",
            memory_unknown_tasks.is_empty(),
            AutonomousIntegrationSeverity::Error,
            "task memory entries should reference known tasks",
            memory_unknown_tasks,
        ),
        invariant(
            "route_feedback_reference_tasks",
            route_unknown_tasks.is_empty(),
            AutonomousIntegrationSeverity::Error,
            "route feedback entries should reference known tasks",
            route_unknown_tasks,
        ),
        invariant(
            "policy_replay_routing_proposals_known",
            policy_unknown_routing_proposals.is_empty(),
            AutonomousIntegrationSeverity::Warning,
            "routing policy lifecycles should map to current routing proposal history when available",
            policy_unknown_routing_proposals,
        ),
        invariant(
            "evaluation_counters_match_inputs",
            evaluation_mismatches.is_empty(),
            AutonomousIntegrationSeverity::Error,
            "autonomous evaluation counters should match the diagnostic input snapshot",
            evaluation_mismatches,
        ),
        invariant(
            "policy_replay_has_no_anomalies",
            summary.policy_anomaly_count == 0,
            AutonomousIntegrationSeverity::Warning,
            "policy lifecycle replay should be free of anomalies",
            input
                .policy_replay
                .as_ref()
                .map(|replay| {
                    replay
                        .anomalies
                        .iter()
                        .take(5)
                        .map(|anomaly| format!("{}:{}", anomaly.kind, anomaly.entry_id))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        ),
        invariant(
            "plan_worker_ids_exist",
            unknown_plan_worker_ids.is_empty(),
            AutonomousIntegrationSeverity::Error,
            "plan execution nodes should only reference known workers",
            unknown_plan_worker_ids,
        ),
    ]
}

fn invariant(
    name: &str,
    passed: bool,
    severity: AutonomousIntegrationSeverity,
    message: &str,
    evidence: Vec<String>,
) -> AutonomousIntegrationInvariant {
    AutonomousIntegrationInvariant {
        name: name.to_string(),
        status: if passed {
            AutonomousIntegrationCheckStatus::Passed
        } else if severity == AutonomousIntegrationSeverity::Error {
            AutonomousIntegrationCheckStatus::Failed
        } else {
            AutonomousIntegrationCheckStatus::Warning
        },
        severity,
        message: message.to_string(),
        evidence,
    }
}

fn policy_unknown_routing_proposals(input: &AutonomousIntegrationInput) -> Vec<String> {
    let known = input
        .routing_proposals
        .iter()
        .map(|proposal| proposal.id.as_str())
        .collect::<BTreeSet<_>>();
    let Some(replay) = &input.policy_replay else {
        return Vec::new();
    };
    if known.is_empty() {
        return Vec::new();
    }
    replay
        .lifecycles
        .iter()
        .filter(|lifecycle| lifecycle.domain == PolicyDomain::Routing)
        .filter(|lifecycle| !known.contains(lifecycle.proposal_id.as_str()))
        .map(|lifecycle| lifecycle.proposal_id.clone())
        .collect()
}

fn evaluation_mismatches(input: &AutonomousIntegrationInput) -> Vec<String> {
    let Some(evaluation) = &input.evaluation else {
        return Vec::new();
    };
    let mut mismatches = Vec::new();
    compare_counter(
        &mut mismatches,
        "tasks",
        evaluation.counters.tasks,
        input.tasks.len(),
    );
    compare_counter(
        &mut mismatches,
        "task_memory_entries",
        evaluation.counters.task_memory_entries,
        input.task_memory.entries().len(),
    );
    compare_counter(
        &mut mismatches,
        "route_feedback_entries",
        evaluation.counters.route_feedback_entries,
        input.route_feedback.feedback().len(),
    );
    if let Some(replay) = &input.policy_replay {
        compare_counter(
            &mut mismatches,
            "policy_lifecycle_events",
            evaluation.counters.policy_lifecycle_events,
            replay.summary.event_count,
        );
        compare_counter(
            &mut mismatches,
            "policy_replay_anomalies",
            evaluation.counters.policy_replay_anomalies,
            replay.summary.anomaly_count,
        );
    }
    mismatches
}

fn compare_counter(mismatches: &mut Vec<String>, name: &str, observed: usize, expected: usize) {
    if observed != expected {
        mismatches.push(format!("{name}: observed={observed} expected={expected}"));
    }
}

fn integration_replay_summary(
    input: &AutonomousIntegrationInput,
) -> AutonomousIntegrationReplaySummary {
    let mut stages = Vec::new();
    let task_events = input
        .task_events
        .iter()
        .map(|event| event.event.as_str())
        .collect::<BTreeSet<_>>();
    stages.push(stage(
        "task_lifecycle",
        !input.tasks.is_empty() && task_events.contains("created"),
        AutonomousIntegrationCheckStatus::Warning,
        vec![format!(
            "tasks={}, task_events={}",
            input.tasks.len(),
            input.task_events.len()
        )],
    ));
    stages.push(stage(
        "scheduler_queue",
        !input.scheduler_queue.is_empty()
            || input
                .scheduler_state
                .as_ref()
                .and_then(|state| state.last_tick.as_ref())
                .is_some(),
        AutonomousIntegrationCheckStatus::Warning,
        vec![format!(
            "queue={}, ticks={}",
            input.scheduler_queue.len(),
            input
                .scheduler_state
                .as_ref()
                .map_or(0, |state| state.tick_count)
        )],
    ));
    stages.push(stage(
        "worker_lifecycle",
        !input.workers.is_empty()
            || input
                .evaluation
                .as_ref()
                .is_some_and(|evaluation| evaluation.counters.worker_dispatches > 0),
        AutonomousIntegrationCheckStatus::Warning,
        vec![format!("workers={}", input.workers.len())],
    ));
    stages.push(stage(
        "memory_feedback",
        !input.task_memory.entries().is_empty(),
        AutonomousIntegrationCheckStatus::Warning,
        vec![format!(
            "memory_entries={}",
            input.task_memory.entries().len()
        )],
    ));
    stages.push(stage(
        "routing_feedback",
        !input.route_feedback.feedback().is_empty() || !input.routing_proposals.is_empty(),
        AutonomousIntegrationCheckStatus::Warning,
        vec![format!(
            "route_feedback={}, routing_proposals={}",
            input.route_feedback.feedback().len(),
            input.routing_proposals.len()
        )],
    ));
    let routing_apply_lifecycles = input.policy_replay.as_ref().map_or(0, |replay| {
        replay
            .lifecycles
            .iter()
            .filter(|lifecycle| lifecycle.domain == PolicyDomain::Routing)
            .filter(|lifecycle| {
                lifecycle
                    .events
                    .iter()
                    .any(|event| event.action.contains("routing_policy_overlay"))
            })
            .count()
    });
    stages.push(stage(
        "routing_policy_replay",
        routing_apply_lifecycles > 0,
        AutonomousIntegrationCheckStatus::Warning,
        vec![format!(
            "routing_apply_lifecycles={routing_apply_lifecycles}"
        )],
    ));
    stages.push(stage(
        "autonomous_evaluation",
        input.evaluation.is_some(),
        AutonomousIntegrationCheckStatus::Warning,
        vec![format!(
            "evaluation_total_score={:?}",
            input
                .evaluation
                .as_ref()
                .map(|evaluation| evaluation.scores.total_score)
        )],
    ));

    let passed_stage_count = stages
        .iter()
        .filter(|stage| stage.status == AutonomousIntegrationCheckStatus::Passed)
        .count();
    let warning_stage_count = stages
        .iter()
        .filter(|stage| stage.status == AutonomousIntegrationCheckStatus::Warning)
        .count();
    let failed_stage_count = stages
        .iter()
        .filter(|stage| stage.status == AutonomousIntegrationCheckStatus::Failed)
        .count();
    let status = if failed_stage_count > 0 {
        AutonomousIntegrationCheckStatus::Failed
    } else if warning_stage_count > 0 {
        AutonomousIntegrationCheckStatus::Warning
    } else {
        AutonomousIntegrationCheckStatus::Passed
    };
    let path_signature = stages
        .iter()
        .map(|stage| format!("{}:{:?}", stage.name, stage.status))
        .collect();
    AutonomousIntegrationReplaySummary {
        status,
        stage_count: stages.len(),
        passed_stage_count,
        warning_stage_count,
        failed_stage_count,
        stages,
        path_signature,
    }
}

fn stage(
    name: &str,
    passed: bool,
    missing_status: AutonomousIntegrationCheckStatus,
    evidence: Vec<String>,
) -> AutonomousIntegrationReplayStage {
    AutonomousIntegrationReplayStage {
        name: name.to_string(),
        status: if passed {
            AutonomousIntegrationCheckStatus::Passed
        } else {
            missing_status
        },
        evidence,
    }
}

fn integration_status(
    components: &[AutonomousIntegrationComponent],
    invariants: &[AutonomousIntegrationInvariant],
    replay: &AutonomousIntegrationReplaySummary,
) -> AutonomousIntegrationStatus {
    if invariants
        .iter()
        .any(|invariant| invariant.status == AutonomousIntegrationCheckStatus::Failed)
        || components
            .iter()
            .any(|component| component.status == AutonomousIntegrationCheckStatus::Failed)
        || replay.status == AutonomousIntegrationCheckStatus::Failed
    {
        AutonomousIntegrationStatus::Blocked
    } else if invariants
        .iter()
        .any(|invariant| invariant.status == AutonomousIntegrationCheckStatus::Warning)
        || components
            .iter()
            .any(|component| component.status == AutonomousIntegrationCheckStatus::Warning)
        || replay.status == AutonomousIntegrationCheckStatus::Warning
    {
        AutonomousIntegrationStatus::Degraded
    } else {
        AutonomousIntegrationStatus::Healthy
    }
}

fn integration_recommendations(
    status: AutonomousIntegrationStatus,
    summary: &AutonomousIntegrationSummary,
    invariants: &[AutonomousIntegrationInvariant],
    replay: &AutonomousIntegrationReplaySummary,
) -> Vec<String> {
    let mut recommendations = Vec::new();
    if status == AutonomousIntegrationStatus::Blocked {
        recommendations.push(
            "Resolve failed integration invariants before trusting autonomous apply loops."
                .to_string(),
        );
    }
    if summary.task_count > 0 && summary.task_memory_entries == 0 {
        recommendations.push("Refresh task memory from the task registry before enabling memory-informed policy decisions.".to_string());
    }
    if summary.task_count > 0 && summary.route_feedback_entries == 0 {
        recommendations.push(
            "Record route feedback for executed tasks before proposing MoE routing policy changes."
                .to_string(),
        );
    }
    if summary.policy_anomaly_count > 0 {
        recommendations.push("Inspect policy lifecycle replay anomalies before applying new governed policy changes.".to_string());
    }
    if replay.warning_stage_count > 0 {
        recommendations.push("Complete missing golden replay stages with daemon report, evaluation, routing feedback, and policy replay evidence.".to_string());
    }
    for invariant in invariants.iter().filter(|invariant| {
        invariant.status == AutonomousIntegrationCheckStatus::Failed
            && !invariant.evidence.is_empty()
    }) {
        recommendations.push(format!(
            "{} failed for {} item(s).",
            invariant.name,
            invariant.evidence.len()
        ));
    }
    if recommendations.is_empty() {
        recommendations.push("Autonomous integration diagnostics are healthy.".to_string());
    }
    recommendations
}

fn duplicates<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut counts = BTreeMap::<&str, usize>::new();
    for value in values {
        *counts.entry(value).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(value, _)| value.to_string())
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        evaluate_autonomous_loop, replay_policy_lifecycle_entries, AutonomousEvaluationInput,
        ModelRouteDecision, ModelRouteFeedback, ModelRoutePhase, PermissionMode, PolicyDecision,
        PolicyLedgerEntry, PolicyLedgerStatus, PolicyLifecycleReplaySummary, PolicyProposal,
        PolicyReference, PolicyReviewSummary, PolicyRiskLevel, RouteFeedbackStore,
        SchedulerDaemonStatus, TaskMemoryStore, WorkerRegistry,
    };

    fn route_feedback(task_id: &str) -> ModelRouteFeedback {
        ModelRouteFeedback::pending(
            task_id,
            ModelRouteDecision {
                phase: ModelRoutePhase::Coding,
                model: "sonnet".to_string(),
                provider: Some("anthropic".to_string()),
                reason: "test route".to_string(),
                confidence: Some(0.8),
                fallback_model: Some("opus".to_string()),
            },
            1,
        )
        .with_outcome(true, Some(true), false, Some("ok".to_string()))
    }

    fn policy_entry(proposal: PolicyProposal, status: PolicyLedgerStatus) -> PolicyLedgerEntry {
        PolicyLedgerEntry {
            version: crate::POLICY_GOVERNANCE_VERSION,
            id: format!("policy-{status:?}"),
            timestamp: 1,
            status,
            domains: vec![proposal.domain],
            proposals: vec![proposal.clone()],
            decisions: vec![PolicyDecision {
                domain: proposal.domain,
                action: "governed_policy_apply".to_string(),
                status,
                risk: proposal.risk,
                reason: "test".to_string(),
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
                routing_proposal_count: 1,
                scheduler_status: None,
                autonomous_action: None,
                autonomous_review_required: false,
                memory_reuse_score: None,
            },
        }
    }

    fn routing_policy_proposal(id: &str) -> PolicyProposal {
        PolicyProposal {
            id: id.to_string(),
            domain: PolicyDomain::Routing,
            action: "apply_routing_policy_overlay".to_string(),
            status: PolicyLedgerStatus::Proposed,
            risk: PolicyRiskLevel::Medium,
            summary: "test routing proposal".to_string(),
            source_fingerprint: "fingerprint".to_string(),
            references: vec![PolicyReference {
                label: "route_policy_proposal".to_string(),
                path: None,
                id: Some(id.to_string()),
            }],
            gates: Vec::new(),
        }
    }

    fn base_input() -> AutonomousIntegrationInput {
        let registry = crate::TaskRegistry::new();
        let task = registry.create("integrate autonomous loop", Some("integration"));
        registry
            .record_route_feedback(&task.task_id, route_feedback(&task.task_id))
            .expect("feedback should record");
        registry
            .set_status(&task.task_id, TaskStatus::Completed)
            .expect("task should complete");
        let tasks = registry.list(None);
        let task_memory = TaskMemoryStore::from_tasks(&tasks);
        let route_feedback = RouteFeedbackStore::from_feedback(tasks[0].route_feedback.clone());
        let routing_proposal = routing_policy_proposal("route-proposal-1");
        let policy_replay = replay_policy_lifecycle_entries(crate::PolicyLedgerLoad {
            ledger_path: PathBuf::from("policy/ledger.jsonl"),
            entries: vec![
                policy_entry(routing_proposal.clone(), PolicyLedgerStatus::Planned),
                policy_entry(routing_proposal.clone(), PolicyLedgerStatus::Applied),
            ],
            malformed_lines: 0,
            warnings: Vec::new(),
        });
        let autonomous_runs = crate::AutonomousRunLoad {
            runs_path: PathBuf::from("runs.jsonl"),
            reports: Vec::new(),
            malformed_lines: 0,
            warnings: Vec::new(),
        };
        let worker_registry = WorkerRegistry::new();
        let _worker = worker_registry.create(".", &[".".to_string()], true);
        let evaluation = evaluate_autonomous_loop(AutonomousEvaluationInput {
            tasks: tasks.clone(),
            task_memory: task_memory.clone(),
            route_feedback: route_feedback.clone(),
            autonomous_runs: autonomous_runs.clone(),
            permission_mode: PermissionMode::ReadOnly,
            requested_max_ticks: 3,
            policy_replay: Some(policy_replay.clone()),
        });
        AutonomousIntegrationInput {
            tasks,
            task_ledger: registry.ledger(),
            task_events: registry.event_log(),
            scheduler_state: Some(SchedulerDaemonState {
                version: 1,
                status: SchedulerDaemonStatus::Idle,
                pid: 0,
                started_at: 1,
                updated_at: 2,
                tick_count: 1,
                lock_path: PathBuf::from("scheduler.lock"),
                last_tick: None,
                message: "idle".to_string(),
            }),
            scheduler_events: Vec::new(),
            scheduler_queue: crate::DurableTaskScheduler::new(
                registry.clone(),
                crate::VerificationRunner::new(None),
            )
            .queue(),
            workers: worker_registry.list(),
            task_memory,
            route_feedback,
            routing_proposals: Vec::new(),
            policy_ledger: Some(crate::PolicyLedgerLoad {
                ledger_path: PathBuf::from("policy/ledger.jsonl"),
                entries: Vec::new(),
                malformed_lines: 0,
                warnings: Vec::new(),
            }),
            policy_replay: Some(policy_replay),
            autonomous_runs,
            evaluation: Some(evaluation),
        }
    }

    #[test]
    fn integration_report_summarizes_cross_domain_state() {
        let report = review_autonomous_integration(base_input());

        assert_eq!(report.summary.task_count, 1);
        assert_eq!(report.summary.task_memory_entries, 1);
        assert_eq!(report.summary.route_feedback_entries, 1);
        assert_eq!(report.summary.policy_lifecycle_count, 1);
        assert!(report
            .invariants
            .iter()
            .all(|invariant| invariant.status != AutonomousIntegrationCheckStatus::Failed));
        assert!(report
            .replay
            .stages
            .iter()
            .any(|stage| stage.name == "routing_policy_replay"
                && stage.status == AutonomousIntegrationCheckStatus::Passed));
    }

    #[test]
    fn integration_report_flags_unknown_feedback_task() {
        let mut input = base_input();
        input.route_feedback = RouteFeedbackStore::from_feedback(vec![route_feedback("missing")]);
        input.evaluation = None;

        let report = review_autonomous_integration(input);

        assert_eq!(report.status, AutonomousIntegrationStatus::Blocked);
        let invariant = report
            .invariants
            .iter()
            .find(|invariant| invariant.name == "route_feedback_reference_tasks")
            .expect("route feedback invariant");
        assert_eq!(invariant.status, AutonomousIntegrationCheckStatus::Failed);
        assert_eq!(invariant.evidence, vec!["missing".to_string()]);
    }

    #[test]
    fn legacy_replay_summary_missing_distributions_still_deserializes() {
        let summary: PolicyLifecycleReplaySummary = serde_json::from_value(serde_json::json!({
            "lifecycle_count": 0,
            "event_count": 0,
            "anomaly_count": 0,
            "malformed_lines": 0
        }))
        .expect("legacy summary should deserialize");

        assert!(summary.domain_counts.is_empty());
        assert!(summary.action_counts.is_empty());
        assert!(summary.anomaly_kind_counts.is_empty());
    }
}
