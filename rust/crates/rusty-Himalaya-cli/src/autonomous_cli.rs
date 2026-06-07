use std::path::Path;

use serde_json::Value;

pub(crate) fn render_autonomous_preflight_blocked_text(value: &Value) -> String {
    let operation = value["operation"].as_str().unwrap_or("operation");
    let status = value["status"].as_str().unwrap_or("unknown");
    let next_action = value["next_action"]
        .as_str()
        .unwrap_or("Inspect daemon report.");
    let mut lines = vec![
        "Autonomous preflight blocked".to_string(),
        format!("  Operation         {operation}"),
        format!("  Status            {status}"),
        format!("  Next action       {next_action}"),
        format!(
            "  Safety            iterate={} apply_policy={}",
            value["safe_to_iterate"].as_bool().unwrap_or(false),
            value["safe_to_apply_policy"].as_bool().unwrap_or(false)
        ),
    ];
    if let Some(blockers) = value["health"]["blockers"].as_array() {
        for blocker in blockers.iter().filter_map(Value::as_str).take(5) {
            lines.push(format!("  Blocker           {blocker}"));
        }
    }
    if let Some(recommendations) = value["recommendations"].as_array() {
        lines.push("Recommendations:".to_string());
        for recommendation in recommendations.iter().filter_map(Value::as_str).take(5) {
            lines.push(format!("  - {recommendation}"));
        }
    }
    lines.join("\n")
}

pub(crate) fn render_autonomous_evaluation_text(value: &Value) -> String {
    let report = value
        .get("evaluation")
        .or_else(|| value.get("report"))
        .unwrap_or(value);
    let scores = &report["scores"];
    let counters = &report["counters"];
    let total = scores["total_score"].as_f64().unwrap_or_default() * 100.0;
    let success = scores["autonomous_success_score"]
        .as_f64()
        .unwrap_or_default()
        * 100.0;
    let routing = scores["routing_adaptation_score"]
        .as_f64()
        .unwrap_or_default()
        * 100.0;
    let memory = scores["memory_reuse_score"].as_f64().unwrap_or_default() * 100.0;
    let mut lines = vec![
        "Autonomous evaluation".to_string(),
        format!("  Total score       {total:.0}%"),
        format!("  Autonomous success {success:.0}%"),
        format!("  Routing adaptation {routing:.0}%"),
        format!("  Memory reuse       {memory:.0}%"),
        format!(
            "  Tasks             {} complete / {} total",
            counters["completed_tasks"].as_u64().unwrap_or(0),
            counters["tasks"].as_u64().unwrap_or(0)
        ),
        format!(
            "  Runs              {}",
            counters["autonomous_runs"].as_u64().unwrap_or(0)
        ),
        format!(
            "  Route feedback    {}",
            counters["route_feedback_entries"].as_u64().unwrap_or(0)
        ),
    ];
    if let Some(path) = value["runs_path"].as_str() {
        lines.push(format!("  Runs path         {path}"));
    }
    if let Some(recommendations) = report["recommendations"].as_array() {
        lines.push("Recommendations:".to_string());
        for recommendation in recommendations.iter().filter_map(Value::as_str) {
            lines.push(format!("  - {recommendation}"));
        }
    }
    lines.join("\n")
}

pub(crate) fn render_autonomous_integration_text(value: &Value) -> String {
    let report = value.get("integration").unwrap_or(value);
    let health = value.get("health");
    let summary = &report["summary"];
    let status = health
        .and_then(|health| health["status"].as_str())
        .or_else(|| report["status"].as_str())
        .unwrap_or("unknown");
    let failed = report["invariants"]
        .as_array()
        .map(|invariants| {
            invariants
                .iter()
                .filter(|invariant| invariant["status"].as_str() == Some("failed"))
                .count()
        })
        .unwrap_or(0);
    let warnings = report["invariants"]
        .as_array()
        .map(|invariants| {
            invariants
                .iter()
                .filter(|invariant| invariant["status"].as_str() == Some("warning"))
                .count()
        })
        .unwrap_or(0);
    let mut lines = vec!["Autonomous integration".to_string()];
    if let Some(health) = health {
        lines.push(format!("  Status            {status}"));
        if let Some(headline) = health["headline"].as_str() {
            lines.push(format!("  Summary           {headline}"));
        }
        if let Some(next_action) = health["next_action"].as_str() {
            lines.push(format!("  Next action       {next_action}"));
        }
        lines.push(format!(
            "  Safety            iterate={} apply_policy={}",
            health["safe_to_iterate"].as_bool().unwrap_or(false),
            health["safe_to_apply_policy"].as_bool().unwrap_or(false)
        ));
        if let Some(blockers) = health["blockers"].as_array() {
            for blocker in blockers.iter().filter_map(Value::as_str).take(3) {
                lines.push(format!("  Blocker           {blocker}"));
            }
        }
    } else {
        lines.push(format!("  Status            {status}"));
    }
    lines.extend([
        format!(
            "  Tasks             {} total / {} runnable / {} blocked",
            summary["task_count"].as_u64().unwrap_or(0),
            summary["runnable_task_count"].as_u64().unwrap_or(0),
            summary["blocked_task_count"].as_u64().unwrap_or(0)
        ),
        format!(
            "  Scheduler         {}, {} tick(s)",
            summary["scheduler_status"].as_str().unwrap_or("unknown"),
            summary["scheduler_tick_count"].as_u64().unwrap_or(0)
        ),
        format!(
            "  Workers           {} total / {} active / {} blocked",
            summary["worker_count"].as_u64().unwrap_or(0),
            summary["active_worker_count"].as_u64().unwrap_or(0),
            summary["blocked_worker_count"].as_u64().unwrap_or(0)
        ),
        format!(
            "  Memory/routes     {} memory / {} route feedback",
            summary["task_memory_entries"].as_u64().unwrap_or(0),
            summary["route_feedback_entries"].as_u64().unwrap_or(0)
        ),
        format!(
            "  Policy replay     {} lifecycle(s), {} anomalie(s)",
            summary["policy_lifecycle_count"].as_u64().unwrap_or(0),
            summary["policy_anomaly_count"].as_u64().unwrap_or(0)
        ),
        format!("  Invariants        {failed} failed / {warnings} warning(s)"),
    ]);
    if let Some(stages) = report["replay"]["stages"].as_array() {
        lines.push("Replay stages:".to_string());
        for stage in stages.iter().take(8) {
            let name = stage["name"].as_str().unwrap_or("stage");
            let status = stage["status"].as_str().unwrap_or("unknown");
            lines.push(format!("  - {name}: {status}"));
        }
    }
    if let Some(recommendations) = report["recommendations"].as_array() {
        lines.push("Recommendations:".to_string());
        for recommendation in recommendations.iter().filter_map(Value::as_str).take(5) {
            lines.push(format!("  - {recommendation}"));
        }
    }
    lines.extend(render_autonomous_guidance_lines(health, status));
    lines.join("\n")
}

pub(crate) fn render_autonomous_daemon_report_text(
    review: &runtime::AutonomousPolicyReview,
    runs_path: &Path,
) -> String {
    let mut lines = vec![format!(
        "Daemon report\n  Runs             {}\n  Blocked rate     {:.0}%\n  Consecutive block {}\n  Policy           {}\n  Max ticks        {} -> {}\n  Runs path        {}",
        review.summary.considered_runs,
        review.summary.blocked_rate * 100.0,
        review.summary.consecutive_blocked_runs,
        review.recommendation.action_label(),
        review.recommendation.requested_max_ticks,
        review.recommendation.recommended_max_ticks,
        runs_path.display()
    )];
    for reason in &review.recommendation.reasons {
        lines.push(format!("  Reason           {reason}"));
    }
    lines.join("\n")
}

pub(crate) fn render_autonomous_health_checkpoint_text(health: &Value) -> String {
    let status = health["status"].as_str().unwrap_or("unknown");
    let next_action = health["next_action"]
        .as_str()
        .unwrap_or("Run daemon report for full diagnostics.");
    let mut lines = vec![
        "Health checkpoint".to_string(),
        format!("  Status            {status}"),
    ];
    if let Some(headline) = health["headline"].as_str() {
        lines.push(format!("  Summary           {headline}"));
    }
    lines.push(format!("  Next action       {next_action}"));
    lines.push(format!(
        "  Safety            iterate={} apply_policy={}",
        health["safe_to_iterate"].as_bool().unwrap_or(false),
        health["safe_to_apply_policy"].as_bool().unwrap_or(false)
    ));
    if let Some(blockers) = health["blockers"].as_array() {
        for blocker in blockers.iter().filter_map(Value::as_str).take(3) {
            lines.push(format!("  Blocker           {blocker}"));
        }
    }
    if let Some(warnings) = health["warnings"].as_array() {
        for warning in warnings.iter().filter_map(Value::as_str).take(3) {
            lines.push(format!("  Warning           {warning}"));
        }
    }
    lines.extend(render_autonomous_guidance_lines(Some(health), status));
    lines.join("\n")
}

fn render_autonomous_guidance_lines(health: Option<&Value>, fallback_status: &str) -> Vec<String> {
    let status = health
        .and_then(|health| health["status"].as_str())
        .unwrap_or(fallback_status);
    let safe_to_iterate = health
        .and_then(|health| health["safe_to_iterate"].as_bool())
        .unwrap_or(status == "healthy" || status == "degraded");
    let safe_to_apply_policy = health
        .and_then(|health| health["safe_to_apply_policy"].as_bool())
        .unwrap_or(status == "healthy");
    let mut lines = vec!["Guidance:".to_string()];
    match status {
        "blocked" => {
            lines.push(
                "  - Resolve listed blockers before daemon start, scheduler run, or policy apply."
                    .to_string(),
            );
            lines.push(
                "  - Re-run `Himalaya tasks daemon report --limit 20 --max-ticks 3` after fixing state."
                    .to_string(),
            );
        }
        "degraded" => {
            lines.push(
                "  - Prefer report, evaluate, and replay until missing evidence is collected."
                    .to_string(),
            );
            if safe_to_iterate {
                lines.push(
                    "  - Use a bounded run such as `Himalaya tasks daemon start --max-ticks 1`."
                        .to_string(),
                );
            }
        }
        "healthy" => {
            lines.push(
                "  - Continue with a bounded daemon run or governed policy dry-run.".to_string(),
            );
        }
        _ => {
            lines.push(
                "  - Re-run daemon report with JSON output for full diagnostics.".to_string(),
            );
        }
    }
    if !safe_to_apply_policy {
        lines.push("  - Keep policy apply in dry-run mode until health is healthy.".to_string());
    }
    lines
}

pub(crate) fn render_autonomous_replay_text(value: &Value) -> String {
    let replay = value
        .get("trace_replay")
        .or_else(|| value.get("replay"))
        .unwrap_or(value);
    let considered = replay["considered_runs"].as_u64().unwrap_or(0);
    let changed = replay["changed_decisions"].as_u64().unwrap_or(0);
    let action = replay["policy_recommendation"]["action"]
        .as_str()
        .unwrap_or("continue");
    let recommended_ticks = replay["policy_recommendation"]["recommended_max_ticks"]
        .as_u64()
        .unwrap_or(1);
    let mut lines = vec![
        "Autonomous trace replay".to_string(),
        format!("  Runs              {considered}"),
        format!("  Changed decisions {changed}"),
        format!("  Current policy    {action} ({recommended_ticks} tick(s))"),
        format!(
            "  Next action       {}",
            if changed > 0 {
                "Review changed decisions before policy apply"
            } else {
                "Keep current autonomous policy"
            }
        ),
    ];
    if let Some(decisions) = replay["decisions"].as_array() {
        lines.push("Decisions:".to_string());
        for decision in decisions.iter().take(10) {
            let run_id = decision["run_id"].as_str().unwrap_or("run");
            let observed = decision["observed_status"].as_str().unwrap_or("unknown");
            let replay_action = decision["replay_action"].as_str().unwrap_or("continue");
            let marker = if decision["changed"].as_bool().unwrap_or(false) {
                "!"
            } else {
                " "
            };
            lines.push(format!(
                "  {marker} {run_id}: observed {observed}, replay {replay_action}"
            ));
        }
    }
    if let Some(recommendations) = replay["recommendations"].as_array() {
        lines.push("Recommendations:".to_string());
        for recommendation in recommendations.iter().filter_map(Value::as_str) {
            lines.push(format!("  - {recommendation}"));
        }
    }
    lines.join("\n")
}

pub(crate) fn render_autonomous_benchmark_text(value: &Value) -> String {
    let mut text = render_autonomous_evaluation_text(&value["run"]);
    if value["route_optimizer"].is_object() {
        let candidates = value["route_optimizer"]["report"]["candidates"]
            .as_array()
            .map_or(0, Vec::len);
        let changed = value["route_optimizer"]["replay"]["changed_routes"]
            .as_array()
            .map_or(0, Vec::len);
        text.push_str(&format!(
            "\nRoute optimizer: {candidates} candidate(s), {changed} replay change(s)"
        ));
    }
    if let Some(record_path) = value["record_path"].as_str() {
        text.push_str(&format!("\nRecord: {record_path}"));
    }
    text
}
