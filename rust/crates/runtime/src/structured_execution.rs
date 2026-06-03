//! Structured task execution: decompose a complex task into a DAG, dispatch
//! its nodes, and verify each node — turning the decisioning plan from an
//! advisory artifact into the backbone that actually drives execution.
//!
//! This module is built in stages and is entirely opt-in (gated by
//! `DecisioningConfig::structured_execution_threshold`). Stage 0 establishes
//! the types and the feasibility decision; later stages fill in model-driven
//! planning (Stage 1), node dispatch (Stage 2), per-node verification and
//! local re-drive (Stage 3), and per-node difficulty-aware routing (Stage 4).
//!
//! The design keeps the heavy `ConversationRuntime` integration thin: this
//! module owns the *pure, testable* structured-plan logic, and the runtime
//! calls into it. Nothing here changes behavior unless the gate is on.

use crate::decisioning::{ExecutionMode, Subtask, TaskPlan};
use crate::{PlanDag, PlanDagEdge, PlanDagEdgeKind, PlanDagNode, PlanNodeKind};
use serde::Deserialize;

/// Why a turn did or did not take the structured execution path. Recorded so
/// callers (and tests) can assert the gating decision without reaching into
/// private state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructuredFeasibility {
    /// The gate is on and the plan is structured enough to execute as a DAG.
    Eligible {
        node_count: usize,
        has_parallel_branch: bool,
    },
    /// Structured execution is disabled by config (threshold 0 or below).
    Disabled,
    /// The task complexity is below the configured threshold.
    BelowThreshold { complexity: u8, threshold: u8 },
    /// The plan is too trivial to benefit from DAG execution (single step).
    PlanTooSimple { node_count: usize },
}

impl StructuredFeasibility {
    /// Whether the turn should run the structured execution path.
    #[must_use]
    pub fn is_eligible(&self) -> bool {
        matches!(self, StructuredFeasibility::Eligible { .. })
    }

    /// A short human-readable reason, for events and tracing.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            StructuredFeasibility::Eligible {
                node_count,
                has_parallel_branch,
            } => format!(
                "structured execution eligible: {node_count} nodes, parallel={has_parallel_branch}"
            ),
            StructuredFeasibility::Disabled => {
                "structured execution disabled by config".to_string()
            }
            StructuredFeasibility::BelowThreshold {
                complexity,
                threshold,
            } => format!("task complexity {complexity} below threshold {threshold}"),
            StructuredFeasibility::PlanTooSimple { node_count } => {
                format!("plan has only {node_count} node(s); single-shot is sufficient")
            }
        }
    }
}

/// Decide whether a task should run the structured execution path.
///
/// Pure function of the gating inputs and the produced plan, so the decision
/// is deterministic and unit-testable independent of the runtime. `enabled`
/// and `threshold` come from `DecisioningConfig`; `complexity` is the task's
/// estimated complexity; `plan` is the (heuristic or model-derived) plan.
#[must_use]
pub fn assess_feasibility(
    enabled: bool,
    threshold: u8,
    complexity: u8,
    plan: &TaskPlan,
) -> StructuredFeasibility {
    if !enabled || threshold == 0 {
        return StructuredFeasibility::Disabled;
    }
    if complexity < threshold {
        return StructuredFeasibility::BelowThreshold {
            complexity,
            threshold,
        };
    }
    let node_count = plan.steps.len();
    if node_count < 2 {
        return StructuredFeasibility::PlanTooSimple { node_count };
    }
    let has_parallel_branch = plan.steps.iter().filter(|step| step.parallelizable).count() > 1;
    StructuredFeasibility::Eligible {
        node_count,
        has_parallel_branch,
    }
}

/// A node of work the structured executor will dispatch. Stage 2 turns these
/// into receipted sub-turns; Stage 3 verifies each one. Derived from the plan's
/// `Subtask`s plus the DAG edges so dependencies are explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionNode {
    pub id: String,
    pub title: String,
    pub depends_on: Vec<String>,
    pub parallelizable: bool,
    pub estimated_effort: u8,
    pub acceptance: Vec<String>,
}

impl ExecutionNode {
    #[must_use]
    pub fn from_subtask(step: &Subtask, depends_on: Vec<String>) -> Self {
        Self {
            id: step.id.clone(),
            title: step.title.clone(),
            depends_on,
            parallelizable: step.parallelizable,
            estimated_effort: step.estimated_effort,
            // Acceptance criteria are populated by Stage 1 model planning; the
            // heuristic plan leaves them empty (node verified by turn-level
            // verification rather than a per-node command).
            acceptance: Vec::new(),
        }
    }
}

/// Build the ordered execution-node list from a DAG, resolving each node's
/// direct dependencies from the `DependsOn` edges. The order is the DAG node
/// order; the scheduler decides actual dispatch order from readiness.
#[must_use]
pub fn execution_nodes_from_dag(dag: &PlanDag, plan: &TaskPlan) -> Vec<ExecutionNode> {
    use crate::PlanDagEdgeKind;
    let mut nodes = Vec::new();
    for step in &plan.steps {
        let depends_on = dag
            .edges
            .iter()
            .filter(|edge| edge.kind == PlanDagEdgeKind::DependsOn && edge.to == step.id)
            .map(|edge| edge.from.clone())
            .collect::<Vec<_>>();
        nodes.push(ExecutionNode::from_subtask(step, depends_on));
    }
    nodes
}

/// One step as returned by the planning model's structured JSON output.
#[derive(Debug, Clone, Deserialize)]
pub struct StructuredPlanStep {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub parallelizable: bool,
    #[serde(default)]
    pub estimated_effort: Option<u8>,
    #[serde(default)]
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// The planning model's full structured plan (the JSON shape it is asked to
/// return). Parsed defensively: any structural problem yields `None` so the
/// caller falls back to the heuristic decomposition.
#[derive(Debug, Clone, Deserialize)]
pub struct StructuredPlan {
    pub steps: Vec<StructuredPlanStep>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// A validated structured plan: the `TaskPlan` to record/execute, the explicit
/// dependency edges, and the per-node acceptance criteria for Stage 3.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedStructuredPlan {
    pub plan: TaskPlan,
    pub dependencies: Vec<(String, String)>,
    pub acceptance: Vec<(String, Vec<String>)>,
}

/// Parse and validate a model-produced structured plan JSON string into a
/// `TaskPlan` plus explicit dependency edges. Returns `None` (→ heuristic
/// fallback) when the JSON is malformed, empty, has duplicate/blank ids, or
/// references a dependency id that is not a declared step. This strict
/// validation keeps a hallucinated plan from corrupting the execution DAG.
#[must_use]
pub fn parse_structured_plan(task_id: &str, json: &str) -> Option<ValidatedStructuredPlan> {
    let parsed: StructuredPlan = serde_json::from_str(json.trim()).ok()?;
    if parsed.steps.is_empty() {
        return None;
    }
    let mut ids = std::collections::BTreeSet::new();
    for step in &parsed.steps {
        let id = step.id.trim();
        if id.is_empty() || step.title.trim().is_empty() {
            return None;
        }
        if !ids.insert(id.to_string()) {
            return None; // duplicate id
        }
    }
    // Every dependency must reference a declared step, and a step may not
    // depend on itself.
    let mut dependencies = Vec::new();
    for step in &parsed.steps {
        for dep in &step.depends_on {
            let dep = dep.trim();
            if dep == step.id.trim() || !ids.contains(dep) {
                return None;
            }
            dependencies.push((dep.to_string(), step.id.trim().to_string()));
        }
    }

    let steps = parsed
        .steps
        .iter()
        .map(|step| Subtask {
            id: step.id.trim().to_string(),
            title: step.title.trim().to_string(),
            required_capabilities: step.capabilities.clone(),
            candidate_tools: Vec::new(),
            parallelizable: step.parallelizable,
            estimated_effort: step.estimated_effort.unwrap_or(1).max(1),
            notes: Vec::new(),
        })
        .collect::<Vec<_>>();

    let has_parallel = steps.iter().filter(|s| s.parallelizable).count() > 1;
    let execution_mode = if has_parallel {
        ExecutionMode::Parallel {
            max_concurrency: steps.iter().filter(|s| s.parallelizable).count(),
        }
    } else {
        ExecutionMode::Serial
    };

    let acceptance = parsed
        .steps
        .iter()
        .filter(|step| !step.acceptance.is_empty())
        .map(|step| (step.id.trim().to_string(), step.acceptance.clone()))
        .collect::<Vec<_>>();

    let plan = TaskPlan {
        task_id: task_id.to_string(),
        steps,
        execution_mode,
        confidence: 0.8,
        notes: if parsed.notes.is_empty() {
            vec!["Model-derived structured plan.".to_string()]
        } else {
            parsed.notes.clone()
        },
    };

    Some(ValidatedStructuredPlan {
        plan,
        dependencies,
        acceptance,
    })
}

/// Build a `PlanDag` from a validated structured plan, honoring the explicit
/// dependency edges (unlike `build_plan_dag`, which infers serial/parallel
/// edges from the execution mode). A step with no explicit dependencies and no
/// incoming edge is treated as ready from the start (depends only on the root).
#[must_use]
pub fn build_structured_dag(
    task_id: &str,
    title: &str,
    validated: &ValidatedStructuredPlan,
) -> PlanDag {
    let root_id = task_id.to_string();
    let mut nodes = vec![PlanDagNode {
        kind: PlanNodeKind::Task,
        id: root_id.clone(),
        title: title.to_string(),
        parallelizable: matches!(
            validated.plan.execution_mode,
            ExecutionMode::Parallel { .. }
        ),
        estimated_effort: 1,
        candidate_tools: Vec::new(),
        notes: validated.plan.notes.clone(),
    }];
    let mut edges = Vec::new();
    for step in &validated.plan.steps {
        nodes.push(PlanDagNode {
            kind: PlanNodeKind::Step,
            id: step.id.clone(),
            title: step.title.clone(),
            parallelizable: step.parallelizable,
            estimated_effort: step.estimated_effort,
            candidate_tools: step.candidate_tools.clone(),
            notes: step.notes.clone(),
        });
        edges.push(PlanDagEdge {
            from: root_id.clone(),
            to: step.id.clone(),
            kind: PlanDagEdgeKind::Contains,
        });
    }
    for (from, to) in &validated.dependencies {
        edges.push(PlanDagEdge {
            from: from.clone(),
            to: to.clone(),
            kind: PlanDagEdgeKind::DependsOn,
        });
    }
    PlanDag {
        task_id: task_id.to_string(),
        root_id,
        nodes,
        edges,
    }
}

/// What a node executor reports back for a single node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeExecutionResult {
    pub succeeded: bool,
    /// Short human-readable outcome (success summary or failure class).
    pub summary: String,
}

impl NodeExecutionResult {
    #[must_use]
    pub fn success(summary: impl Into<String>) -> Self {
        Self {
            succeeded: true,
            summary: summary.into(),
        }
    }

    #[must_use]
    pub fn failure(summary: impl Into<String>) -> Self {
        Self {
            succeeded: false,
            summary: summary.into(),
        }
    }
}

/// Outcome of dispatching a whole structured plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOutcome {
    pub completed: Vec<String>,
    pub failed: Vec<String>,
    pub dispatched: usize,
}

impl DispatchOutcome {
    #[must_use]
    pub fn all_succeeded(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Drive a structured plan's DAG to completion in dependency order, calling
/// `execute_node(node)` for each ready node. The executor returns whether the
/// node succeeded; success unlocks dependents, failure skips them. This is the
/// deterministic scheduling core — the runtime supplies an `execute_node`
/// closure that runs a focused model sub-turn (Stage 2) and, later, verifies
/// the node (Stage 3). Kept pure so the topological logic is unit-testable
/// without a live model.
///
/// `max_nodes` bounds total dispatches as a runaway guard.
pub fn dispatch_plan<F>(
    dag: &PlanDag,
    execution: &mut crate::PlanExecution,
    max_nodes: usize,
    mut execute_node: F,
) -> DispatchOutcome
where
    F: FnMut(&ExecutionNode) -> NodeExecutionResult,
{
    let node_lookup = node_index(dag);
    let mut completed = Vec::new();
    let mut failed = Vec::new();
    let mut dispatched = 0usize;

    // The root/Task node is structural, not executable — succeed it up front so
    // its children can proceed. `succeed_node` only accepts Ready/Running and is
    // a no-op (ignored error) otherwise. Without this the root stays Ready
    // forever and the loop never finishes.
    let _ = execution.succeed_node(dag, &dag.root_id, Some("root".to_string()));

    while !execution.is_finished() && dispatched < max_nodes {
        let ready = execution
            .ready_nodes()
            .into_iter()
            .filter(|id| id != &dag.root_id)
            .collect::<Vec<_>>();
        if ready.is_empty() {
            break;
        }
        // Deterministic order: DAG node order.
        let mut ordered = ready;
        ordered.sort_by_key(|id| node_lookup.get(id).copied().unwrap_or(usize::MAX));

        for node_id in ordered {
            if dispatched >= max_nodes {
                break;
            }
            let Some(node) = build_execution_node(dag, &node_id) else {
                // Non-step node that is somehow ready — mark it done structurally
                // so it cannot wedge the loop.
                let _ = execution.succeed_node(dag, &node_id, Some("structural".to_string()));
                continue;
            };
            if execution.start_node(&node_id).is_err() {
                continue;
            }
            dispatched += 1;
            let result = execute_node(&node);
            if result.succeeded {
                let _ = execution.succeed_node(dag, &node_id, Some(result.summary));
                completed.push(node_id);
            } else {
                let _ = execution.fail_node(dag, &node_id, result.summary);
                failed.push(node_id);
            }
        }
    }

    DispatchOutcome {
        completed,
        failed,
        dispatched,
    }
}

fn node_index(dag: &PlanDag) -> std::collections::HashMap<String, usize> {
    dag.nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.clone(), index))
        .collect()
}

/// Reconstruct an `ExecutionNode` (with resolved dependencies) for a single DAG
/// step node by id. Returns `None` for the root/task node or unknown ids.
fn build_execution_node(dag: &PlanDag, node_id: &str) -> Option<ExecutionNode> {
    let node = dag.nodes.iter().find(|n| n.id == node_id)?;
    if node.kind != PlanNodeKind::Step {
        return None;
    }
    let depends_on = dag
        .edges
        .iter()
        .filter(|edge| edge.kind == PlanDagEdgeKind::DependsOn && edge.to == node_id)
        .map(|edge| edge.from.clone())
        .collect::<Vec<_>>();
    Some(ExecutionNode {
        id: node.id.clone(),
        title: node.title.clone(),
        depends_on,
        parallelizable: node.parallelizable,
        estimated_effort: node.estimated_effort,
        acceptance: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decisioning::{ExecutionMode, Subtask, TaskPlan};

    fn step(id: &str, parallelizable: bool) -> Subtask {
        Subtask {
            id: id.to_string(),
            title: format!("do {id}"),
            required_capabilities: Vec::new(),
            candidate_tools: Vec::new(),
            parallelizable,
            estimated_effort: 1,
            notes: Vec::new(),
        }
    }

    fn plan(steps: Vec<Subtask>) -> TaskPlan {
        TaskPlan {
            task_id: "t".to_string(),
            steps,
            execution_mode: ExecutionMode::Serial,
            confidence: 0.8,
            notes: Vec::new(),
        }
    }

    #[test]
    fn disabled_when_gate_off() {
        let p = plan(vec![step("a", false), step("b", false)]);
        assert_eq!(
            assess_feasibility(false, 3, 5, &p),
            StructuredFeasibility::Disabled
        );
        assert_eq!(
            assess_feasibility(true, 0, 5, &p),
            StructuredFeasibility::Disabled
        );
    }

    #[test]
    fn below_threshold_is_rejected() {
        let p = plan(vec![step("a", false), step("b", false)]);
        let feasibility = assess_feasibility(true, 4, 2, &p);
        assert_eq!(
            feasibility,
            StructuredFeasibility::BelowThreshold {
                complexity: 2,
                threshold: 4,
            }
        );
        assert!(!feasibility.is_eligible());
    }

    #[test]
    fn single_step_plan_is_too_simple() {
        let p = plan(vec![step("only", false)]);
        assert_eq!(
            assess_feasibility(true, 3, 5, &p),
            StructuredFeasibility::PlanTooSimple { node_count: 1 }
        );
    }

    #[test]
    fn eligible_when_gate_on_and_plan_structured() {
        let p = plan(vec![step("a", true), step("b", true), step("c", false)]);
        let feasibility = assess_feasibility(true, 3, 5, &p);
        assert!(feasibility.is_eligible());
        assert_eq!(
            feasibility,
            StructuredFeasibility::Eligible {
                node_count: 3,
                has_parallel_branch: true,
            }
        );
    }

    #[test]
    fn execution_nodes_resolve_dependencies_from_dag() {
        let task = crate::decisioning::Task::new(
            "t",
            "ship",
            4,
            vec!["a".to_string(), "b".to_string()],
            Vec::new(),
        );
        let p = plan(vec![
            step("t-analyze", false),
            step("t-a", true),
            step("t-b", true),
        ]);
        let dag = crate::build_plan_dag(&task, &p, &[]);
        let nodes = execution_nodes_from_dag(&dag, &p);
        assert_eq!(nodes.len(), 3);
        // The analyze node is first and has no DependsOn predecessor.
        let analyze = nodes.iter().find(|n| n.id == "t-analyze").expect("node");
        assert!(analyze.depends_on.is_empty());
    }

    #[test]
    fn parses_valid_structured_plan_with_dependencies() {
        let json = r#"{
            "steps": [
                {"id": "design", "title": "Design API", "parallelizable": false, "estimated_effort": 2, "acceptance": ["cargo build"]},
                {"id": "impl", "title": "Implement", "depends_on": ["design"], "parallelizable": true},
                {"id": "test", "title": "Add tests", "depends_on": ["impl"], "acceptance": ["cargo test"]}
            ],
            "notes": ["model plan"]
        }"#;
        let validated = parse_structured_plan("task-1", json).expect("valid plan");
        assert_eq!(validated.plan.steps.len(), 3);
        assert_eq!(validated.dependencies.len(), 2);
        assert!(validated
            .dependencies
            .contains(&("design".to_string(), "impl".to_string())));
        // Acceptance criteria are carried for nodes that declared them.
        assert_eq!(validated.acceptance.len(), 2);
    }

    #[test]
    fn rejects_plan_with_unknown_or_self_dependency() {
        let unknown = r#"{"steps":[{"id":"a","title":"A","depends_on":["ghost"]}]}"#;
        assert!(parse_structured_plan("t", unknown).is_none());
        let self_dep = r#"{"steps":[{"id":"a","title":"A","depends_on":["a"]}]}"#;
        assert!(parse_structured_plan("t", self_dep).is_none());
    }

    #[test]
    fn rejects_malformed_or_empty_or_duplicate_plan() {
        assert!(parse_structured_plan("t", "not json").is_none());
        assert!(parse_structured_plan("t", r#"{"steps":[]}"#).is_none());
        let dup = r#"{"steps":[{"id":"a","title":"A"},{"id":"a","title":"B"}]}"#;
        assert!(parse_structured_plan("t", dup).is_none());
        let blank = r#"{"steps":[{"id":"  ","title":"A"}]}"#;
        assert!(parse_structured_plan("t", blank).is_none());
    }

    #[test]
    fn builds_dag_honoring_explicit_dependencies() {
        let json = r#"{
            "steps": [
                {"id": "a", "title": "A"},
                {"id": "b", "title": "B", "depends_on": ["a"]}
            ]
        }"#;
        let validated = parse_structured_plan("task-9", json).expect("valid");
        let dag = build_structured_dag("task-9", "Ship it", &validated);
        // root + 2 step nodes
        assert_eq!(dag.nodes.len(), 3);
        // Contains edges from root to each step + one DependsOn edge a->b.
        let depends_on = dag
            .edges
            .iter()
            .filter(|e| e.kind == crate::PlanDagEdgeKind::DependsOn)
            .count();
        assert_eq!(depends_on, 1);
        assert!(dag
            .edges
            .iter()
            .any(|e| e.from == "a" && e.to == "b" && e.kind == crate::PlanDagEdgeKind::DependsOn));
    }

    fn validated_chain() -> ValidatedStructuredPlan {
        // a -> b -> c (strict chain)
        let json = r#"{"steps":[
            {"id":"a","title":"A"},
            {"id":"b","title":"B","depends_on":["a"]},
            {"id":"c","title":"C","depends_on":["b"]}
        ]}"#;
        parse_structured_plan("task", json).expect("valid chain")
    }

    #[test]
    fn dispatch_runs_nodes_in_dependency_order() {
        let validated = validated_chain();
        let dag = build_structured_dag("task", "Chain", &validated);
        let mut execution = crate::PlanExecution::new(&dag);
        let mut order = Vec::new();
        let outcome = dispatch_plan(&dag, &mut execution, 100, |node| {
            order.push(node.id.clone());
            NodeExecutionResult::success("ok")
        });
        assert!(outcome.all_succeeded());
        assert_eq!(order, vec!["a", "b", "c"]);
        assert_eq!(outcome.completed.len(), 3);
        assert!(execution.is_finished());
    }

    #[test]
    fn dispatch_skips_dependents_of_a_failed_node() {
        let validated = validated_chain();
        let dag = build_structured_dag("task", "Chain", &validated);
        let mut execution = crate::PlanExecution::new(&dag);
        let mut executed = Vec::new();
        let outcome = dispatch_plan(&dag, &mut execution, 100, |node| {
            executed.push(node.id.clone());
            if node.id == "a" {
                NodeExecutionResult::failure("boom")
            } else {
                NodeExecutionResult::success("ok")
            }
        });
        // Only 'a' runs; b and c are skipped because their dependency failed.
        assert_eq!(executed, vec!["a"]);
        assert!(!outcome.all_succeeded());
        assert_eq!(outcome.failed, vec!["a"]);
    }

    #[test]
    fn dispatch_honors_max_nodes_guard() {
        let validated = validated_chain();
        let dag = build_structured_dag("task", "Chain", &validated);
        let mut execution = crate::PlanExecution::new(&dag);
        let mut count = 0usize;
        let outcome = dispatch_plan(&dag, &mut execution, 1, |_node| {
            count += 1;
            NodeExecutionResult::success("ok")
        });
        assert_eq!(count, 1);
        assert_eq!(outcome.dispatched, 1);
        assert!(!execution.is_finished());
    }
}
