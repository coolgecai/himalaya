use serde::{Deserialize, Serialize};

use crate::{
    build_verification_request, evaluate_verification_result, infer_verification_policy,
    FailureClassification, FailureClassifier, PermissionMode, PlanDag, PlanDagNode, PlanExecution,
    PlanNodeKind, PlanNodeStatus, RecoveryActionEngine, RecoveryActionExecution,
    RecoveryOrchestrator, RecoveryOrchestratorOutcome, TaskPacket, TaskRegistry, TaskStatus,
    TeamCoordinator, VerificationCommandResult, VerificationDecision, VerificationRequest,
    VerificationResult, VerificationRunner, Worker, WorkerRegistry, WorkerStatus,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPlanProgress {
    pub total: usize,
    pub pending: usize,
    pub ready: usize,
    pub running: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub skipped: usize,
    pub completed_nodes: Vec<String>,
    pub resumable_nodes: Vec<String>,
    pub current_node: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskExecutionReport {
    pub task_id: String,
    pub outcome: TaskExecutionOutcome,
    pub verification_result: Option<VerificationResult>,
    pub verification_decision: VerificationDecision,
    pub failure: Option<FailureClassification>,
    pub recovery: Option<RecoveryOrchestratorOutcome>,
    pub recovery_action: Option<RecoveryActionExecution>,
    pub final_status: TaskStatus,
    pub plan_progress: Option<TaskPlanProgress>,
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

    pub fn execute_with_recovery(
        &self,
        task_id: &str,
        from_node: Option<&str>,
        permission_mode: PermissionMode,
    ) -> Result<TaskExecutionReport, String> {
        let mut outcome = self.execute(task_id, from_node)?;
        let mut verification_result = self.ensure_task_verification(task_id)?;
        let mut verification_decision =
            self.verification_decision(task_id, verification_result.as_ref())?;
        self.apply_verification_decision(task_id, &verification_decision)?;
        let mut failure = self.classify_failure(task_id, &outcome, &verification_decision);
        let mut recovery = None;
        let mut recovery_action = None;

        if let Some(classification) = failure.clone() {
            let mut orchestrator = RecoveryOrchestrator::new();
            let recovery_outcome = orchestrator.recover_once(classification.scenario);
            for event in &recovery_outcome.events {
                self.registry
                    .record_recovery_event(task_id, event.clone())?;
            }
            let node_id = current_plan_node(&self.registry, task_id);
            let engine = RecoveryActionEngine::new();
            let plan = engine.plan(task_id.to_string(), &recovery_outcome, node_id);
            let action_execution =
                engine.execute_against_registry(plan, permission_mode, &self.registry);
            let should_retry = action_execution.results.iter().any(|result| {
                result.executed
                    && matches!(
                        result.action.kind,
                        crate::RecoveryActionKind::RetryNode
                            | crate::RecoveryActionKind::RerunVerification
                            | crate::RecoveryActionKind::SwitchModel
                            | crate::RecoveryActionKind::RetryMcpHandshake
                            | crate::RecoveryActionKind::RestartPlugin
                    )
            });
            recovery = Some(recovery_outcome);
            recovery_action = Some(action_execution);
            if should_retry {
                outcome = self.execute(task_id, None)?;
                verification_result = self.ensure_task_verification(task_id)?;
                verification_decision =
                    self.verification_decision(task_id, verification_result.as_ref())?;
                self.apply_verification_decision(task_id, &verification_decision)?;
                failure = self.classify_failure(task_id, &outcome, &verification_decision);
            }
        }

        let final_task = self
            .registry
            .get(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let completed = final_task.status == TaskStatus::Completed;
        let blocked = matches!(
            final_task.status,
            TaskStatus::Blocked | TaskStatus::Failed | TaskStatus::WaitingForPermission
        ) || failure.is_some();
        let message = if completed {
            "task execution loop completed".to_string()
        } else if let Some(classification) = failure.as_ref() {
            format!(
                "task execution loop blocked: {} ({})",
                classification.failure_class, classification.reason
            )
        } else {
            format!(
                "task execution loop ended with status {}",
                final_task.status
            )
        };

        Ok(TaskExecutionReport {
            task_id: task_id.to_string(),
            outcome,
            verification_result,
            verification_decision,
            failure,
            recovery,
            recovery_action,
            final_status: final_task.status,
            plan_progress: task_plan_progress(&final_task),
            completed,
            blocked,
            message,
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

    fn ensure_task_verification(
        &self,
        task_id: &str,
    ) -> Result<Option<VerificationResult>, String> {
        let task = self
            .registry
            .get(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        if !task_plan_succeeded(&task) {
            return Ok(task.verification_result);
        }
        let policy = infer_verification_policy(task.task_packet.as_ref());
        if policy == crate::VerificationPolicy::None {
            return Ok(task.verification_result);
        }
        if task.verification_result.is_some() {
            return Ok(task.verification_result);
        }
        let Some(packet) = task.task_packet.as_ref() else {
            return Ok(None);
        };
        let result = self.run_task_packet_verification(task_id, packet);
        self.registry.record_verification(task_id, result.clone())?;
        Ok(Some(result))
    }

    fn run_task_packet_verification(
        &self,
        task_id: &str,
        packet: &TaskPacket,
    ) -> VerificationResult {
        let policy = infer_verification_policy(Some(packet));
        let request = build_verification_request(task_id, packet, policy);
        self.verification_runner.run(&request)
    }

    fn verification_decision(
        &self,
        task_id: &str,
        verification_result: Option<&VerificationResult>,
    ) -> Result<VerificationDecision, String> {
        let task = self
            .registry
            .get(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let policy = infer_verification_policy(task.task_packet.as_ref());
        if !task_plan_succeeded(&task) {
            return Ok(task.task_packet.as_ref().map_or(
                VerificationDecision::NotRequired,
                |packet| {
                    VerificationDecision::Required(build_verification_request(
                        task_id, packet, policy,
                    ))
                },
            ));
        }
        Ok(evaluate_verification_result(policy, verification_result))
    }

    fn apply_verification_decision(
        &self,
        task_id: &str,
        decision: &VerificationDecision,
    ) -> Result<(), String> {
        let task = self
            .registry
            .get(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        if !task_plan_succeeded(&task) {
            return Ok(());
        }
        match decision {
            VerificationDecision::Passed | VerificationDecision::NotRequired => {
                self.registry.set_status(task_id, TaskStatus::Completed)?;
            }
            VerificationDecision::Failed { reason }
                if reason == "verification result is required before completion" =>
            {
                self.registry
                    .set_status(task_id, TaskStatus::WaitingForVerification)?;
            }
            VerificationDecision::Failed { .. } => {
                self.registry.set_status(task_id, TaskStatus::Blocked)?;
            }
            VerificationDecision::Required(_) => {
                self.registry
                    .set_status(task_id, TaskStatus::WaitingForVerification)?;
            }
        }
        Ok(())
    }

    fn classify_failure(
        &self,
        task_id: &str,
        outcome: &TaskExecutionOutcome,
        verification_decision: &VerificationDecision,
    ) -> Option<FailureClassification> {
        let classifier = FailureClassifier::new();
        let task = self.registry.get(task_id)?;
        if task_plan_succeeded(&task) {
            if let Some(classification) =
                classifier.classify_verification_decision(verification_decision)
            {
                return Some(classification);
            }
        }
        if !outcome.blocked {
            return None;
        }
        let failure_reason = task
            .plan
            .as_ref()
            .and_then(|plan| {
                plan.execution
                    .nodes
                    .values()
                    .find(|node| node.status == PlanNodeStatus::Failed)
                    .and_then(|node| {
                        node.failure_class
                            .clone()
                            .or_else(|| node.output_summary.clone())
                    })
            })
            .or_else(|| Some(outcome.message.clone()))?;
        Some(classifier.classify_reason(&failure_reason))
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

#[must_use]
pub fn task_plan_progress(task: &crate::task_registry::Task) -> Option<TaskPlanProgress> {
    let plan = task.plan.as_ref()?;
    let mut progress = TaskPlanProgress {
        total: plan.execution.nodes.len(),
        pending: 0,
        ready: 0,
        running: 0,
        succeeded: 0,
        failed: 0,
        skipped: 0,
        completed_nodes: plan.execution.completed_nodes(),
        resumable_nodes: plan.execution.resumable_nodes(),
        current_node: plan
            .resume_cursor
            .as_ref()
            .and_then(|cursor| cursor.node_id.clone()),
    };
    for node in plan.execution.nodes.values() {
        match node.status {
            PlanNodeStatus::Pending => progress.pending += 1,
            PlanNodeStatus::Ready => progress.ready += 1,
            PlanNodeStatus::Running => progress.running += 1,
            PlanNodeStatus::Succeeded => progress.succeeded += 1,
            PlanNodeStatus::Failed => progress.failed += 1,
            PlanNodeStatus::Skipped => progress.skipped += 1,
        }
    }
    Some(progress)
}

fn task_plan_succeeded(task: &crate::task_registry::Task) -> bool {
    task.plan.as_ref().is_some_and(|plan| {
        !plan.execution.nodes.is_empty()
            && plan
                .execution
                .nodes
                .values()
                .all(|node| node.status == PlanNodeStatus::Succeeded)
    })
}

fn current_plan_node(registry: &TaskRegistry, task_id: &str) -> Option<String> {
    let task = registry.get(task_id)?;
    task.plan
        .as_ref()
        .and_then(|plan| plan.resume_cursor.as_ref())
        .and_then(|cursor| cursor.node_id.clone())
        .or_else(|| {
            task.plan.as_ref().and_then(|plan| {
                plan.execution
                    .nodes
                    .values()
                    .find(|node| node.status == PlanNodeStatus::Failed)
                    .map(|node| node.node_id.clone())
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        PlanDag, PlanDagEdge, PlanDagEdgeKind, PlanDagNode, PlanExecution, PlanNodeKind, TaskPacket,
    };

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

    fn packet_registry(acceptance_tests: Vec<String>) -> (TaskRegistry, String) {
        let registry = TaskRegistry::new();
        let task = registry
            .create_from_packet(TaskPacket {
                objective: "execute packet".to_string(),
                scope: "runtime".to_string(),
                repo: ".".to_string(),
                branch_policy: "current".to_string(),
                acceptance_tests,
                commit_policy: "no commit".to_string(),
                reporting_contract: "report verification".to_string(),
                escalation_policy: "block on failure".to_string(),
            })
            .expect("packet task should create");
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

    #[test]
    fn execute_with_recovery_runs_packet_verification_and_completes() {
        let (registry, task_id) = packet_registry(vec!["python3 --version".to_string()]);
        let engine = TaskExecutionEngine::new(registry.clone(), VerificationRunner::new(None));

        let report = engine
            .execute_with_recovery(&task_id, None, PermissionMode::ReadOnly)
            .expect("execution loop should run");

        assert!(report.completed, "{report:?}");
        assert!(!report.blocked, "{report:?}");
        assert_eq!(report.final_status, TaskStatus::Completed);
        assert!(matches!(
            report.verification_decision,
            VerificationDecision::Passed
        ));
        assert_eq!(
            registry.get(&task_id).expect("task").status,
            TaskStatus::Completed
        );
        assert_eq!(
            report.plan_progress.as_ref().expect("progress").succeeded,
            report.plan_progress.as_ref().expect("progress").total
        );
    }

    #[test]
    fn execute_with_recovery_records_failure_and_recovery_action() {
        let (registry, task_id) = packet_registry(vec!["definitely-not-allowed".to_string()]);
        let engine = TaskExecutionEngine::new(registry.clone(), VerificationRunner::new(None));

        let report = engine
            .execute_with_recovery(&task_id, None, PermissionMode::ReadOnly)
            .expect("execution loop should run");
        let task = registry.get(&task_id).expect("task");

        assert!(report.blocked, "{report:?}");
        assert!(matches!(
            report.final_status,
            TaskStatus::Blocked | TaskStatus::WaitingForPermission
        ));
        assert!(report.failure.is_some());
        assert!(!task.recovery_events.is_empty());
        assert!(!task.recovery_action_executions.is_empty());
    }

    #[test]
    fn execute_with_recovery_does_not_verify_before_workers_finish() {
        let (registry, task_id) = packet_registry(vec!["python3 --version".to_string()]);
        let workers = WorkerRegistry::new();
        let engine = TaskExecutionEngine::with_workers(
            registry.clone(),
            VerificationRunner::new(None),
            workers,
        );

        let report = engine
            .execute_with_recovery(&task_id, None, PermissionMode::ReadOnly)
            .expect("execution loop should wait for worker");

        assert!(!report.completed, "{report:?}");
        assert!(!report.blocked, "{report:?}");
        assert_eq!(report.final_status, TaskStatus::Running);
        assert!(matches!(
            report.verification_decision,
            VerificationDecision::Required(_)
        ));
        assert!(report.verification_result.is_none());
        assert!(report.failure.is_none());
        assert!(report.recovery.is_none());
    }
}
