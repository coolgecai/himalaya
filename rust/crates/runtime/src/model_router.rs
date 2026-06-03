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
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
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
            input_tokens: None,
            output_tokens: None,
            cost_usd: None,
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

    #[must_use]
    pub fn with_metrics(
        mut self,
        latency_ms: Option<u32>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cost_usd: Option<f64>,
    ) -> Self {
        self.latency_ms = latency_ms;
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
        self.cost_usd = cost_usd;
        self
    }

    pub fn merge_observations(&mut self, observation: &Self) {
        if observation.succeeded.is_some() {
            self.succeeded = observation.succeeded;
        }
        if observation.latency_ms.is_some() {
            self.latency_ms = observation.latency_ms;
        }
        if observation.input_tokens.is_some() {
            self.input_tokens = observation.input_tokens;
        }
        if observation.output_tokens.is_some() {
            self.output_tokens = observation.output_tokens;
        }
        if observation.cost_usd.is_some() {
            self.cost_usd = observation.cost_usd;
        }
        if observation.verification_passed.is_some() {
            self.verification_passed = observation.verification_passed;
        }
        self.recovery_triggered |= observation.recovery_triggered;
        if observation.note.is_some() {
            self.note = observation.note.clone();
        }
    }

    #[must_use]
    pub fn total_tokens(&self) -> Option<u64> {
        match (self.input_tokens, self.output_tokens) {
            (Some(input), Some(output)) => Some(input.saturating_add(output)),
            (Some(input), None) => Some(input),
            (None, Some(output)) => Some(output),
            (None, None) => None,
        }
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
                let stats = RouteFeedbackStats::for_route(
                    phase,
                    route.provider.as_deref(),
                    &route.model,
                    feedback,
                );
                (route, stats, adaptive_route_score(route, &stats))
            })
            .collect::<Vec<_>>();
        let primary_stats = RouteFeedbackStats::for_route(
            phase,
            primary.provider.as_deref(),
            &primary.model,
            feedback,
        );
        let primary_failure_rate = primary_stats.failure_rate();
        let threshold = f32::from(self.policy.switch_failure_threshold_percent) / 100.0;
        let should_switch = primary_stats.total >= self.policy.min_feedback_samples
            && primary_failure_rate >= threshold
            && scored.len() > 1;
        if should_switch {
            if let Some((route, stats, _score)) = scored
                .iter()
                .filter(|(route, _, _)| {
                    route.model != primary.model || route.provider != primary.provider
                })
                .max_by(|left, right| left.2.total_cmp(&right.2))
            {
                let mut decision = self.decision_for_route(
                    phase,
                    route,
                    &format!(
                        "adaptive route selected from route feedback after {:.0}% failure/recovery rate for {}/{}",
                        primary_failure_rate * 100.0,
                        primary.provider.as_deref().unwrap_or("default"),
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
                "{}; route feedback for {}/{} shows {:.0}% recent failure/recovery rate",
                decision.reason,
                primary.provider.as_deref().unwrap_or("default"),
                primary.model,
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
    avg_latency_ms: Option<f32>,
    avg_tokens: Option<f32>,
    avg_cost_usd: Option<f32>,
}

impl RouteFeedbackStats {
    fn for_route(
        phase: ModelRoutePhase,
        provider: Option<&str>,
        model: &str,
        feedback: &[ModelRouteFeedback],
    ) -> Self {
        let related = feedback
            .iter()
            .filter(|entry| {
                entry.route.phase == phase
                    && entry.route.model == model
                    && entry.route.provider.as_deref() == provider
                    && entry.succeeded.is_some()
            })
            .collect::<Vec<_>>();
        let failures = related
            .iter()
            .filter(|entry| entry.succeeded == Some(false) || entry.recovery_triggered)
            .count();
        let avg_latency_ms = average_metric(
            related
                .iter()
                .filter_map(|entry| entry.latency_ms.map(|value| value as f32)),
        );
        let avg_tokens = average_metric(
            related
                .iter()
                .filter_map(|entry| entry.total_tokens().map(|value| value as f32)),
        );
        let avg_cost_usd = average_metric(
            related
                .iter()
                .filter_map(|entry| entry.cost_usd.map(|value| value as f32)),
        );
        Self {
            total: related.len(),
            failures,
            avg_latency_ms,
            avg_tokens,
            avg_cost_usd,
        }
    }

    fn failure_rate(self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.failures as f32 / self.total as f32
        }
    }

    fn efficiency_factor(self, route: &ModelRoute) -> f32 {
        let latency_factor = self.avg_latency_ms.map_or(1.0, |latency| {
            metric_penalty(latency, 1_000.0, route.latency_weight)
        });
        let token_factor = self.avg_tokens.map_or(1.0, |tokens| {
            metric_penalty(tokens, 4_000.0, route.cost_weight)
        });
        let cost_factor = self
            .avg_cost_usd
            .map_or(1.0, |cost| metric_penalty(cost, 0.05, route.cost_weight));
        (latency_factor * token_factor * cost_factor).clamp(0.25, 1.25)
    }
}

fn route_weight_score(route: &ModelRoute) -> f32 {
    f32::from(route.quality_weight) * 2.0
        + f32::from(route.latency_weight)
        + f32::from(route.cost_weight)
}

fn average_metric(values: impl Iterator<Item = f32>) -> Option<f32> {
    let mut total = 0.0;
    let mut count = 0_u32;
    for value in values {
        total += value;
        count = count.saturating_add(1);
    }
    (count > 0).then_some(total / count as f32)
}

fn metric_penalty(value: f32, target: f32, weight: u8) -> f32 {
    let weight = f32::from(weight).clamp(1.0, 5.0) / 5.0;
    let over_target = (value / target).max(1.0) - 1.0;
    (1.0 / (1.0 + over_target * weight)).clamp(0.35, 1.0)
}

fn adaptive_route_score(route: &ModelRoute, stats: &RouteFeedbackStats) -> f32 {
    route_weight_score(route)
        * (1.0 - stats.failure_rate()).clamp(0.1, 1.0)
        * stats.efficiency_factor(route)
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

    #[test]
    fn adaptive_feedback_keeps_provider_routes_separate() {
        let policy = MoERoutingPolicy::new(
            "sonnet",
            vec![
                ModelRoute::new(ModelRoutePhase::Verification, "shared")
                    .with_provider("anthropic")
                    .with_weights(1, 1, 5),
                ModelRoute::new(ModelRoutePhase::Verification, "shared")
                    .with_provider("openai")
                    .with_weights(1, 1, 4),
            ],
        )
        .with_adaptive(true, 2, 50);
        let router = ModelRouter::new(policy);
        let primary = router.select(ModelRoutePhase::Verification);
        assert_eq!(primary.provider.as_deref(), Some("anthropic"));
        let failed_primary = ModelRouteDecision {
            provider: Some("anthropic".to_string()),
            ..primary.clone()
        };
        let healthy_same_model_other_provider = ModelRouteDecision {
            provider: Some("openai".to_string()),
            ..primary.clone()
        };
        let feedback = vec![
            ModelRouteFeedback::pending("task-1", failed_primary.clone(), 1).with_outcome(
                false,
                Some(false),
                true,
                Some("failed".to_string()),
            ),
            ModelRouteFeedback::pending("task-2", failed_primary, 2).with_outcome(
                false,
                Some(false),
                true,
                Some("failed".to_string()),
            ),
            ModelRouteFeedback::pending("task-3", healthy_same_model_other_provider, 3)
                .with_outcome(true, Some(true), false, None),
        ];

        let decision = router.select_with_feedback(ModelRoutePhase::Verification, &feedback);

        assert_eq!(decision.model, "shared");
        assert_eq!(decision.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn adaptive_score_penalizes_expensive_slow_routes() {
        let fast_route = ModelRoute::new(ModelRoutePhase::Coding, "fast").with_weights(5, 5, 3);
        let slow_route = ModelRoute::new(ModelRoutePhase::Coding, "slow").with_weights(5, 5, 3);
        let fast_decision = ModelRouteDecision {
            phase: ModelRoutePhase::Coding,
            model: "fast".to_string(),
            provider: None,
            reason: "test".to_string(),
            confidence: None,
            fallback_model: None,
        };
        let slow_decision = ModelRouteDecision {
            phase: ModelRoutePhase::Coding,
            model: "slow".to_string(),
            provider: None,
            reason: "test".to_string(),
            confidence: None,
            fallback_model: None,
        };
        let feedback = vec![
            ModelRouteFeedback::pending("task-1", fast_decision, 1)
                .with_metrics(Some(500), Some(800), Some(200), Some(0.01))
                .with_outcome(true, Some(true), false, None),
            ModelRouteFeedback::pending("task-2", slow_decision, 2)
                .with_metrics(Some(5_000), Some(8_000), Some(4_000), Some(0.25))
                .with_outcome(true, Some(true), false, None),
        ];
        let fast_stats =
            RouteFeedbackStats::for_route(ModelRoutePhase::Coding, None, "fast", &feedback);
        let slow_stats =
            RouteFeedbackStats::for_route(ModelRoutePhase::Coding, None, "slow", &feedback);

        assert!(
            adaptive_route_score(&fast_route, &fast_stats)
                > adaptive_route_score(&slow_route, &slow_stats)
        );
    }
}
