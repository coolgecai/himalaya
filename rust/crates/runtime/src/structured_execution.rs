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

use crate::decisioning::{Subtask, TaskPlan};
use crate::PlanDag;

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
}
