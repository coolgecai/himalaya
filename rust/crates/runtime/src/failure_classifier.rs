use serde::{Deserialize, Serialize};

use crate::{FailureScenario, VerificationDecision, VerificationResult};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureClassification {
    pub scenario: FailureScenario,
    pub failure_class: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct FailureClassifier;

impl FailureClassifier {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    #[must_use]
    pub fn classify_verification_decision(
        &self,
        decision: &VerificationDecision,
    ) -> Option<FailureClassification> {
        match decision {
            VerificationDecision::Failed { reason } => Some(self.classify_reason(reason)),
            VerificationDecision::Required(request) => Some(FailureClassification {
                scenario: FailureScenario::ProviderFailure,
                failure_class: "verification_missing".to_string(),
                reason: format!(
                    "verification required before completion: {:?}",
                    request.policy
                ),
            }),
            VerificationDecision::NotRequired | VerificationDecision::Passed => None,
        }
    }

    #[must_use]
    pub fn classify_verification_result(
        &self,
        result: &VerificationResult,
    ) -> Option<FailureClassification> {
        if result.passed {
            return None;
        }
        Some(self.classify_reason(&result.summary))
    }

    #[must_use]
    pub fn classify_reason(&self, reason: &str) -> FailureClassification {
        let normalized = reason.to_lowercase();
        let (scenario, failure_class) = if contains_any(
            &normalized,
            &["stale branch", "base commit", "rebase", "merge conflict"],
        ) {
            (FailureScenario::StaleBranch, "stale_branch")
        } else if contains_any(
            &normalized,
            &[
                "cargo",
                "rustc",
                "compile",
                "clippy",
                "test failed",
                "tests failed",
                "exit code",
                "non-zero",
            ],
        ) {
            (
                FailureScenario::CompileRedCrossCrate,
                "verification_command",
            )
        } else if contains_any(&normalized, &["mcp", "handshake", "json-rpc", "protocol"]) {
            (FailureScenario::McpHandshakeFailure, "mcp_handshake")
        } else if contains_any(&normalized, &["plugin", "extension host", "activation"]) {
            (FailureScenario::PartialPluginStartup, "plugin_startup")
        } else if contains_any(
            &normalized,
            &["permission", "denied", "trust", "not allowed", "requires"],
        ) {
            (FailureScenario::TrustPromptUnresolved, "trust_gate")
        } else if contains_any(
            &normalized,
            &[
                "provider",
                "api",
                "model",
                "rate limit",
                "timeout",
                "connection",
            ],
        ) {
            (FailureScenario::ProviderFailure, "provider_failure")
        } else {
            (FailureScenario::ProviderFailure, "verification_failure")
        };

        FailureClassification {
            scenario,
            failure_class: failure_class.to_string(),
            reason: reason.to_string(),
        }
    }
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_compile_failures_from_command_output() {
        let classifier = FailureClassifier::new();
        let classification =
            classifier.classify_reason("cargo test failed with non-zero exit code");

        assert_eq!(
            classification.scenario,
            FailureScenario::CompileRedCrossCrate
        );
        assert_eq!(classification.failure_class, "verification_command");
    }

    #[test]
    fn ignores_passed_verification_results() {
        let classifier = FailureClassifier::new();
        let result = VerificationResult {
            task_id: "task-1".to_string(),
            passed: true,
            observed_green_level: None,
            summary: "ok".to_string(),
            evidence: Vec::new(),
        };

        assert_eq!(classifier.classify_verification_result(&result), None);
    }
}
