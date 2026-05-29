use serde::{Deserialize, Serialize};

use crate::{
    PlanNodeStatus, TaskRegistry, TaskStatus, TeamCoordinator, VerificationCommandResult,
    VerificationRequest, VerificationResult, VerificationRunner,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskExecutionStepKind {
    ResumeNode,
    RunNodeVerification,
    CompleteTask,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskExecutionStep {
    pub task_id: String,
    pub node_id: Option<String>,
    pub kind: TaskExecutionStepKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskExecutionOutcome {
    pub task_id: String,
    pub steps: Vec<TaskExecutionStep>,
    pub completed: bool,
    pub blocked: bool,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct TaskExecutionEngine {
    registry: TaskRegistry,
    verification_runner: VerificationRunner,
}

impl TaskExecutionEngine {
    #[must_use]
    pub fn new(registry: TaskRegistry, verification_runner: VerificationRunner) -> Self {
        Self {
            registry,
            verification_runner,
        }
    }

    pub fn execute(
        &self,
        task_id: &str,
        from_node: Option<&str>,
    ) -> Result<TaskExecutionOutcome, String> {
        let task = self.registry.resume(task_id).or_else(|_| {
            self.registry
                .get(task_id)
                .ok_or_else(|| format!("task not found: {task_id}"))
        })?;
        let Some(plan) = task.plan.clone() else {
            self.registry.set_status(task_id, TaskStatus::Blocked)?;
            return Ok(TaskExecutionOutcome {
                task_id: task_id.to_string(),
                steps: vec![TaskExecutionStep {
                    task_id: task_id.to_string(),
                    node_id: None,
                    kind: TaskExecutionStepKind::Blocked,
                    message: "task has no persisted plan".to_string(),
                }],
                completed: false,
                blocked: true,
                message: "task has no persisted plan".to_string(),
            });
        };

        let mut steps = Vec::new();
        let mut execution = plan.execution.clone();
        let mut next_nodes = from_node
            .map(|node| vec![node.to_string()])
            .unwrap_or_else(|| execution.resumable_nodes());

        while !next_nodes.is_empty() {
            let mut advanced = false;
            for node_id in std::mem::take(&mut next_nodes) {
                let Some(node) = execution.nodes.get(&node_id).cloned() else {
                    continue;
                };
                if node.status == PlanNodeStatus::Succeeded {
                    continue;
                }
                steps.push(TaskExecutionStep {
                    task_id: task_id.to_string(),
                    node_id: Some(node_id.clone()),
                    kind: TaskExecutionStepKind::ResumeNode,
                    message: format!("resuming node {node_id}"),
                });
                if matches!(
                    node.status,
                    PlanNodeStatus::Failed | PlanNodeStatus::Skipped | PlanNodeStatus::Running
                ) {
                    execution.retry_node(&plan.dag, &node_id)?;
                }
                execution.start_node(&node_id)?;
                advanced = true;
                if let Some(gate) = node.verification_gate {
                    steps.push(TaskExecutionStep {
                        task_id: task_id.to_string(),
                        node_id: Some(node_id.clone()),
                        kind: TaskExecutionStepKind::RunNodeVerification,
                        message: gate.command.clone(),
                    });
                    let result = self.run_node_verification(task_id, &node_id, &gate.command);
                    let passed = result.passed;
                    let summary = result.summary.clone();
                    execution.record_node_verification(&node_id, passed, summary.clone())?;
                    self.registry.record_verification(task_id, result)?;
                    if !passed {
                        if gate.required {
                            self.registry
                                .record_plan(task_id, plan.dag.clone(), execution)?;
                            self.registry.set_status(task_id, TaskStatus::Blocked)?;
                            return Ok(TaskExecutionOutcome {
                                task_id: task_id.to_string(),
                                steps,
                                completed: false,
                                blocked: true,
                                message: format!("node {node_id} verification failed"),
                            });
                        }
                        execution.retry_node(&plan.dag, &node_id)?;
                        execution.start_node(&node_id)?;
                    }
                    let output_summary = if passed {
                        format!("node verification passed: {summary}")
                    } else {
                        format!("optional node verification failed: {summary}")
                    };
                    execution.succeed_node(&plan.dag, &node_id, Some(output_summary))?;
                } else {
                    execution.succeed_node(
                        &plan.dag,
                        &node_id,
                        Some("node resumed".to_string()),
                    )?;
                }
            }
            if !advanced {
                break;
            }
            next_nodes = execution
                .resumable_nodes()
                .into_iter()
                .filter(|node_id| {
                    execution
                        .nodes
                        .get(node_id)
                        .is_some_and(|node| node.status != PlanNodeStatus::Succeeded)
                })
                .collect();
        }

        let completed = execution.is_finished()
            && execution
                .nodes
                .values()
                .all(|node| node.status == PlanNodeStatus::Succeeded);
        let status = if completed {
            TaskStatus::Completed
        } else {
            TaskStatus::Blocked
        };
        self.registry
            .record_plan(task_id, plan.dag, execution.clone())?;
        self.registry.set_status(task_id, status)?;
        if completed {
            steps.push(TaskExecutionStep {
                task_id: task_id.to_string(),
                node_id: None,
                kind: TaskExecutionStepKind::CompleteTask,
                message: "all plan nodes succeeded".to_string(),
            });
        }
        Ok(TaskExecutionOutcome {
            task_id: task_id.to_string(),
            steps,
            completed,
            blocked: !completed,
            message: if completed {
                "task execution completed".to_string()
            } else {
                "task execution blocked with remaining nodes".to_string()
            },
        })
    }

    pub fn assign_team(&self, task_id: &str) -> Result<Vec<crate::TeamExecutionEvent>, String> {
        let task = self
            .registry
            .get(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let Some(plan) = task.plan else {
            return Ok(Vec::new());
        };
        let mut coordinator = TeamCoordinator::new(format!("team-{task_id}"));
        let coordination = coordinator.plan(&plan.dag);
        let events = coordination
            .assignments
            .iter()
            .map(|assignment| coordinator.event_for_assignment(task_id, assignment, None))
            .collect::<Vec<_>>();
        for event in &events {
            let _ = self.registry.record_team_event(task_id, event.clone());
        }
        Ok(events)
    }

    fn run_node_verification(
        &self,
        task_id: &str,
        node_id: &str,
        command: &str,
    ) -> VerificationResult {
        let request = VerificationRequest {
            task_id: task_id.to_string(),
            objective: format!("verify node {node_id}"),
            scope: node_id.to_string(),
            acceptance_tests: vec![command.to_string()],
            reporting_contract: "node verification result".to_string(),
            policy: crate::VerificationPolicy::Targeted,
            required_green_level: Some(crate::green_contract::GreenLevel::TargetedTests),
        };
        self.verification_runner.run(&request)
    }
}

#[allow(dead_code)]
fn _keep_command_result_type(_: Option<VerificationCommandResult>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlanDag, PlanDagEdge, PlanDagEdgeKind, PlanDagNode, PlanExecution, PlanNodeKind};

    fn sample_registry() -> (TaskRegistry, String) {
        let registry = TaskRegistry::new();
        let task = registry.create("execute", Some("test"));
        let dag = PlanDag {
            task_id: task.task_id.clone(),
            root_id: task.task_id.clone(),
            nodes: vec![PlanDagNode {
                kind: PlanNodeKind::Step,
                id: "node-1".to_string(),
                title: "Node".to_string(),
                parallelizable: false,
                estimated_effort: 1,
                candidate_tools: Vec::new(),
                notes: Vec::new(),
            }],
            edges: Vec::<PlanDagEdge>::new(),
        };
        let execution = PlanExecution::new(&dag);
        registry
            .record_plan(&task.task_id, dag, execution)
            .expect("plan");
        (registry, task.task_id)
    }

    fn dependent_registry() -> (TaskRegistry, String) {
        let registry = TaskRegistry::new();
        let task = registry.create("execute", Some("test"));
        let dag = PlanDag {
            task_id: task.task_id.clone(),
            root_id: task.task_id.clone(),
            nodes: vec![
                PlanDagNode {
                    kind: PlanNodeKind::Step,
                    id: "analyze".to_string(),
                    title: "Analyze".to_string(),
                    parallelizable: false,
                    estimated_effort: 1,
                    candidate_tools: Vec::new(),
                    notes: Vec::new(),
                },
                PlanDagNode {
                    kind: PlanNodeKind::Step,
                    id: "edit".to_string(),
                    title: "Edit".to_string(),
                    parallelizable: false,
                    estimated_effort: 1,
                    candidate_tools: Vec::new(),
                    notes: Vec::new(),
                },
            ],
            edges: vec![PlanDagEdge {
                from: "analyze".to_string(),
                to: "edit".to_string(),
                kind: PlanDagEdgeKind::DependsOn,
            }],
        };
        let execution = PlanExecution::new(&dag);
        registry
            .record_plan(&task.task_id, dag, execution)
            .expect("plan");
        (registry, task.task_id)
    }

    #[test]
    fn executes_single_ready_node_to_completion() {
        let (registry, task_id) = sample_registry();
        let engine = TaskExecutionEngine::new(registry.clone(), VerificationRunner::new(None));
        let outcome = engine.execute(&task_id, None).expect("execute");

        assert!(outcome.completed);
        assert_eq!(
            registry.get(&task_id).expect("task").status,
            TaskStatus::Completed
        );
    }

    #[test]
    fn execute_continues_to_nodes_unblocked_by_resume() {
        let (registry, task_id) = dependent_registry();
        let engine = TaskExecutionEngine::new(registry.clone(), VerificationRunner::new(None));
        let outcome = engine.execute(&task_id, None).expect("execute");
        let task = registry.get(&task_id).expect("task");
        let plan = task.plan.expect("plan");

        assert!(outcome.completed);
        assert_eq!(
            plan.execution.nodes["analyze"].status,
            PlanNodeStatus::Succeeded
        );
        assert_eq!(
            plan.execution.nodes["edit"].status,
            PlanNodeStatus::Succeeded
        );
    }

    #[test]
    fn node_verification_failure_blocks_only_current_node() {
        let (registry, task_id) = dependent_registry();
        registry
            .attach_node_verification(&task_id, "analyze", "definitely-not-allowed", true)
            .expect("gate");
        let engine = TaskExecutionEngine::new(registry.clone(), VerificationRunner::new(None));
        let outcome = engine.execute(&task_id, None).expect("execute");
        let task = registry.get(&task_id).expect("task");
        let plan = task.plan.expect("plan");

        assert!(outcome.blocked);
        assert_eq!(task.status, TaskStatus::Blocked);
        assert_eq!(
            plan.execution.nodes["analyze"].status,
            PlanNodeStatus::Failed
        );
        assert_eq!(plan.execution.nodes["edit"].status, PlanNodeStatus::Pending);
    }
}
