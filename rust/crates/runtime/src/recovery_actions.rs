use serde::{Deserialize, Serialize};

use crate::{
    FailureScenario, PermissionMode, RecoveryOrchestratorOutcome, RecoveryStep, TaskRegistry,
    TaskStatus,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryActionRisk {
    Safe,
    NeedsWorkspaceWrite,
    NeedsDangerFullAccess,
    NeedsHuman,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryActionKind {
    RerunVerification,
    RetryNode,
    RequestPermission,
    SwitchModel,
    RestartPlugin,
    RetryMcpHandshake,
    MarkBlocked,
    Escalate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryAction {
    pub kind: RecoveryActionKind,
    pub scenario: FailureScenario,
    pub risk: RecoveryActionRisk,
    pub node_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryActionPlan {
    pub task_id: String,
    pub actions: Vec<RecoveryAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryActionResult {
    pub action: RecoveryAction,
    pub executed: bool,
    pub blocked: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryActionExecution {
    pub task_id: String,
    pub results: Vec<RecoveryActionResult>,
}

#[derive(Debug, Clone, Default)]
pub struct RecoveryActionEngine;

impl RecoveryActionEngine {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    #[must_use]
    pub fn plan(
        &self,
        task_id: impl Into<String>,
        outcome: &RecoveryOrchestratorOutcome,
        node_id: Option<String>,
    ) -> RecoveryActionPlan {
        let task_id = task_id.into();
        let mut actions = recovery_steps(outcome)
            .into_iter()
            .map(|step| action_from_step(outcome.scenario, step, node_id.clone()))
            .collect::<Vec<_>>();
        if actions.is_empty() {
            actions.push(RecoveryAction {
                kind: RecoveryActionKind::MarkBlocked,
                scenario: outcome.scenario,
                risk: RecoveryActionRisk::Safe,
                node_id,
                message: "mark task blocked until recovery can be resumed".to_string(),
            });
        }
        RecoveryActionPlan { task_id, actions }
    }

    #[must_use]
    pub fn execute_safe_actions(
        &self,
        plan: RecoveryActionPlan,
        permission_mode: PermissionMode,
    ) -> RecoveryActionExecution {
        let results = plan
            .actions
            .into_iter()
            .map(|action| {
                let allowed = action_allowed(action.risk, permission_mode);
                RecoveryActionResult {
                    blocked: !allowed,
                    executed: allowed && matches!(action.risk, RecoveryActionRisk::Safe),
                    reason: if allowed {
                        "safe recovery action recorded".to_string()
                    } else {
                        format!(
                            "recovery action requires {:?}; current mode is {}",
                            action.risk,
                            permission_mode.as_str()
                        )
                    },
                    action,
                }
            })
            .collect();
        RecoveryActionExecution {
            task_id: plan.task_id,
            results,
        }
    }

    pub fn execute_against_registry(
        &self,
        plan: RecoveryActionPlan,
        permission_mode: PermissionMode,
        registry: &TaskRegistry,
    ) -> RecoveryActionExecution {
        let task_id = plan.task_id.clone();
        let mut execution = self.execute_safe_actions(plan, permission_mode);
        for result in &mut execution.results {
            if result.blocked {
                match result.action.kind {
                    RecoveryActionKind::RequestPermission => {
                        let _ = registry.set_status(&task_id, TaskStatus::WaitingForPermission);
                    }
                    _ => {
                        let _ = registry.set_status(&task_id, TaskStatus::Blocked);
                    }
                }
                continue;
            }
            match result.action.kind {
                RecoveryActionKind::RetryNode => {
                    if let Some(node_id) = result.action.node_id.as_deref() {
                        match registry.retry_plan_node(&task_id, node_id) {
                            Ok(_) => {
                                result.executed = true;
                                result.reason = format!("scheduled retry for node {node_id}");
                            }
                            Err(error) => {
                                result.blocked = true;
                                result.executed = false;
                                result.reason = error;
                            }
                        }
                    }
                }
                RecoveryActionKind::MarkBlocked | RecoveryActionKind::Escalate => {
                    let _ = registry.set_status(&task_id, TaskStatus::Blocked);
                    result.executed = true;
                }
                RecoveryActionKind::RequestPermission => {
                    let _ = registry.set_status(&task_id, TaskStatus::WaitingForPermission);
                    result.executed = false;
                    result.blocked = true;
                }
                RecoveryActionKind::RerunVerification => {
                    let _ = registry.set_status(&task_id, TaskStatus::WaitingForVerification);
                }
                RecoveryActionKind::SwitchModel
                | RecoveryActionKind::RestartPlugin
                | RecoveryActionKind::RetryMcpHandshake => {}
            }
        }
        let _ = registry.record_recovery_action_execution(&task_id, execution.clone());
        execution
    }
}

fn recovery_steps(outcome: &RecoveryOrchestratorOutcome) -> Vec<RecoveryStep> {
    if let Some(steps) = outcome.events.iter().find_map(|event| match event {
        crate::RecoveryEvent::RecoveryAttempted { recipe, .. } => Some(recipe.steps.clone()),
        _ => None,
    }) {
        return steps;
    }
    outcome.result.recipe_steps()
}

trait RecoveryResultSteps {
    fn recipe_steps(&self) -> Vec<RecoveryStep>;
}

impl RecoveryResultSteps for crate::RecoveryResult {
    fn recipe_steps(&self) -> Vec<RecoveryStep> {
        match self {
            Self::Recovered { .. } => Vec::new(),
            Self::PartialRecovery { remaining, .. } => remaining.clone(),
            Self::EscalationRequired { .. } => Vec::new(),
        }
    }
}

fn action_from_step(
    scenario: FailureScenario,
    step: RecoveryStep,
    node_id: Option<String>,
) -> RecoveryAction {
    match step {
        RecoveryStep::CleanBuild => RecoveryAction {
            kind: RecoveryActionKind::RerunVerification,
            scenario,
            risk: RecoveryActionRisk::NeedsDangerFullAccess,
            node_id,
            message: "rerun verification after clean build recovery".to_string(),
        },
        RecoveryStep::AcceptTrustPrompt => RecoveryAction {
            kind: RecoveryActionKind::RequestPermission,
            scenario,
            risk: RecoveryActionRisk::NeedsHuman,
            node_id,
            message: "request user approval for trust prompt".to_string(),
        },
        RecoveryStep::RedirectPromptToAgent => RecoveryAction {
            kind: RecoveryActionKind::RetryNode,
            scenario,
            risk: RecoveryActionRisk::Safe,
            node_id,
            message: "retry node with corrected prompt delivery".to_string(),
        },
        RecoveryStep::RebaseBranch => RecoveryAction {
            kind: RecoveryActionKind::Escalate,
            scenario,
            risk: RecoveryActionRisk::NeedsHuman,
            node_id,
            message: "stale branch recovery requires explicit user git action".to_string(),
        },
        RecoveryStep::RetryMcpHandshake { .. } => RecoveryAction {
            kind: RecoveryActionKind::RetryMcpHandshake,
            scenario,
            risk: RecoveryActionRisk::Safe,
            node_id,
            message: "retry MCP handshake".to_string(),
        },
        RecoveryStep::RestartPlugin { name } => RecoveryAction {
            kind: RecoveryActionKind::RestartPlugin,
            scenario,
            risk: RecoveryActionRisk::Safe,
            node_id,
            message: format!("restart plugin {name}"),
        },
        RecoveryStep::RestartWorker => RecoveryAction {
            kind: RecoveryActionKind::SwitchModel,
            scenario,
            risk: RecoveryActionRisk::Safe,
            node_id,
            message: "restart worker or switch route".to_string(),
        },
        RecoveryStep::EscalateToHuman { reason } => RecoveryAction {
            kind: RecoveryActionKind::Escalate,
            scenario,
            risk: RecoveryActionRisk::NeedsHuman,
            node_id,
            message: reason,
        },
    }
}

fn action_allowed(risk: RecoveryActionRisk, permission_mode: PermissionMode) -> bool {
    match risk {
        RecoveryActionRisk::Safe => true,
        RecoveryActionRisk::NeedsWorkspaceWrite => {
            permission_mode.satisfies(PermissionMode::WorkspaceWrite)
        }
        RecoveryActionRisk::NeedsDangerFullAccess => {
            permission_mode.satisfies(PermissionMode::DangerFullAccess)
        }
        RecoveryActionRisk::NeedsHuman => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FailureScenario, PlanDag, PlanDagEdge, PlanDagNode, PlanExecution, PlanNodeKind,
        RecoveryOrchestrator,
    };

    fn registry_with_failed_node() -> (TaskRegistry, String) {
        let registry = TaskRegistry::new();
        let task = registry.create("recover", Some("test"));
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
        let mut execution = PlanExecution::new(&dag);
        execution.start_node("node-1").expect("start");
        execution.fail_node(&dag, "node-1", "failed").expect("fail");
        registry
            .record_plan(&task.task_id, dag, execution)
            .expect("plan");
        (registry, task.task_id)
    }
    #[test]
    fn safe_prompt_misdelivery_action_executes_without_privilege() {
        let mut orchestrator = RecoveryOrchestrator::new();
        let outcome = orchestrator.recover_once(FailureScenario::PromptMisdelivery);
        let engine = RecoveryActionEngine::new();
        let plan = engine.plan("task-1", &outcome, Some("node-1".to_string()));
        let execution = engine.execute_safe_actions(plan, PermissionMode::ReadOnly);

        assert!(execution.results.iter().any(|result| result.executed));
    }

    #[test]
    fn registry_executor_retries_failed_node() {
        let (registry, task_id) = registry_with_failed_node();
        let action = RecoveryAction {
            kind: RecoveryActionKind::RetryNode,
            scenario: FailureScenario::PromptMisdelivery,
            risk: RecoveryActionRisk::Safe,
            node_id: Some("node-1".to_string()),
            message: "retry".to_string(),
        };
        let engine = RecoveryActionEngine::new();
        let execution = engine.execute_against_registry(
            RecoveryActionPlan {
                task_id: task_id.clone(),
                actions: vec![action],
            },
            PermissionMode::ReadOnly,
            &registry,
        );
        let task = registry.get(&task_id).expect("task");
        let node = &task.plan.expect("plan").execution.nodes["node-1"];

        assert!(execution.results[0].executed);
        assert!(!execution.results[0].blocked);
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(node.status, crate::PlanNodeStatus::Ready);
    }

    #[test]
    fn registry_executor_requests_permission_status_for_human_action() {
        let registry = TaskRegistry::new();
        let task = registry.create("recover", Some("test"));
        let action = RecoveryAction {
            kind: RecoveryActionKind::RequestPermission,
            scenario: FailureScenario::TrustPromptUnresolved,
            risk: RecoveryActionRisk::NeedsHuman,
            node_id: None,
            message: "approve".to_string(),
        };
        let engine = RecoveryActionEngine::new();
        let execution = engine.execute_against_registry(
            RecoveryActionPlan {
                task_id: task.task_id.clone(),
                actions: vec![action],
            },
            PermissionMode::ReadOnly,
            &registry,
        );

        assert!(execution.results[0].blocked);
        assert_eq!(
            registry.get(&task.task_id).expect("task").status,
            TaskStatus::WaitingForPermission
        );
    }

    #[test]
    fn danger_recovery_action_is_blocked_in_read_only() {
        let action = RecoveryAction {
            kind: RecoveryActionKind::RerunVerification,
            scenario: FailureScenario::CompileRedCrossCrate,
            risk: RecoveryActionRisk::NeedsDangerFullAccess,
            node_id: None,
            message: "rerun".to_string(),
        };
        let engine = RecoveryActionEngine::new();
        let execution = engine.execute_safe_actions(
            RecoveryActionPlan {
                task_id: "task-1".to_string(),
                actions: vec![action],
            },
            PermissionMode::ReadOnly,
        );

        assert!(execution.results[0].blocked);
        assert!(!execution.results[0].executed);
    }
}
