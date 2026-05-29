use serde::{Deserialize, Serialize};

use crate::{attempt_recovery, FailureScenario, RecoveryContext, RecoveryEvent, RecoveryResult};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryOrchestratorDecision {
    Recovered,
    Escalate { reason: String },
    Blocked { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryOrchestratorOutcome {
    pub scenario: FailureScenario,
    pub result: RecoveryResult,
    pub decision: RecoveryOrchestratorDecision,
    pub events: Vec<RecoveryEvent>,
}

#[derive(Debug, Clone, Default)]
pub struct RecoveryOrchestrator {
    context: RecoveryContext,
}

impl RecoveryOrchestrator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn context(&self) -> &RecoveryContext {
        &self.context
    }

    pub fn recover_once(&mut self, scenario: FailureScenario) -> RecoveryOrchestratorOutcome {
        let start = self.context.events().len();
        let result = attempt_recovery(&scenario, &mut self.context);
        let events = self.context.events()[start..].to_vec();
        let decision = match &result {
            RecoveryResult::Recovered { .. } => RecoveryOrchestratorDecision::Recovered,
            RecoveryResult::PartialRecovery { remaining, .. } => {
                RecoveryOrchestratorDecision::Blocked {
                    reason: format!("partial recovery; {} step(s) remain", remaining.len()),
                }
            }
            RecoveryResult::EscalationRequired { reason } => {
                RecoveryOrchestratorDecision::Escalate {
                    reason: reason.clone(),
                }
            }
        };
        RecoveryOrchestratorOutcome {
            scenario,
            result,
            decision,
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_recovery_attempt_succeeds_and_second_escalates() {
        let mut orchestrator = RecoveryOrchestrator::new();
        let first = orchestrator.recover_once(FailureScenario::PromptMisdelivery);
        assert_eq!(first.decision, RecoveryOrchestratorDecision::Recovered);

        let second = orchestrator.recover_once(FailureScenario::PromptMisdelivery);
        assert!(matches!(
            second.decision,
            RecoveryOrchestratorDecision::Escalate { .. }
        ));
    }
}
