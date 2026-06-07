use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::task_registry::Task as RegistryTask;
use crate::{
    recommend_autonomous_policy_for_summary, summarize_autonomous_run_reports,
    AutonomousPolicyAction, AutonomousPolicyRecommendation, AutonomousRunHistorySummary,
    AutonomousRunLoad, AutonomousRunStatus, ModelRouteFeedback, PermissionMode, RouteFeedbackStore,
    RouteFeedbackSummary, TaskMemoryStore, TaskMemorySummary, TaskStatus,
};

pub const AUTONOMOUS_EVALUATION_VERSION: u32 = 1;
pub const AUTONOMOUS_BENCHMARK_SUITE_ID: &str = "autonomous-agent-loop-v1";
pub const AUTONOMOUS_BENCHMARK_VERSION: &str = "2026.06";

#[derive(Debug, Clone)]
pub struct AutonomousEvaluationInput {
    pub tasks: Vec<RegistryTask>,
    pub task_memory: TaskMemoryStore,
    pub route_feedback: RouteFeedbackStore,
    pub autonomous_runs: AutonomousRunLoad,
    pub permission_mode: PermissionMode,
    pub requested_max_ticks: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousEvaluationScores {
    pub task_completion_score: f32,
    pub recovery_quality_score: f32,
    pub scheduler_efficiency_score: f32,
    pub memory_reuse_score: f32,
    pub routing_adaptation_score: f32,
    pub autonomous_success_score: f32,
    pub total_score: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomousEvaluationCounters {
    pub tasks: usize,
    pub completed_tasks: usize,
    pub blocked_tasks: usize,
    pub failed_tasks: usize,
    pub recovery_triggered_tasks: usize,
    pub recovered_tasks: usize,
    pub task_memory_entries: usize,
    pub route_feedback_entries: usize,
    pub route_failures: usize,
    pub route_recovery_triggered: usize,
    pub autonomous_runs: usize,
    pub malformed_run_lines: usize,
    pub scheduler_ticks: usize,
    pub productive_scheduler_ticks: usize,
    pub worker_dispatches: usize,
    pub worker_completions: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousEvaluationReport {
    pub version: u32,
    pub evaluated_at: u64,
    pub counters: AutonomousEvaluationCounters,
    pub scores: AutonomousEvaluationScores,
    pub run_summary: AutonomousRunHistorySummary,
    pub route_summaries: Vec<RouteFeedbackSummary>,
    pub task_memory_summaries: Vec<TaskMemorySummary>,
    pub trace_replay: AutonomousTraceReplayReport,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousTraceReplayDecision {
    pub seq: usize,
    pub run_id: String,
    pub observed_status: AutonomousRunStatus,
    pub observed_max_ticks: usize,
    pub observed_tick_count: usize,
    pub original_action: Option<AutonomousPolicyAction>,
    pub original_recommended_max_ticks: Option<usize>,
    pub replay_action: AutonomousPolicyAction,
    pub replay_recommended_max_ticks: usize,
    pub changed: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousTraceReplayReport {
    pub considered_runs: usize,
    pub changed_decisions: usize,
    pub policy_recommendation: AutonomousPolicyRecommendation,
    pub decisions: Vec<AutonomousTraceReplayDecision>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousBenchmarkRun {
    pub suite_id: String,
    pub version: String,
    pub evaluation: AutonomousEvaluationReport,
}

pub fn evaluate_autonomous_loop(input: AutonomousEvaluationInput) -> AutonomousEvaluationReport {
    let run_summary = summarize_autonomous_run_reports(
        input.autonomous_runs.reports.clone(),
        input.autonomous_runs.runs_path.clone(),
        input.autonomous_runs.malformed_lines,
    );
    let route_summaries = input.route_feedback.summaries();
    let task_memory_summaries = input.task_memory.summaries();
    let counters = evaluation_counters(
        &input.tasks,
        &input.task_memory,
        input.route_feedback.feedback(),
        &input.autonomous_runs,
    );
    let scores = evaluation_scores(&counters, &run_summary, input.route_feedback.feedback());
    let trace_replay = replay_autonomous_trace(
        &input.autonomous_runs,
        input.permission_mode,
        input.requested_max_ticks,
    );
    let recommendations =
        evaluation_recommendations(&counters, &scores, &run_summary, &trace_replay);

    AutonomousEvaluationReport {
        version: AUTONOMOUS_EVALUATION_VERSION,
        evaluated_at: now_secs(),
        counters,
        scores,
        run_summary,
        route_summaries,
        task_memory_summaries,
        trace_replay,
        recommendations,
    }
}

pub fn run_autonomous_benchmark(input: AutonomousEvaluationInput) -> AutonomousBenchmarkRun {
    AutonomousBenchmarkRun {
        suite_id: AUTONOMOUS_BENCHMARK_SUITE_ID.to_string(),
        version: AUTONOMOUS_BENCHMARK_VERSION.to_string(),
        evaluation: evaluate_autonomous_loop(input),
    }
}

pub fn replay_autonomous_trace(
    run_load: &AutonomousRunLoad,
    permission_mode: PermissionMode,
    requested_max_ticks: usize,
) -> AutonomousTraceReplayReport {
    let requested_max_ticks = requested_max_ticks.max(1);
    let current_summary = summarize_autonomous_run_reports(
        run_load.reports.clone(),
        run_load.runs_path.clone(),
        run_load.malformed_lines,
    );
    let policy_recommendation = recommend_autonomous_policy_for_summary(
        &current_summary,
        requested_max_ticks,
        permission_mode,
    );

    let mut prefix = Vec::new();
    let mut decisions = Vec::new();
    for (index, report) in run_load.reports.iter().enumerate() {
        let prefix_summary = summarize_autonomous_run_reports(
            prefix.clone(),
            run_load.runs_path.clone(),
            run_load.malformed_lines,
        );
        let replay = recommend_autonomous_policy_for_summary(
            &prefix_summary,
            report.max_ticks.max(1),
            permission_mode,
        );
        let original = report.policy_recommendation.as_ref();
        let changed = original.is_some_and(|original| {
            original.action != replay.action
                || original.recommended_max_ticks != replay.recommended_max_ticks
        });
        decisions.push(AutonomousTraceReplayDecision {
            seq: index.saturating_add(1),
            run_id: report.run_id.clone(),
            observed_status: report.status,
            observed_max_ticks: report.max_ticks,
            observed_tick_count: report.tick_count,
            original_action: original.map(|recommendation| recommendation.action),
            original_recommended_max_ticks: original
                .map(|recommendation| recommendation.recommended_max_ticks),
            replay_action: replay.action,
            replay_recommended_max_ticks: replay.recommended_max_ticks,
            changed,
            reason: replay.reasons.join("; "),
        });
        prefix.push(report.clone());
    }
    let changed_decisions = decisions.iter().filter(|decision| decision.changed).count();
    let recommendations = replay_recommendations(run_load, changed_decisions);

    AutonomousTraceReplayReport {
        considered_runs: run_load.reports.len(),
        changed_decisions,
        policy_recommendation,
        decisions,
        recommendations,
    }
}

fn evaluation_counters(
    tasks: &[RegistryTask],
    task_memory: &TaskMemoryStore,
    route_feedback: &[ModelRouteFeedback],
    run_load: &AutonomousRunLoad,
) -> AutonomousEvaluationCounters {
    let route_failures = route_feedback
        .iter()
        .filter(|feedback| feedback.succeeded == Some(false))
        .count();
    let route_recovery_triggered = route_feedback
        .iter()
        .filter(|feedback| feedback.recovery_triggered)
        .count();
    let scheduler_ticks = run_load
        .reports
        .iter()
        .map(|report| report.scheduler_runs.len())
        .sum();
    let productive_scheduler_ticks = run_load
        .reports
        .iter()
        .flat_map(|report| report.scheduler_runs.iter())
        .filter(|run| run.tick.selected_task_id.is_some())
        .count();
    let worker_dispatches = tasks
        .iter()
        .flat_map(|task| task.execution_reports.iter())
        .flat_map(|report| report.outcome.steps.iter())
        .filter(|step| step.kind == crate::TaskExecutionStepKind::DispatchWorker)
        .count();
    let worker_completions = tasks
        .iter()
        .flat_map(|task| task.execution_reports.iter())
        .filter(|report| report.completed)
        .count();
    let recovery_triggered_tasks = task_memory
        .entries()
        .iter()
        .filter(|entry| entry.recovery_triggered)
        .count();
    let recovered_tasks = task_memory
        .entries()
        .iter()
        .filter(|entry| entry.recovery_triggered && entry.completed)
        .count();

    AutonomousEvaluationCounters {
        tasks: tasks.len(),
        completed_tasks: tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Completed)
            .count(),
        blocked_tasks: tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.status,
                    TaskStatus::Blocked | TaskStatus::WaitingForPermission
                )
            })
            .count(),
        failed_tasks: tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Failed)
            .count(),
        recovery_triggered_tasks,
        recovered_tasks,
        task_memory_entries: task_memory.entries().len(),
        route_feedback_entries: route_feedback.len(),
        route_failures,
        route_recovery_triggered,
        autonomous_runs: run_load.reports.len(),
        malformed_run_lines: run_load.malformed_lines,
        scheduler_ticks,
        productive_scheduler_ticks,
        worker_dispatches,
        worker_completions,
    }
}

fn evaluation_scores(
    counters: &AutonomousEvaluationCounters,
    run_summary: &AutonomousRunHistorySummary,
    route_feedback: &[ModelRouteFeedback],
) -> AutonomousEvaluationScores {
    let task_completion_score = ratio(counters.completed_tasks, counters.tasks);
    let recovery_quality_score = if counters.recovery_triggered_tasks == 0 {
        if counters.blocked_tasks == 0 && counters.failed_tasks == 0 {
            1.0
        } else {
            0.5
        }
    } else {
        ratio(counters.recovered_tasks, counters.recovery_triggered_tasks)
    };
    let scheduler_efficiency_score = if counters.scheduler_ticks == 0 {
        0.0
    } else {
        let productivity = ratio(
            counters.productive_scheduler_ticks,
            counters.scheduler_ticks,
        );
        let tick_pressure =
            (1.0 / (1.0 + (run_summary.average_ticks as f32 / 8.0))).clamp(0.0, 1.0);
        (productivity * 0.7 + tick_pressure * 0.3).clamp(0.0, 1.0)
    };
    let memory_reuse_score = if counters.tasks == 0 {
        0.0
    } else {
        ratio(counters.task_memory_entries, counters.tasks)
    };
    let routing_adaptation_score = routing_adaptation_score(route_feedback);
    let autonomous_success_score = if counters.autonomous_runs == 0 {
        0.0
    } else {
        (1.0 - run_summary.blocked_rate as f32).clamp(0.0, 1.0)
    };
    let total_score = (task_completion_score * 0.25
        + recovery_quality_score * 0.20
        + scheduler_efficiency_score * 0.20
        + memory_reuse_score * 0.15
        + routing_adaptation_score * 0.20)
        .clamp(0.0, 1.0);

    AutonomousEvaluationScores {
        task_completion_score,
        recovery_quality_score,
        scheduler_efficiency_score,
        memory_reuse_score,
        routing_adaptation_score,
        autonomous_success_score,
        total_score,
    }
}

fn routing_adaptation_score(route_feedback: &[ModelRouteFeedback]) -> f32 {
    if route_feedback.is_empty() {
        return 0.0;
    }
    let successes = route_feedback
        .iter()
        .filter(|feedback| feedback.succeeded != Some(false))
        .count();
    let success_rate = ratio(successes, route_feedback.len());
    let adaptive_evidence = route_feedback
        .iter()
        .filter(|feedback| {
            feedback.route.fallback_model.is_some()
                || feedback.route.reason.contains("adaptive")
                || feedback.recovery_decision.is_some()
                || feedback.plan_progress.is_some()
        })
        .count();
    let adaptive_rate = ratio(adaptive_evidence, route_feedback.len());
    (success_rate * 0.7 + adaptive_rate * 0.3).clamp(0.0, 1.0)
}

fn evaluation_recommendations(
    counters: &AutonomousEvaluationCounters,
    scores: &AutonomousEvaluationScores,
    run_summary: &AutonomousRunHistorySummary,
    trace_replay: &AutonomousTraceReplayReport,
) -> Vec<String> {
    let mut recommendations = Vec::new();
    if counters.tasks == 0 {
        recommendations.push(
            "No persisted tasks were found; run durable tasks before trusting autonomous quality scores."
                .to_string(),
        );
    }
    if counters.autonomous_runs == 0 {
        recommendations.push(
            "No autonomous run history is available; run tasks daemon start or cron run first."
                .to_string(),
        );
    }
    if run_summary.malformed_lines > 0 {
        recommendations.push(format!(
            "{} malformed autonomous run log line(s) were skipped during evaluation.",
            run_summary.malformed_lines
        ));
    }
    if run_summary.blocked_rate >= 0.5 && counters.autonomous_runs >= 2 {
        recommendations.push(
            "Recent autonomous runs are frequently blocked; inspect recovery policy before expanding execution windows."
                .to_string(),
        );
    }
    if scores.routing_adaptation_score < 0.5 && counters.route_feedback_entries > 0 {
        recommendations.push(
            "Route feedback exists but adaptation score is low; review failed phases before tuning MoE routing."
                .to_string(),
        );
    }
    if scores.memory_reuse_score < 0.5 && counters.tasks > 0 {
        recommendations.push(
            "Task memory coverage is low; persist task memory before relying on long-horizon planning feedback."
                .to_string(),
        );
    }
    if trace_replay.changed_decisions > 0 {
        recommendations.push(format!(
            "{} replayed autonomous policy decision(s) differ under the current policy.",
            trace_replay.changed_decisions
        ));
    }
    if recommendations.is_empty() {
        recommendations.push(
            "Autonomous evaluation found no immediate policy or telemetry blockers.".to_string(),
        );
    }
    recommendations
}

fn replay_recommendations(run_load: &AutonomousRunLoad, changed_decisions: usize) -> Vec<String> {
    let mut recommendations = Vec::new();
    if run_load.reports.is_empty() {
        recommendations.push("No autonomous run trace is available for replay.".to_string());
    }
    if run_load.malformed_lines > 0 {
        recommendations.push(format!(
            "{} malformed autonomous run log line(s) were ignored during replay.",
            run_load.malformed_lines
        ));
    }
    if changed_decisions > 0 {
        recommendations.push(
            "Current autonomous policy would make different recommendations for part of the historical trace."
                .to_string(),
        );
    }
    if recommendations.is_empty() {
        recommendations
            .push("Historical autonomous trace is stable under current policy.".to_string());
    }
    recommendations
}

fn ratio(numerator: usize, denominator: usize) -> f32 {
    if denominator == 0 {
        0.0
    } else {
        (numerator as f32 / denominator as f32).clamp(0.0, 1.0)
    }
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
        append_autonomous_run_report, AutonomousRunReport, ModelRouteDecision, ModelRoutePhase,
        TaskRegistry,
    };

    fn run_report(
        run_id: &str,
        status: AutonomousRunStatus,
        max_ticks: usize,
        tick_count: usize,
    ) -> AutonomousRunReport {
        AutonomousRunReport {
            run_id: run_id.to_string(),
            started_at: 1,
            updated_at: 1,
            status,
            permission_mode: PermissionMode::ReadOnly.as_str().to_string(),
            max_ticks,
            tick_count,
            prior_summary: None,
            policy_recommendation: None,
            worker_supervisor_ticks: Vec::new(),
            scheduler_runs: Vec::new(),
            policy_audit: Vec::new(),
            final_queue: Vec::new(),
            latest_daemon_state: None,
            message: status.to_string(),
        }
    }

    #[test]
    fn evaluation_scores_tasks_memory_routes_and_runs() {
        let registry = TaskRegistry::new();
        let completed = registry.create("done", Some("coding"));
        registry
            .set_status(&completed.task_id, TaskStatus::Completed)
            .expect("completed status should update");
        let blocked = registry.create("blocked", Some("coding"));
        registry
            .set_status(&blocked.task_id, TaskStatus::Blocked)
            .expect("blocked status should update");
        let tasks = registry.list(None);
        let memory = TaskMemoryStore::from_tasks(&tasks);
        let route = ModelRouteDecision {
            phase: ModelRoutePhase::Verification,
            model: "opus".to_string(),
            provider: Some("anthropic".to_string()),
            reason: "adaptive route selected from route feedback".to_string(),
            confidence: Some(0.8),
            fallback_model: Some("sonnet".to_string()),
        };
        let route_feedback = RouteFeedbackStore::from_feedback(vec![ModelRouteFeedback::pending(
            &completed.task_id,
            route,
            1,
        )
        .with_outcome(true, Some(true), false, Some("verified".to_string()))]);
        let run_load = AutonomousRunLoad {
            runs_path: PathBuf::from("runs.jsonl"),
            reports: vec![run_report("run-1", AutonomousRunStatus::Running, 3, 1)],
            malformed_lines: 0,
            warnings: Vec::new(),
        };

        let report = evaluate_autonomous_loop(AutonomousEvaluationInput {
            tasks,
            task_memory: memory,
            route_feedback,
            autonomous_runs: run_load,
            permission_mode: PermissionMode::ReadOnly,
            requested_max_ticks: 3,
        });

        assert_eq!(report.counters.tasks, 2);
        assert_eq!(report.counters.completed_tasks, 1);
        assert_eq!(report.counters.blocked_tasks, 1);
        assert_eq!(report.counters.route_feedback_entries, 1);
        assert_eq!(report.run_summary.considered_runs, 1);
        assert!(report.scores.task_completion_score > 0.4);
        assert!(report.scores.routing_adaptation_score > 0.9);
        assert_eq!(report.trace_replay.considered_runs, 1);
    }

    #[test]
    fn trace_replay_detects_policy_drift() {
        let dir = std::env::temp_dir().join(format!("himalaya-eval-{}", now_secs()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut first = run_report("run-1", AutonomousRunStatus::Blocked, 4, 1);
        first.policy_recommendation = Some(AutonomousPolicyRecommendation {
            requested_max_ticks: 4,
            recommended_max_ticks: 4,
            permission_mode: PermissionMode::Prompt.as_str().to_string(),
            conservative_permission_mode: PermissionMode::Prompt.as_str().to_string(),
            action: AutonomousPolicyAction::Continue,
            review_required: false,
            cool_down: false,
            reasons: vec!["legacy policy".to_string()],
        });
        append_autonomous_run_report(&dir, &first).expect("first report should append");
        append_autonomous_run_report(
            &dir,
            &run_report("run-2", AutonomousRunStatus::Blocked, 4, 1),
        )
        .expect("second report should append");
        let mut third = run_report("run-3", AutonomousRunStatus::Blocked, 4, 1);
        third.policy_recommendation = Some(AutonomousPolicyRecommendation {
            requested_max_ticks: 4,
            recommended_max_ticks: 4,
            permission_mode: PermissionMode::Prompt.as_str().to_string(),
            conservative_permission_mode: PermissionMode::Prompt.as_str().to_string(),
            action: AutonomousPolicyAction::Continue,
            review_required: false,
            cool_down: false,
            reasons: vec!["legacy policy".to_string()],
        });
        append_autonomous_run_report(&dir, &third).expect("third report should append");
        let run_load =
            crate::load_autonomous_run_reports_with_diagnostics(&dir, 20).expect("runs load");

        let replay = replay_autonomous_trace(&run_load, PermissionMode::Prompt, 4);

        assert_eq!(replay.considered_runs, 3);
        assert!(replay.changed_decisions >= 1);
        assert_eq!(
            replay.policy_recommendation.action,
            AutonomousPolicyAction::RequestReview
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
