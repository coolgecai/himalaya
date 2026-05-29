use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    build_plan_dag, DecisioningEngine, ExecutionMode, MoERoutingPolicy, ModelRouteDecision,
    ModelRoutePhase, ModelRouter, PlanExecution, ReasoningContext, SafetyOutcome, SafetyPolicy,
    Task, TaskPlanner, Tool, ToolSelector,
};

pub const COMPLEX_CODING_BENCHMARK_SUITE_ID: &str = "complex-coding-agent-v1";
pub const COMPLEX_CODING_BENCHMARK_VERSION: &str = "2026.05";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkTaskSpec {
    pub id: String,
    pub title: String,
    pub objective: String,
    pub scope: String,
    pub category: String,
    pub complexity: u8,
    pub expected_capabilities: Vec<String>,
    pub constraints: Vec<String>,
    pub acceptance_tests: Vec<String>,
    pub evaluation_focus: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkSuite {
    pub suite_id: String,
    pub version: String,
    pub description: String,
    pub tasks: Vec<BenchmarkTaskSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkScore {
    pub capability_coverage: f32,
    pub decomposition_score: f32,
    pub scheduler_score: f32,
    pub moe_route_score: f32,
    pub total: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkTaskResult {
    pub task_id: String,
    pub title: String,
    pub selected_tools: Vec<String>,
    pub plan_steps: usize,
    pub parallelizable_steps: usize,
    pub execution_mode: String,
    pub risk_score: f32,
    pub risk_level: String,
    pub scheduler_ready_nodes: usize,
    pub scheduler_total_nodes: usize,
    pub route_decisions: Vec<ModelRouteDecision>,
    pub score: BenchmarkScore,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkSummary {
    pub total_tasks: usize,
    pub average_total_score: f32,
    pub average_capability_coverage: f32,
    pub parallel_plans: usize,
    pub review_or_deny_tasks: usize,
    pub total_plan_steps: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkRun {
    pub suite_id: String,
    pub version: String,
    pub model: String,
    pub max_parallelism: usize,
    pub summary: BenchmarkSummary,
    pub results: Vec<BenchmarkTaskResult>,
}

#[must_use]
pub fn complex_coding_benchmark_suite() -> BenchmarkSuite {
    BenchmarkSuite {
        suite_id: COMPLEX_CODING_BENCHMARK_SUITE_ID.to_string(),
        version: COMPLEX_CODING_BENCHMARK_VERSION.to_string(),
        description: "Ten complex coding tasks for evaluating planner decomposition, durable scheduling, and MoE routing without calling a model API.".to_string(),
        tasks: vec![
            benchmark_task(
                "stream-contract-drift",
                "Stream protocol drift across CLI and VS Code",
                "Add a new runtime stream-json event and wire it through Rust contract tests, TypeScript protocol validation, and the VS Code webview renderer.",
                "runtime stream-json, rusty-Himalaya-cli tests, vscode-extension stream protocol",
                "protocol",
                5,
                &["read", "search", "edit", "test", "shell"],
                &["protocol_version must remain stable", "no network access", "preserve backwards-compatible schema validation"],
                &["cargo test --manifest-path rust/Cargo.toml -p rusty-Himalaya-cli stream_json", "npm test --prefix vscode-extension"],
                &["cross-language schema coverage", "contract-test completeness", "minimal UI rendering change"],
            ),
            benchmark_task(
                "durable-resume-crash",
                "Durable task resume after process interruption",
                "Persist task plan execution state, restart the CLI, and resume from the first unfinished node with ledger evidence.",
                "runtime task registry, execution scheduler, CLI tasks commands",
                "durable-task",
                5,
                &["read", "search", "edit", "write", "test", "agent"],
                &["must not discard existing task ledger", "resume cursor must be deterministic"],
                &["cargo test --manifest-path rust/Cargo.toml -p runtime execution_scheduler", "cargo test --manifest-path rust/Cargo.toml -p rusty-Himalaya-cli task"],
                &["checkpoint fidelity", "scheduler queue correctness", "append-only evidence"],
            ),
            benchmark_task(
                "mcp-degraded-lifecycle",
                "MCP lifecycle degraded-mode recovery",
                "Detect an MCP startup failure, surface degraded mode, retry the handshake, and keep non-MCP tools available.",
                "runtime MCP lifecycle, MCP stdio client, recovery recipes",
                "mcp",
                4,
                &["read", "search", "edit", "test", "mcp", "permission"],
                &["do not mask failed MCP servers", "safe tools remain usable"],
                &["cargo test --manifest-path rust/Cargo.toml -p runtime mcp"],
                &["failure classification", "recovery action risk", "degraded capability reporting"],
            ),
            benchmark_task(
                "vscode-recovery-evidence",
                "VS Code task board recovery evidence replay",
                "Render task packet, plan execution, scheduler, and recovery events in the VS Code task board and replay them from chat history.",
                "vscode-extension chat panel, history, stream protocol",
                "vscode-ui",
                4,
                &["read", "search", "edit", "test"],
                &["sanitize all rendered event content", "history replay must not duplicate task cards"],
                &["npm test --prefix vscode-extension"],
                &["event aggregation", "safe rendering", "history replay behavior"],
            ),
            benchmark_task(
                "route-feedback-regression",
                "MoE route feedback regression loop",
                "Record failed verification feedback, persist it, and lower confidence or expose fallback routing on the next verification route decision.",
                "runtime model router, route feedback store, stream-json contract",
                "moe-routing",
                5,
                &["read", "search", "edit", "write", "test", "model"],
                &["feedback replay must deduplicate identical entries", "route choice must remain explainable"],
                &["cargo test --manifest-path rust/Cargo.toml -p runtime route_feedback_store", "cargo test --manifest-path rust/Cargo.toml -p rusty-Himalaya-cli stream_json_packet_verification_failure_emits_recovery_event"],
                &["feedback persistence", "route confidence adjustment", "recovery evidence"],
            ),
            benchmark_task(
                "worker-trust-recovery",
                "Worker supervisor trust-gate recovery",
                "Track a reusable worker from spawning through trust-required, resolution, prompt delivery, restart, and supervisor tick reporting.",
                "runtime worker boot, worker supervisor, CLI workers commands",
                "worker-supervision",
                4,
                &["read", "search", "edit", "write", "test", "agent"],
                &["trust gate must not auto-approve outside allowlisted roots", "worker state must persist across CLI invocations"],
                &["cargo test --manifest-path rust/Cargo.toml -p runtime worker", "cargo test --manifest-path rust/Cargo.toml -p rusty-Himalaya-cli worker_supervisor_commands_persist_worker_state"],
                &["state-machine transitions", "supervisor blocked/running status", "persistent worker registry"],
            ),
            benchmark_task(
                "long-context-compaction",
                "Long-context session compaction handoff",
                "Compact a long session while preserving task state, file references, recovery evidence, and the next actionable prompt.",
                "runtime summary compression, sessions, task registry",
                "long-running-context",
                5,
                &["read", "search", "edit", "test", "memory"],
                &["do not lose unresolved blockers", "handoff must include verification state"],
                &["cargo test --manifest-path rust/Cargo.toml -p runtime compact"],
                &["context retention", "task continuity", "resume prompt quality"],
            ),
            benchmark_task(
                "permission-policy-hardening",
                "Permission policy hardening for risky tool calls",
                "Add policy checks that distinguish read-only, workspace-write, and danger-full-access operations across CLI and VS Code flows.",
                "runtime permission enforcer, CLI permission prompts, VS Code confirmation gate",
                "safety",
                4,
                &["read", "search", "edit", "test", "permission"],
                &["never bypass explicit user denial", "danger-full-access requires visible confirmation"],
                &["cargo test --manifest-path rust/Cargo.toml -p runtime permission", "npm test --prefix vscode-extension"],
                &["risk classification", "denial recovery suggestion", "UI confirmation consistency"],
            ),
            benchmark_task(
                "cross-crate-cli-contract",
                "Cross-crate CLI output contract expansion",
                "Add a new local CLI command with text, JSON, and stream-json outputs, including Rust integration tests and TypeScript schema validation.",
                "rusty-Himalaya-cli, runtime public exports, vscode-extension stream protocol",
                "cli-contract",
                3,
                &["read", "search", "edit", "test", "shell"],
                &["JSON output must be machine-readable", "stream-json must include protocol_version"],
                &["cargo test --manifest-path rust/Cargo.toml -p rusty-Himalaya-cli output_format", "npm test --prefix vscode-extension"],
                &["output-format parity", "public runtime API shape", "schema validation"],
            ),
            benchmark_task(
                "parallel-agent-verification",
                "Parallel agent verification orchestration",
                "Split a feature into planner, implementer, verifier, and reviewer lanes, then aggregate verification results into one task ledger.",
                "team coordinator, task execution engine, durable scheduler, model router",
                "multi-agent-orchestration",
                5,
                &["read", "search", "edit", "test", "agent", "model"],
                &["parallel work must respect dependencies", "failed verification triggers recovery before completion"],
                &["cargo test --manifest-path rust/Cargo.toml -p runtime team", "cargo test --manifest-path rust/Cargo.toml -p runtime execution_scheduler"],
                &["role assignment", "dependency-aware parallelism", "verification aggregation"],
            ),
        ],
    }
}

#[must_use]
pub fn run_complex_coding_benchmark(default_model: &str, max_parallelism: usize) -> BenchmarkRun {
    run_benchmark_suite(
        complex_coding_benchmark_suite(),
        default_model,
        max_parallelism,
    )
}

#[must_use]
pub fn run_benchmark_suite(
    suite: BenchmarkSuite,
    default_model: &str,
    max_parallelism: usize,
) -> BenchmarkRun {
    let max_parallelism = max_parallelism.max(1);
    let tools = benchmark_tools();
    let router = ModelRouter::new(MoERoutingPolicy::balanced(default_model));
    let results = suite
        .tasks
        .iter()
        .map(|spec| run_benchmark_task(spec, &tools, &router, max_parallelism))
        .collect::<Vec<_>>();
    let summary = summarize_results(&results);
    BenchmarkRun {
        suite_id: suite.suite_id,
        version: suite.version,
        model: default_model.to_string(),
        max_parallelism,
        summary,
        results,
    }
}

fn run_benchmark_task(
    spec: &BenchmarkTaskSpec,
    tools: &[Tool],
    router: &ModelRouter,
    max_parallelism: usize,
) -> BenchmarkTaskResult {
    let task = Task::new(
        spec.id.clone(),
        spec.objective.clone(),
        spec.complexity,
        spec.expected_capabilities.clone(),
        benchmark_constraints(spec),
    );
    let context = ReasoningContext {
        active_constraints: task.constraints.clone(),
        max_parallelism,
        ..ReasoningContext::default()
    };
    let engine = DecisioningEngine::new(
        ToolSelector::new(tools.to_vec(), context),
        TaskPlanner::new(max_parallelism),
        SafetyPolicy::default(),
    );
    let snapshot = engine.analyze(&task);
    let dag = build_plan_dag(&snapshot.task, &snapshot.plan, &snapshot.selected_tools);
    let execution = PlanExecution::new(&dag);
    let route_decisions = benchmark_route_phases(spec)
        .into_iter()
        .map(|phase| router.select(phase))
        .collect::<Vec<_>>();
    let selected_tools = snapshot
        .selected_tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    let parallelizable_steps = snapshot
        .plan
        .steps
        .iter()
        .filter(|step| step.parallelizable)
        .count();
    let score = score_benchmark_task(
        spec,
        &snapshot.selected_tools,
        snapshot.plan.steps.len(),
        execution.ready_nodes().len(),
        execution.nodes.len(),
        route_decisions.len(),
    );

    BenchmarkTaskResult {
        task_id: spec.id.clone(),
        title: spec.title.clone(),
        selected_tools,
        plan_steps: snapshot.plan.steps.len(),
        parallelizable_steps,
        execution_mode: execution_mode_label(&snapshot.plan.execution_mode).to_string(),
        risk_score: snapshot.risk.score,
        risk_level: risk_level_label(&snapshot.risk.outcome).to_string(),
        scheduler_ready_nodes: execution.ready_nodes().len(),
        scheduler_total_nodes: execution.nodes.len(),
        route_decisions,
        score,
    }
}

fn benchmark_task(
    id: &str,
    title: &str,
    objective: &str,
    scope: &str,
    category: &str,
    complexity: u8,
    expected_capabilities: &[&str],
    constraints: &[&str],
    acceptance_tests: &[&str],
    evaluation_focus: &[&str],
) -> BenchmarkTaskSpec {
    BenchmarkTaskSpec {
        id: id.to_string(),
        title: title.to_string(),
        objective: objective.to_string(),
        scope: scope.to_string(),
        category: category.to_string(),
        complexity,
        expected_capabilities: strings(expected_capabilities),
        constraints: strings(constraints),
        acceptance_tests: strings(acceptance_tests),
        evaluation_focus: strings(evaluation_focus),
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn benchmark_constraints(spec: &BenchmarkTaskSpec) -> Vec<String> {
    let mut constraints = spec.constraints.clone();
    constraints.push("permission-mode:workspace-write".to_string());
    constraints.push(format!("scope:{}", spec.scope));
    constraints
}

fn benchmark_tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "read_file",
            strings(&["read", "file"]),
            0.96,
            60,
            0.05,
            true,
        ),
        Tool::new(
            "grep_search",
            strings(&["search", "read"]),
            0.94,
            70,
            0.05,
            true,
        ),
        Tool::new(
            "edit_file",
            strings(&["edit", "write", "file"]),
            0.86,
            110,
            0.12,
            false,
        ),
        Tool::new(
            "write_file",
            strings(&["write", "file"]),
            0.84,
            120,
            0.12,
            false,
        ),
        Tool::new(
            "bash",
            strings(&["shell", "test", "git"]),
            0.78,
            240,
            0.30,
            false,
        ),
        Tool::new(
            "verification_runner",
            strings(&["test", "verification", "shell"]),
            0.88,
            180,
            0.22,
            false,
        ),
        Tool::new(
            "task_scheduler",
            strings(&["agent", "task", "scheduler"]),
            0.82,
            210,
            0.25,
            true,
        ),
        Tool::new(
            "worker_supervisor",
            strings(&["agent", "worker", "task"]),
            0.80,
            220,
            0.25,
            true,
        ),
        Tool::new(
            "model_router",
            strings(&["model", "reasoning"]),
            0.84,
            160,
            0.20,
            true,
        ),
        Tool::new(
            "mcp_tool_bridge",
            strings(&["mcp", "tool"]),
            0.76,
            300,
            0.30,
            true,
        ),
        Tool::new(
            "permission_enforcer",
            strings(&["permission", "auth"]),
            0.90,
            100,
            0.12,
            true,
        ),
        Tool::new(
            "summary_compression",
            strings(&["memory", "summarization"]),
            0.83,
            150,
            0.18,
            true,
        ),
    ]
}

fn benchmark_route_phases(spec: &BenchmarkTaskSpec) -> Vec<ModelRoutePhase> {
    let mut phases = vec![ModelRoutePhase::Planning];
    if has_any(
        &spec.expected_capabilities,
        &["edit", "write", "shell", "agent"],
    ) {
        phases.push(ModelRoutePhase::Coding);
    }
    if has_any(
        &spec.expected_capabilities,
        &["test", "verification", "permission"],
    ) {
        phases.push(ModelRoutePhase::Verification);
    }
    if has_any(&spec.expected_capabilities, &["memory", "summarization"]) {
        phases.push(ModelRoutePhase::Summarization);
    }
    phases.sort();
    phases.dedup();
    phases
}

fn has_any(values: &[String], needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|needle| values.iter().any(|value| value == needle))
}

fn score_benchmark_task(
    spec: &BenchmarkTaskSpec,
    selected_tools: &[Tool],
    plan_steps: usize,
    ready_nodes: usize,
    total_nodes: usize,
    route_count: usize,
) -> BenchmarkScore {
    let capability_coverage = capability_coverage(&spec.expected_capabilities, selected_tools);
    let expected_steps = usize::from(spec.complexity.clamp(2, 5));
    let decomposition_score = (plan_steps as f32 / expected_steps as f32).clamp(0.0, 1.0);
    let scheduler_score = if ready_nodes > 0 && total_nodes >= plan_steps.saturating_add(1) {
        1.0
    } else {
        0.0
    };
    let expected_routes = benchmark_route_phases(spec).len().max(1);
    let moe_route_score = (route_count as f32 / expected_routes as f32).clamp(0.0, 1.0);
    let total = capability_coverage * 0.35
        + decomposition_score * 0.25
        + scheduler_score * 0.20
        + moe_route_score * 0.20;
    BenchmarkScore {
        capability_coverage,
        decomposition_score,
        scheduler_score,
        moe_route_score,
        total,
    }
}

fn capability_coverage(expected: &[String], selected_tools: &[Tool]) -> f32 {
    if expected.is_empty() {
        return 1.0;
    }
    let covered = selected_tools
        .iter()
        .flat_map(|tool| tool.capabilities.iter())
        .collect::<BTreeSet<_>>();
    let matched = expected
        .iter()
        .filter(|capability| covered.contains(capability))
        .count();
    (matched as f32 / expected.len() as f32).clamp(0.0, 1.0)
}

fn summarize_results(results: &[BenchmarkTaskResult]) -> BenchmarkSummary {
    let total_tasks = results.len();
    let denominator = total_tasks.max(1) as f32;
    BenchmarkSummary {
        total_tasks,
        average_total_score: results.iter().map(|result| result.score.total).sum::<f32>()
            / denominator,
        average_capability_coverage: results
            .iter()
            .map(|result| result.score.capability_coverage)
            .sum::<f32>()
            / denominator,
        parallel_plans: results
            .iter()
            .filter(|result| result.execution_mode == "parallel")
            .count(),
        review_or_deny_tasks: results
            .iter()
            .filter(|result| result.risk_level != "low")
            .count(),
        total_plan_steps: results.iter().map(|result| result.plan_steps).sum(),
    }
}

fn execution_mode_label(mode: &ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Serial => "serial",
        ExecutionMode::Parallel { .. } => "parallel",
    }
}

fn risk_level_label(outcome: &SafetyOutcome) -> &'static str {
    match outcome {
        SafetyOutcome::Allow => "low",
        SafetyOutcome::Review => "medium",
        SafetyOutcome::Deny => "high",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complex_coding_suite_contains_ten_tasks() {
        let suite = complex_coding_benchmark_suite();

        assert_eq!(suite.tasks.len(), 10);
        assert!(suite
            .tasks
            .iter()
            .all(|task| !task.acceptance_tests.is_empty()));
        assert!(suite.tasks.iter().any(|task| task
            .expected_capabilities
            .iter()
            .any(|capability| capability == "model")));
        assert!(suite.tasks.iter().any(|task| task
            .expected_capabilities
            .iter()
            .any(|capability| capability == "agent")));
    }

    #[test]
    fn benchmark_run_scores_planner_scheduler_and_routes() {
        let run = run_complex_coding_benchmark("sonnet", 4);

        assert_eq!(run.summary.total_tasks, 10);
        assert_eq!(run.results.len(), 10);
        assert!(run.summary.average_total_score > 0.50);
        assert!(run.summary.total_plan_steps >= 30);
        assert!(run
            .results
            .iter()
            .all(|result| result.scheduler_total_nodes > 0));
        assert!(run
            .results
            .iter()
            .all(|result| !result.route_decisions.is_empty()));
    }
}
