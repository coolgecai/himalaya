use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRoutePhase {
    Planning,
    Coding,
    Verification,
    Summarization,
    Vision,
    LocalFast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    Planning,
    Coding,
    Refactor,
    TestGeneration,
    Verification,
    Summarization,
    Vision,
    LocalFast,
    Cheap,
    LongContext,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRoute {
    pub role: ModelRoutePhase,
    pub model: String,
    pub provider: Option<String>,
    pub capabilities: Vec<ModelCapability>,
    pub max_tokens: Option<u32>,
    pub cost_weight: u8,
    pub latency_weight: u8,
    pub quality_weight: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoERoutingPolicy {
    pub routes: Vec<ModelRoute>,
    pub default_model: String,
}

impl MoERoutingPolicy {
    #[must_use]
    pub fn new(default_model: impl Into<String>, routes: Vec<ModelRoute>) -> Self {
        Self {
            default_model: default_model.into(),
            routes,
        }
    }

    #[must_use]
    pub fn balanced(default_model: impl Into<String>) -> Self {
        let default_model = default_model.into();
        Self {
            routes: vec![
                ModelRoute::new(ModelRoutePhase::Planning, default_model.clone())
                    .with_capabilities(vec![
                        ModelCapability::Planning,
                        ModelCapability::LongContext,
                    ])
                    .with_weights(1, 1, 5),
                ModelRoute::new(ModelRoutePhase::Coding, default_model.clone())
                    .with_capabilities(vec![ModelCapability::Coding, ModelCapability::Refactor])
                    .with_weights(2, 2, 5),
                ModelRoute::new(ModelRoutePhase::Verification, default_model.clone())
                    .with_capabilities(vec![
                        ModelCapability::Verification,
                        ModelCapability::TestGeneration,
                    ])
                    .with_weights(2, 2, 5),
                ModelRoute::new(ModelRoutePhase::Summarization, default_model.clone())
                    .with_capabilities(vec![ModelCapability::Summarization, ModelCapability::Cheap])
                    .with_weights(5, 3, 2),
            ],
            default_model,
        }
    }
}

impl ModelRoute {
    #[must_use]
    pub fn new(role: ModelRoutePhase, model: impl Into<String>) -> Self {
        Self {
            role,
            model: model.into(),
            provider: None,
            capabilities: Vec::new(),
            max_tokens: None,
            cost_weight: 1,
            latency_weight: 1,
            quality_weight: 1,
        }
    }

    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Vec<ModelCapability>) -> Self {
        self.capabilities = capabilities;
        self
    }

    #[must_use]
    pub fn with_weights(mut self, cost_weight: u8, latency_weight: u8, quality_weight: u8) -> Self {
        self.cost_weight = cost_weight;
        self.latency_weight = latency_weight;
        self.quality_weight = quality_weight;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRouteDecision {
    pub phase: ModelRoutePhase,
    pub model: String,
    pub provider: Option<String>,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRouteFeedback {
    pub task_id: String,
    pub route: ModelRouteDecision,
    pub succeeded: Option<bool>,
    pub latency_ms: Option<u32>,
    pub verification_passed: Option<bool>,
    pub recovery_triggered: bool,
    pub timestamp: u64,
    pub note: Option<String>,
}

impl ModelRouteFeedback {
    #[must_use]
    pub fn pending(task_id: impl Into<String>, route: ModelRouteDecision, timestamp: u64) -> Self {
        Self {
            task_id: task_id.into(),
            route,
            succeeded: None,
            latency_ms: None,
            verification_passed: None,
            recovery_triggered: false,
            timestamp,
            note: None,
        }
    }

    #[must_use]
    pub fn with_outcome(
        mut self,
        succeeded: bool,
        verification_passed: Option<bool>,
        recovery_triggered: bool,
        note: Option<String>,
    ) -> Self {
        self.succeeded = Some(succeeded);
        self.verification_passed = verification_passed;
        self.recovery_triggered = recovery_triggered;
        self.note = note;
        self
    }
}

#[derive(Debug, Clone)]
pub struct ModelRouter {
    policy: MoERoutingPolicy,
}

impl ModelRouter {
    #[must_use]
    pub fn new(policy: MoERoutingPolicy) -> Self {
        Self { policy }
    }

    #[must_use]
    pub fn select(&self, phase: ModelRoutePhase) -> ModelRouteDecision {
        if let Some(route) = self.policy.routes.iter().find(|route| route.role == phase) {
            return ModelRouteDecision {
                phase,
                model: route.model.clone(),
                provider: route.provider.clone(),
                reason: format!("selected configured {phase:?} route"),
                confidence: Some(0.85),
                fallback_model: Some(self.policy.default_model.clone()),
            };
        }
        ModelRouteDecision {
            phase,
            model: self.policy.default_model.clone(),
            provider: None,
            reason: "fell back to default model route".to_string(),
            confidence: Some(0.55),
            fallback_model: None,
        }
    }

    #[must_use]
    pub fn select_with_feedback(
        &self,
        phase: ModelRoutePhase,
        feedback: &[ModelRouteFeedback],
    ) -> ModelRouteDecision {
        let mut decision = self.select(phase);
        let relevant = feedback
            .iter()
            .filter(|entry| {
                entry.route.phase == phase
                    && entry.route.model == decision.model
                    && entry.succeeded.is_some()
            })
            .collect::<Vec<_>>();
        let failed = relevant
            .iter()
            .filter(|entry| entry.succeeded == Some(false) || entry.recovery_triggered)
            .count();
        let total = relevant.len();
        if total > 0 {
            let failure_rate = failed as f32 / total as f32;
            decision.confidence = Some((1.0 - failure_rate).clamp(0.2, 0.95));
            if failure_rate >= 0.5 {
                decision.reason = format!(
                    "{}; route feedback shows {:.0}% recent failure/recovery rate",
                    decision.reason,
                    failure_rate * 100.0
                );
                decision.fallback_model = Some(self.policy.default_model.clone());
            }
        }
        decision
    }
    #[must_use]
    pub fn policy(&self) -> &MoERoutingPolicy {
        &self.policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_configured_phase_route() {
        let policy = MoERoutingPolicy::new(
            "sonnet",
            vec![ModelRoute::new(ModelRoutePhase::Verification, "opus")],
        );
        let router = ModelRouter::new(policy);
        let decision = router.select(ModelRoutePhase::Verification);
        assert_eq!(decision.model, "opus");
    }

    #[test]
    fn feedback_reduces_confidence_for_failing_route() {
        let router = ModelRouter::new(MoERoutingPolicy::balanced("sonnet"));
        let route = router.select(ModelRoutePhase::Verification);
        let feedback = vec![
            ModelRouteFeedback::pending("task-1", route.clone(), 1).with_outcome(
                false,
                Some(false),
                true,
                Some("failed".to_string()),
            ),
            ModelRouteFeedback::pending("task-2", route, 2).with_outcome(
                false,
                Some(false),
                true,
                Some("failed".to_string()),
            ),
        ];
        let decision = router.select_with_feedback(ModelRoutePhase::Verification, &feedback);

        assert!(decision.reason.contains("route feedback"));
        assert!(decision
            .confidence
            .is_some_and(|confidence| confidence <= 0.5));
    }
}
