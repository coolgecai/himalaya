use serde::{Deserialize, Serialize};

use crate::{ModelRouteDecision, ModelRoutePhase, TaskStatus, VerificationDecision};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRole {
    Planner,
    Implementer,
    Verifier,
    Reviewer,
    Summarizer,
}

impl TeamRole {
    #[must_use]
    pub fn route_phase(self) -> ModelRoutePhase {
        match self {
            Self::Planner => ModelRoutePhase::Planning,
            Self::Implementer => ModelRoutePhase::Coding,
            Self::Verifier | Self::Reviewer => ModelRoutePhase::Verification,
            Self::Summarizer => ModelRoutePhase::Summarization,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamExecutionEventKind {
    TaskAssigned,
    NodeStarted,
    NodeFinished,
    VerificationFailed,
    VerificationPassed,
    SummaryReady,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamExecutionEvent {
    pub seq: u64,
    pub team_id: String,
    pub task_id: String,
    pub role: TeamRole,
    pub kind: TeamExecutionEventKind,
    pub model_route: Option<ModelRouteDecision>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamExecutionSummary {
    pub team_id: String,
    pub task_id: String,
    pub status: TaskStatus,
    pub events: Vec<TeamExecutionEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamExecutionLedger {
    pub team_id: String,
    pub task_id: String,
    events: Vec<TeamExecutionEvent>,
    seq: u64,
}

impl TeamExecutionLedger {
    #[must_use]
    pub fn new(team_id: impl Into<String>, task_id: impl Into<String>) -> Self {
        Self {
            team_id: team_id.into(),
            task_id: task_id.into(),
            events: Vec::new(),
            seq: 0,
        }
    }

    pub fn push(
        &mut self,
        role: TeamRole,
        kind: TeamExecutionEventKind,
        model_route: Option<ModelRouteDecision>,
        message: Option<String>,
    ) -> TeamExecutionEvent {
        self.seq += 1;
        let event = TeamExecutionEvent {
            seq: self.seq,
            team_id: self.team_id.clone(),
            task_id: self.task_id.clone(),
            role,
            kind,
            model_route,
            message,
        };
        self.events.push(event.clone());
        event
    }

    pub fn record_verification(
        &mut self,
        decision: &VerificationDecision,
        model_route: Option<ModelRouteDecision>,
    ) -> TeamExecutionEvent {
        match decision {
            VerificationDecision::Passed => self.push(
                TeamRole::Verifier,
                TeamExecutionEventKind::VerificationPassed,
                model_route,
                Some("verification passed".to_string()),
            ),
            VerificationDecision::Failed { reason } => self.push(
                TeamRole::Verifier,
                TeamExecutionEventKind::VerificationFailed,
                model_route,
                Some(reason.clone()),
            ),
            VerificationDecision::NotRequired => self.push(
                TeamRole::Verifier,
                TeamExecutionEventKind::VerificationPassed,
                model_route,
                Some("verification not required".to_string()),
            ),
            VerificationDecision::Required(request) => self.push(
                TeamRole::Verifier,
                TeamExecutionEventKind::NodeStarted,
                model_route,
                Some(format!("verification required by {:?}", request.policy)),
            ),
        }
    }

    #[must_use]
    pub fn events(&self) -> &[TeamExecutionEvent] {
        &self.events
    }

    #[must_use]
    pub fn summary(&self, status: TaskStatus) -> TeamExecutionSummary {
        TeamExecutionSummary {
            team_id: self.team_id.clone(),
            task_id: self.task_id.clone(),
            status,
            events: self.events.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_team_role_events_with_sequence() {
        let mut ledger = TeamExecutionLedger::new("team-1", "task-1");
        ledger.push(
            TeamRole::Planner,
            TeamExecutionEventKind::TaskAssigned,
            None,
            Some("assigned".to_string()),
        );
        ledger.push(
            TeamRole::Summarizer,
            TeamExecutionEventKind::SummaryReady,
            None,
            Some("done".to_string()),
        );
        assert_eq!(ledger.events()[0].seq, 1);
        assert_eq!(ledger.events()[1].seq, 2);
    }
}
