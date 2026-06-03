use serde::{Deserialize, Serialize};

use crate::{
    PlanDag, PlanDagNode, PlanExecution, PlanNodeKind, PlanNodeStatus, TaskRegistry, TaskStatus,
    TeamCoordinator, VerificationCommandResult, VerificationRequest, VerificationResult,
    VerificationRunner, Worker, WorkerRegistry, WorkerStatus,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskExecutionStepKind {
    ResumeNode,
    DispatchWorker,
    AwaitWorker,
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
    worker_registry: Option<WorkerRegistry>,
}

impl TaskExecutionEngine {
    #[must_use]
    pub fn new(registry: TaskRegistry, verification_runner: VerificationRunner) -> Self {
        Self {
            registry,
            verification_runner,
            worker_registry: None,
        }
    }

    #[must_use]
    pub fn with_workers(
        registry: TaskRegistry,
        verification_runner: VerificationRunner,
        worker_registry: WorkerRegistry,
    ) -> Self {
        Self {
            registry,
            verification_runner,
            worker_registry: Some(worker_registry),
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

                if self.worker_registry.is_some()
                    && self.is_task_container_node(&plan.dag, &node_id)
                {
                    if matches!(
                        node.status,
                        PlanNodeStatus::Failed | PlanNodeStatus::Skipped
                    ) {
                        execution.retry_node(&plan.dag, &node_id)?;
                    }
                    if execution
                        .nodes
                        .get(&node_id)
                        .is_some_and(|node| node.status == PlanNodeStatus::Ready)
                    {
                        execution.start_node(&node_id)?;
                    }
                    if execution
                        .nodes
                        .get(&node_id)
                        .is_some_and(|node| node.status == PlanNodeStatus::Running)
                    {
                        execution.succeed_node(
                            &plan.dag,
                            &node_id,
                            Some("task container resumed".to_string()),
                        )?;
                        advanced = true;
                    }
                    continue;
                }

                if let Some(worker_registry) = self.worker_registry.as_ref() {
                    let node_advanced = self.advance_worker_node(
                        task_id,
                        &task.prompt,
                        &plan.dag,
                        &mut execution,
                        worker_registry,
                        &node_id,
                        &mut steps,
                    )?;
                    advanced |= node_advanced;
                    continue;
                }

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

        let plan_completed = execution.is_finished()
            && execution
                .nodes
                .values()
                .all(|node| node.status == PlanNodeStatus::Succeeded);
        let verification_decision = plan_completed.then(|| {
            crate::evaluate_verification_result(
                crate::infer_verification_policy(task.task_packet.as_ref()),
                task.verification_result.as_ref(),
            )
        });
        let waiting_for_verification = matches!(
            verification_decision.as_ref(),
            Some(crate::VerificationDecision::Failed { reason })
                if reason == "verification result is required before completion"
        );
        let verification_blocked = matches!(
            verification_decision.as_ref(),
            Some(crate::VerificationDecision::Failed { .. })
        ) && !waiting_for_verification;
        let completed = plan_completed
            && matches!(
                verification_decision,
                Some(
                    crate::VerificationDecision::NotRequired | crate::VerificationDecision::Passed
                )
            );
        let node_blocked = execution.nodes.values().any(|node| {
            matches!(
                node.status,
                PlanNodeStatus::Failed | PlanNodeStatus::Skipped
            )
        });
        let blocked = !completed && (node_blocked || verification_blocked);
        let status = if completed {
            TaskStatus::Completed
        } else if waiting_for_verification {
            TaskStatus::WaitingForVerification
        } else if blocked {
            TaskStatus::Blocked
        } else if self.worker_registry.is_some() {
            TaskStatus::Running
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
            blocked,
            message: if completed {
                "task execution completed".to_string()
            } else if waiting_for_verification {
                "task execution waiting for verification".to_string()
            } else if blocked {
                "task execution blocked with remaining nodes".to_string()
            } else {
                "task execution waiting for workers".to_string()
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

    #[allow(clippy::too_many_arguments)]
    fn advance_worker_node(
        &self,
        task_id: &str,
        task_prompt: &str,
        dag: &PlanDag,
        execution: &mut PlanExecution,
        worker_registry: &WorkerRegistry,
        node_id: &str,
        steps: &mut Vec<TaskExecutionStep>,
    ) -> Result<bool, String> {
        let node = execution
            .nodes
            .get(node_id)
            .cloned()
            .ok_or_else(|| format!("plan node not found: {node_id}"))?;

        match node.status {
            PlanNodeStatus::Ready => {
                execution.start_node(node_id)?;
                let worker = worker_registry.create(".", &[".".to_string()], true);
                let _ = worker_registry.observe(&worker.worker_id, "Ready for input\n>")?;
                let prompt = self.worker_prompt(task_prompt, dag, node_id);
                let worker = worker_registry.send_prompt(&worker.worker_id, Some(&prompt))?;
                execution.assign_worker(node_id, worker.worker_id.clone())?;
                steps.push(TaskExecutionStep {
                    task_id: task_id.to_string(),
                    node_id: Some(node_id.to_string()),
                    kind: TaskExecutionStepKind::DispatchWorker,
                    message: format!("dispatched node {node_id} to {}", worker.worker_id),
                });
                Ok(true)
            }
            PlanNodeStatus::Running => {
                let Some(worker_id) = node.worker_id.as_deref() else {
                    execution.fail_node(dag, node_id, "worker_missing")?;
                    steps.push(TaskExecutionStep {
                        task_id: task_id.to_string(),
                        node_id: Some(node_id.to_string()),
                        kind: TaskExecutionStepKind::Blocked,
                        message: format!("node {node_id} is running without an assigned worker"),
                    });
                    return Ok(true);
                };
                let Some(worker) = worker_registry.get(worker_id) else {
                    execution.fail_node(dag, node_id, "worker_missing")?;
                    steps.push(TaskExecutionStep {
                        task_id: task_id.to_string(),
                        node_id: Some(node_id.to_string()),
                        kind: TaskExecutionStepKind::Blocked,
                        message: format!("assigned worker {worker_id} was not found"),
                    });
                    return Ok(true);
                };
                self.advance_running_worker(
                    task_id,
                    dag,
                    execution,
                    node_id,
                    worker_registry,
                    worker,
                    steps,
                )
            }
            PlanNodeStatus::Failed | PlanNodeStatus::Skipped => {
                execution.retry_node(dag, node_id)?;
                Ok(true)
            }
            PlanNodeStatus::Pending | PlanNodeStatus::Succeeded => Ok(false),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn advance_running_worker(
        &self,
        task_id: &str,
        dag: &PlanDag,
        execution: &mut PlanExecution,
        node_id: &str,
        worker_registry: &WorkerRegistry,
        worker: Worker,
        steps: &mut Vec<TaskExecutionStep>,
    ) -> Result<bool, String> {
        match worker.status {
            WorkerStatus::Finished => {
                execution.succeed_node(
                    dag,
                    node_id,
                    Some(format!("worker {} finished", worker.worker_id)),
                )?;
                steps.push(TaskExecutionStep {
                    task_id: task_id.to_string(),
                    node_id: Some(node_id.to_string()),
                    kind: TaskExecutionStepKind::AwaitWorker,
                    message: format!("worker {} finished node {node_id}", worker.worker_id),
                });
                Ok(true)
            }
            WorkerStatus::ReadyForPrompt if worker.replay_prompt.is_some() => {
                let worker = worker_registry.send_prompt(&worker.worker_id, None)?;
                steps.push(TaskExecutionStep {
                    task_id: task_id.to_string(),
                    node_id: Some(node_id.to_string()),
                    kind: TaskExecutionStepKind::DispatchWorker,
                    message: format!("replayed node {node_id} prompt to {}", worker.worker_id),
                });
                Ok(false)
            }
            WorkerStatus::Failed | WorkerStatus::TrustRequired => {
                execution.fail_node(dag, node_id, worker.status.to_string())?;
                steps.push(TaskExecutionStep {
                    task_id: task_id.to_string(),
                    node_id: Some(node_id.to_string()),
                    kind: TaskExecutionStepKind::Blocked,
                    message: format!(
                        "worker {} blocked node {node_id}: {}",
                        worker.worker_id, worker.status
                    ),
                });
                Ok(true)
            }
            WorkerStatus::Spawning
            | WorkerStatus::ReadyForPrompt
            | WorkerStatus::PromptAccepted
            | WorkerStatus::Running => {
                steps.push(TaskExecutionStep {
                    task_id: task_id.to_string(),
                    node_id: Some(node_id.to_string()),
                    kind: TaskExecutionStepKind::AwaitWorker,
                    message: format!(
                        "awaiting worker {} for node {node_id}: {}",
                        worker.worker_id, worker.status
                    ),
                });
                Ok(false)
            }
        }
    }

    fn worker_prompt(&self, task_prompt: &str, dag: &PlanDag, node_id: &str) -> String {
        let Some(node) = self.dag_node(dag, node_id) else {
            return format!("Complete plan node {node_id} for task: {task_prompt}");
        };
        let tools = if node.candidate_tools.is_empty() {
            "<none>".to_string()
        } else {
            node.candidate_tools.join(", ")
        };
        format!(
            "Task: {task_prompt}\n\nPlan node: {}\nTitle: {}\nCandidate tools: {}\n\nComplete this node and report completion evidence.",
            node.id, node.title, tools
        )
    }

    fn is_task_container_node(&self, dag: &PlanDag, node_id: &str) -> bool {
        self.dag_node(dag, node_id)
            .is_some_and(|node| node.kind == PlanNodeKind::Task)
    }

    fn dag_node<'a>(&self, dag: &'a PlanDag, node_id: &str) -> Option<&'a PlanDagNode> {
        dag.nodes.iter().find(|node| node.id == node_id)
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

    #[test]
    fn worker_engine_dispatches_and_completes_node_from_worker_status() {
        let (registry, task_id) = sample_registry();
        let workers = WorkerRegistry::new();
        let engine = TaskExecutionEngine::with_workers(
            registry.clone(),
            VerificationRunner::new(None),
            workers.clone(),
        );

        let first = engine.execute(&task_id, None).expect("first execute");
        let task = registry.get(&task_id).expect("task");
        let plan = task.plan.expect("plan");
        let worker_id = plan.execution.nodes["node-1"]
            .worker_id
            .clone()
            .expect("worker assigned");

        assert!(!first.completed);
        assert!(!first.blocked);
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(
            plan.execution.nodes["node-1"].status,
            PlanNodeStatus::Running
        );
        assert_eq!(
            workers.get(&worker_id).expect("worker").status,
            WorkerStatus::PromptAccepted
        );
        assert!(first
            .steps
            .iter()
            .any(|step| step.kind == TaskExecutionStepKind::DispatchWorker));

        workers
            .observe_completion(&worker_id, "stop", 42)
            .expect("worker complete");
        let second = engine.execute(&task_id, None).expect("second execute");

        assert!(second.completed);
        assert_eq!(
            registry.get(&task_id).expect("task").status,
            TaskStatus::Completed
        );
    }
}
