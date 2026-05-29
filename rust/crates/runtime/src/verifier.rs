use serde::{Deserialize, Serialize};

use crate::{green_contract::GreenLevel, TaskPacket};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPolicy {
    None,
    Targeted,
    Full,
    ExternalAgent,
}

impl VerificationPolicy {
    #[must_use]
    pub fn required_green_level(self) -> Option<GreenLevel> {
        match self {
            Self::None => None,
            Self::Targeted | Self::ExternalAgent => Some(GreenLevel::TargetedTests),
            Self::Full => Some(GreenLevel::Workspace),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationRequest {
    pub task_id: String,
    pub objective: String,
    pub scope: String,
    pub acceptance_tests: Vec<String>,
    pub reporting_contract: String,
    pub policy: VerificationPolicy,
    pub required_green_level: Option<GreenLevel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationResult {
    pub task_id: String,
    pub passed: bool,
    pub observed_green_level: Option<GreenLevel>,
    pub summary: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationDecision {
    NotRequired,
    Required(VerificationRequest),
    Passed,
    Failed { reason: String },
}

#[must_use]
pub fn infer_verification_policy(packet: Option<&TaskPacket>) -> VerificationPolicy {
    let Some(packet) = packet else {
        return VerificationPolicy::None;
    };
    if packet.acceptance_tests.is_empty() {
        return VerificationPolicy::ExternalAgent;
    }
    if packet
        .acceptance_tests
        .iter()
        .any(|test| test.contains("--workspace") || test.contains("workspace"))
    {
        VerificationPolicy::Full
    } else {
        VerificationPolicy::Targeted
    }
}

#[must_use]
pub fn build_verification_request(
    task_id: &str,
    packet: &TaskPacket,
    policy: VerificationPolicy,
) -> VerificationRequest {
    VerificationRequest {
        task_id: task_id.to_string(),
        objective: packet.objective.clone(),
        scope: packet.scope.clone(),
        acceptance_tests: packet.acceptance_tests.clone(),
        reporting_contract: packet.reporting_contract.clone(),
        policy,
        required_green_level: policy.required_green_level(),
    }
}

#[must_use]
pub fn evaluate_verification_result(
    policy: VerificationPolicy,
    result: Option<&VerificationResult>,
) -> VerificationDecision {
    if policy == VerificationPolicy::None {
        return VerificationDecision::NotRequired;
    }
    let Some(result) = result else {
        return VerificationDecision::Failed {
            reason: "verification result is required before completion".to_string(),
        };
    };
    if !result.passed {
        return VerificationDecision::Failed {
            reason: result.summary.clone(),
        };
    }
    if let Some(required) = policy.required_green_level() {
        match result.observed_green_level {
            Some(observed) if observed >= required => VerificationDecision::Passed,
            observed => VerificationDecision::Failed {
                reason: format!(
                    "verification observed green level {observed:?}, required {required:?}"
                ),
            },
        }
    } else {
        VerificationDecision::Passed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(tests: Vec<&str>) -> TaskPacket {
        TaskPacket {
            objective: "Ship feature".to_string(),
            scope: "runtime".to_string(),
            repo: "repo".to_string(),
            branch_policy: "main".to_string(),
            acceptance_tests: tests.into_iter().map(str::to_string).collect(),
            commit_policy: "no commit".to_string(),
            reporting_contract: "report checks".to_string(),
            escalation_policy: "manual".to_string(),
        }
    }

    #[test]
    fn infers_full_policy_for_workspace_tests() {
        assert_eq!(
            infer_verification_policy(Some(&packet(vec!["cargo test --workspace"]))),
            VerificationPolicy::Full
        );
    }

    #[test]
    fn blocks_completion_when_required_result_is_missing() {
        assert!(matches!(
            evaluate_verification_result(VerificationPolicy::Targeted, None),
            VerificationDecision::Failed { .. }
        ));
    }

    #[test]
    fn requires_observed_green_level_to_meet_policy() {
        let result = VerificationResult {
            task_id: "task-1".to_string(),
            passed: true,
            observed_green_level: Some(GreenLevel::Package),
            summary: "package green".to_string(),
            evidence: Vec::new(),
        };
        assert!(matches!(
            evaluate_verification_result(VerificationPolicy::Full, Some(&result)),
            VerificationDecision::Failed { .. }
        ));
    }
}
