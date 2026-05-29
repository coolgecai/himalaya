use serde::{Deserialize, Serialize};

use crate::{
    DecisioningEvent, ModelRouteDecision, PlanExecutionEvent, ProgressLedgerEntry,
    RecoveryActionExecution, RecoveryEvent, TaskExecutionOutcome, TeamExecutionEvent,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEvent {
    Decisioning(Box<DecisioningEvent>),
    PlanExecution(PlanExecutionEvent),
    TaskLedger(ProgressLedgerEntry),
    ModelRoute(ModelRouteDecision),
    TeamExecution(TeamExecutionEvent),
    Recovery(RecoveryEvent),
    RecoveryAction(RecoveryActionExecution),
    TaskExecution(TaskExecutionOutcome),
}

impl RuntimeEvent {
    #[must_use]
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Decisioning(_) => "decisioning_event",
            Self::PlanExecution(_) => "plan_execution_event",
            Self::TaskLedger(_) => "task_ledger_event",
            Self::ModelRoute(_) => "model_route_event",
            Self::TeamExecution(_) => "team_execution_event",
            Self::Recovery(_) => "recovery_event",
            Self::RecoveryAction(_) => "recovery_action_event",
            Self::TaskExecution(_) => "task_execution_event",
        }
    }
}

pub trait RuntimeEventReporter: Send + Sync {
    fn emit_runtime_event(&self, event: &RuntimeEvent);
}
