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
    #[serde(default = "default_adaptive_routing")]
    pub adaptive: bool,
    #[serde(default = "default_min_feedback_samples")]
    pub min_feedback_samples: usize,
    #[serde(default = "default_switch_failure_threshold_percent")]
    pub switch_failure_threshold_percent: u8,
}

impl MoERoutingPolicy {
    #[must_use]
    pub fn new(default_model: impl Into<String>, routes: Vec<ModelRoute>) -> Self {
        Self {
            default_model: default_model.into(),
            routes,
            adaptive: default_adaptive_routing(),
            min_feedback_samples: default_min_feedback_samples(),
            switch_failure_threshold_percent: default_switch_failure_threshold_percent(),
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
            adaptive: default_adaptive_routing(),
            min_feedback_samples: default_min_feedback_samples(),
            switch_failure_threshold_percent: default_switch_failure_threshold_percent(),
        }
    }

    #[must_use]
    pub fn with_adaptive(
        mut self,
        adaptive: bool,
        min_feedback_samples: usize,
        switch_failure_threshold_percent: u8,
    ) -> Self {
        self.adaptive = adaptive;
        self.min_feedback_samples = min_feedback_samples.max(1);
        self.switch_failure_threshold_percent = switch_failure_threshold_percent.min(100);
        self
    }
}

fn default_adaptive_routing() -> bool {
    true
}

fn default_min_feedback_samples() -> usize {
    2
}

fn default_switch_failure_threshold_percent() -> u8 {
    50
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
        let Some(route) = self.best_configured_route(phase) else {
            return ModelRouteDecision {
                phase,
                model: self.policy.default_model.clone(),
                provider: None,
                reason: "fell back to default model route".to_string(),
                confidence: Some(0.55),
                fallback_model: None,
            };
        };
        self.decision_for_route(phase, route, "selected configured route", Some(0.85))
    }

    #[must_use]
    pub fn select_with_feedback(
        &self,
        phase: ModelRoutePhase,
        feedback: &[ModelRouteFeedback],
    ) -> ModelRouteDecision {
        let Some(primary) = self.best_configured_route(phase) else {
            return self.select(phase);
        };
        if !self.policy.adaptive {
            return self.decision_for_route(
                phase,
                primary,
                "selected configured route",
                Some(0.85),
            );
        }

        let candidates = self.routes_for_phase(phase);
        let scored = candidates
            .iter()
            .map(|route| {
                let stats = RouteFeedbackStats::for_route(phase, &route.model, feedback);
                (route, stats, adaptive_route_score(route, &stats))
            })
            .collect::<Vec<_>>();
        let primary_stats = RouteFeedbackStats::for_route(phase, &primary.model, feedback);
        let primary_failure_rate = primary_stats.failure_rate();
        let threshold = f32::from(self.policy.switch_failure_threshold_percent) / 100.0;
        let should_switch = primary_stats.total >= self.policy.min_feedback_samples
            && primary_failure_rate >= threshold
            && scored.len() > 1;
        if should_switch {
            if let Some((route, stats, _score)) = scored
                .iter()
                .filter(|(route, _, _)| route.model != primary.model)
                .max_by(|left, right| left.2.total_cmp(&right.2))
            {
                let mut decision = self.decision_for_route(
                    phase,
                    route,
                    &format!(
                        "adaptive route selected from route feedback after {:.0}% failure/recovery rate for {}",
                        primary_failure_rate * 100.0,
                        primary.model
                    ),
                    Some((1.0 - stats.failure_rate()).clamp(0.35, 0.95)),
                );
                decision.fallback_model = Some(primary.model.clone());
                return decision;
            }
        }

        let mut decision = self.decision_for_route(
            phase,
            primary,
            "selected configured route",
            Some((1.0 - primary_failure_rate).clamp(0.2, 0.95)),
        );
        if primary_stats.total > 0 && primary_failure_rate >= threshold {
            decision.reason = format!(
                "{}; route feedback shows {:.0}% recent failure/recovery rate",
                decision.reason,
                primary_failure_rate * 100.0
            );
        }
        decision
    }

    #[must_use]
    pub fn policy(&self) -> &MoERoutingPolicy {
        &self.policy
    }

    fn best_configured_route(&self, phase: ModelRoutePhase) -> Option<&ModelRoute> {
        self.routes_for_phase(phase)
            .into_iter()
            .max_by(|left, right| route_weight_score(left).total_cmp(&route_weight_score(right)))
    }

    fn routes_for_phase(&self, phase: ModelRoutePhase) -> Vec<&ModelRoute> {
        self.policy
            .routes
            .iter()
            .filter(|route| route.role == phase)
            .collect()
    }

    fn decision_for_route(
        &self,
        phase: ModelRoutePhase,
        route: &ModelRoute,
        reason: &str,
        confidence: Option<f32>,
    ) -> ModelRouteDecision {
        ModelRouteDecision {
            phase,
            model: route.model.clone(),
            provider: route.provider.clone(),
            reason: format!("{reason}: {phase:?}"),
            confidence,
            fallback_model: Some(self.policy.default_model.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RouteFeedbackStats {
    total: usize,
    failures: usize,
}

impl RouteFeedbackStats {
    fn for_route(phase: ModelRoutePhase, model: &str, feedback: &[ModelRouteFeedback]) -> Self {
        let related = feedback
            .iter()
            .filter(|entry| {
                entry.route.phase == phase
                    && entry.route.model == model
                    && entry.succeeded.is_some()
            })
            .collect::<Vec<_>>();
        let failures = related
            .iter()
            .filter(|entry| entry.succeeded == Some(false) || entry.recovery_triggered)
            .count();
        Self {
            total: related.len(),
            failures,
        }
    }

    fn failure_rate(self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.failures as f32 / self.total as f32
        }
    }
}

fn route_weight_score(route: &ModelRoute) -> f32 {
    f32::from(route.quality_weight) * 2.0
        + f32::from(route.latency_weight)
        + f32::from(route.cost_weight)
}

fn adaptive_route_score(route: &ModelRoute, stats: &RouteFeedbackStats) -> f32 {
    route_weight_score(route) * (1.0 - stats.failure_rate()).clamp(0.1, 1.0)
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
    fn adaptive_feedback_switches_to_alternate_route() {
        let policy = MoERoutingPolicy::new(
            "sonnet",
            vec![
                ModelRoute::new(ModelRoutePhase::Verification, "sonnet").with_weights(1, 1, 5),
                ModelRoute::new(ModelRoutePhase::Verification, "opus").with_weights(1, 1, 4),
            ],
        )
        .with_adaptive(true, 2, 50);
        let router = ModelRouter::new(policy);
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

        assert_eq!(decision.model, "opus");
        assert_eq!(decision.fallback_model, Some("sonnet".to_string()));
        assert!(decision.reason.contains("adaptive route selected"));
        assert!(decision.reason.contains("route feedback"));
    }
}
