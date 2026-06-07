use serde::{Deserialize, Serialize};

use crate::{
    MoERoutingPolicy, ModelRouteDecision, ModelRouteFeedback, ModelRoutePhase, ModelRouter,
    RouteFeedbackStore, RouteFeedbackSummary,
};

pub const ROUTING_OPTIMIZER_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingHealthStatus {
    Stable,
    Degraded,
    NeedsFallback,
    InsufficientData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPolicyCandidateKind {
    ObserveMore,
    LowerConfidence,
    RecommendFallback,
    PreferQuality,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingHealthEntry {
    pub phase: ModelRoutePhase,
    pub provider: Option<String>,
    pub model: String,
    pub total: usize,
    pub failures: usize,
    pub recovery_triggered: usize,
    pub success_rate: f32,
    pub failure_rate: f32,
    pub health: RoutingHealthStatus,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingPolicyCandidate {
    pub phase: ModelRoutePhase,
    pub provider: Option<String>,
    pub model: String,
    pub kind: RoutingPolicyCandidateKind,
    pub fallback_model: Option<String>,
    pub confidence_delta: f32,
    pub estimated_success_delta: f32,
    pub recovery_reduction_estimate: f32,
    pub cost_latency_tradeoff: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingEvaluationReport {
    pub version: u32,
    pub feedback_count: usize,
    pub min_samples: usize,
    pub switch_failure_threshold: f32,
    pub health: Vec<RoutingHealthEntry>,
    pub candidates: Vec<RoutingPolicyCandidate>,
    pub summaries: Vec<RouteFeedbackSummary>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingReplayChange {
    pub phase: ModelRoutePhase,
    pub current_model: String,
    pub current_provider: Option<String>,
    pub candidate_model: String,
    pub candidate_provider: Option<String>,
    pub fallback_model: Option<String>,
    pub estimated_success_delta: f32,
    pub recovery_reduction_estimate: f32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingOptimizerReplayReport {
    pub version: u32,
    pub feedback_count: usize,
    pub evaluated_phases: Vec<ModelRoutePhase>,
    pub current_decisions: Vec<ModelRouteDecision>,
    pub changed_routes: Vec<RoutingReplayChange>,
    pub estimated_success_delta: f32,
    pub recovery_reduction_estimate: f32,
    pub cost_latency_tradeoff: String,
    pub recommendations: Vec<String>,
}

pub fn evaluate_routing_feedback(
    store: &RouteFeedbackStore,
    min_samples: usize,
    switch_failure_threshold: f32,
) -> RoutingEvaluationReport {
    let min_samples = min_samples.max(1);
    let switch_failure_threshold = switch_failure_threshold.clamp(0.0, 1.0);
    let summaries = store.summaries();
    let mut health = Vec::new();
    for summary in &summaries {
        health.push(health_entry(summary, min_samples, switch_failure_threshold));
    }
    let candidates = policy_candidates(&health, &summaries);
    let recommendations = evaluation_recommendations(store.feedback().len(), &health, &candidates);

    RoutingEvaluationReport {
        version: ROUTING_OPTIMIZER_VERSION,
        feedback_count: store.feedback().len(),
        min_samples,
        switch_failure_threshold,
        health,
        candidates,
        summaries,
        recommendations,
    }
}

pub fn replay_routing_optimizer(
    policy: MoERoutingPolicy,
    store: &RouteFeedbackStore,
    min_samples: usize,
    switch_failure_threshold: f32,
) -> RoutingOptimizerReplayReport {
    let evaluation = evaluate_routing_feedback(store, min_samples, switch_failure_threshold);
    let router = ModelRouter::new(policy);
    let phases = observed_phases(store.feedback());
    let mut current_decisions = Vec::new();
    let mut changed_routes = Vec::new();
    for phase in &phases {
        let current = router.select_with_feedback(*phase, store.feedback());
        for candidate in evaluation
            .candidates
            .iter()
            .filter(|candidate| candidate.phase == *phase)
            .filter(|candidate| {
                matches!(
                    candidate.kind,
                    RoutingPolicyCandidateKind::RecommendFallback
                        | RoutingPolicyCandidateKind::PreferQuality
                )
            })
        {
            let candidate_model = candidate
                .fallback_model
                .clone()
                .unwrap_or_else(|| candidate.model.clone());
            let candidate_provider = if candidate.fallback_model.is_some() {
                None
            } else {
                candidate.provider.clone()
            };
            let changed =
                current.model != candidate_model || current.provider != candidate_provider;
            if changed {
                changed_routes.push(RoutingReplayChange {
                    phase: *phase,
                    current_model: current.model.clone(),
                    current_provider: current.provider.clone(),
                    candidate_model,
                    candidate_provider,
                    fallback_model: candidate.fallback_model.clone(),
                    estimated_success_delta: candidate.estimated_success_delta,
                    recovery_reduction_estimate: candidate.recovery_reduction_estimate,
                    reason: candidate.reason.clone(),
                });
            }
        }
        current_decisions.push(current);
    }
    dedupe_replay_changes(&mut changed_routes);
    let estimated_success_delta = average_delta(
        changed_routes
            .iter()
            .map(|change| change.estimated_success_delta),
    );
    let recovery_reduction_estimate = average_delta(
        changed_routes
            .iter()
            .map(|change| change.recovery_reduction_estimate),
    );
    let cost_latency_tradeoff = if changed_routes.is_empty() {
        "no route changes estimated".to_string()
    } else if changed_routes
        .iter()
        .any(|change| change.reason.contains("quality"))
    {
        "quality-biased fallback may increase cost or latency".to_string()
    } else {
        "fallback recommendation is based on observed route health".to_string()
    };
    let recommendations = replay_recommendations(&changed_routes);

    RoutingOptimizerReplayReport {
        version: ROUTING_OPTIMIZER_VERSION,
        feedback_count: store.feedback().len(),
        evaluated_phases: phases,
        current_decisions,
        changed_routes,
        estimated_success_delta,
        recovery_reduction_estimate,
        cost_latency_tradeoff,
        recommendations,
    }
}

fn health_entry(
    summary: &RouteFeedbackSummary,
    min_samples: usize,
    switch_failure_threshold: f32,
) -> RoutingHealthEntry {
    let failure_rate = if summary.total == 0 {
        0.0
    } else {
        summary.failures.max(summary.recovery_triggered) as f32 / summary.total as f32
    };
    let health = if summary.total < min_samples {
        RoutingHealthStatus::InsufficientData
    } else if failure_rate >= switch_failure_threshold {
        RoutingHealthStatus::NeedsFallback
    } else if summary.recovery_triggered > 0 || failure_rate >= switch_failure_threshold * 0.5 {
        RoutingHealthStatus::Degraded
    } else {
        RoutingHealthStatus::Stable
    };
    let reason = match health {
        RoutingHealthStatus::Stable => "route feedback is stable".to_string(),
        RoutingHealthStatus::Degraded => format!(
            "route has {:.0}% failure/recovery pressure",
            failure_rate * 100.0
        ),
        RoutingHealthStatus::NeedsFallback => format!(
            "route exceeds {:.0}% failure/recovery threshold",
            switch_failure_threshold * 100.0
        ),
        RoutingHealthStatus::InsufficientData => format!(
            "route has {} sample(s), below min_samples={min_samples}",
            summary.total
        ),
    };

    RoutingHealthEntry {
        phase: summary.phase,
        provider: summary.provider.clone(),
        model: summary.model.clone(),
        total: summary.total,
        failures: summary.failures,
        recovery_triggered: summary.recovery_triggered,
        success_rate: summary.success_rate,
        failure_rate,
        health,
        reason,
    }
}

fn policy_candidates(
    health: &[RoutingHealthEntry],
    summaries: &[RouteFeedbackSummary],
) -> Vec<RoutingPolicyCandidate> {
    let mut candidates = Vec::new();
    for entry in health {
        match entry.health {
            RoutingHealthStatus::Stable => {}
            RoutingHealthStatus::InsufficientData => {
                candidates.push(candidate_for_entry(
                    entry,
                    RoutingPolicyCandidateKind::ObserveMore,
                    None,
                    0.0,
                    0.0,
                    "insufficient route feedback; keep observing before changing policy",
                ));
            }
            RoutingHealthStatus::Degraded => {
                candidates.push(candidate_for_entry(
                    entry,
                    RoutingPolicyCandidateKind::LowerConfidence,
                    best_fallback(entry, summaries),
                    0.0 - (entry.failure_rate * 0.25).clamp(0.05, 0.25),
                    entry.failure_rate * 0.5,
                    "route is degraded; lower confidence and keep fallback visible",
                ));
            }
            RoutingHealthStatus::NeedsFallback => {
                let fallback = best_fallback(entry, summaries);
                candidates.push(candidate_for_entry(
                    entry,
                    RoutingPolicyCandidateKind::RecommendFallback,
                    fallback.clone(),
                    0.0 - (entry.failure_rate * 0.4).clamp(0.10, 0.40),
                    entry.failure_rate,
                    "route needs fallback based on observed failures",
                ));
                if matches!(
                    entry.phase,
                    ModelRoutePhase::Coding | ModelRoutePhase::Verification
                ) {
                    candidates.push(candidate_for_entry(
                        entry,
                        RoutingPolicyCandidateKind::PreferQuality,
                        fallback,
                        0.0 - (entry.failure_rate * 0.2).clamp(0.05, 0.20),
                        entry.failure_rate * 0.75,
                        "quality-sensitive phase should prefer a healthier fallback",
                    ));
                }
            }
        }
    }
    candidates
}

fn candidate_for_entry(
    entry: &RoutingHealthEntry,
    kind: RoutingPolicyCandidateKind,
    fallback_model: Option<String>,
    confidence_delta: f32,
    estimated_success_delta: f32,
    reason: &str,
) -> RoutingPolicyCandidate {
    RoutingPolicyCandidate {
        phase: entry.phase,
        provider: entry.provider.clone(),
        model: entry.model.clone(),
        kind,
        fallback_model,
        confidence_delta,
        estimated_success_delta: estimated_success_delta.clamp(0.0, 1.0),
        recovery_reduction_estimate: entry.failure_rate.clamp(0.0, 1.0),
        cost_latency_tradeoff: match kind {
            RoutingPolicyCandidateKind::PreferQuality => {
                "may trade cost/latency for higher quality".to_string()
            }
            RoutingPolicyCandidateKind::RecommendFallback => {
                "fallback may shift cost/latency depending on configured model".to_string()
            }
            RoutingPolicyCandidateKind::LowerConfidence => {
                "keeps current route but makes fallback more likely".to_string()
            }
            RoutingPolicyCandidateKind::ObserveMore => {
                "no cost or latency change recommended".to_string()
            }
        },
        reason: reason.to_string(),
    }
}

fn best_fallback(entry: &RoutingHealthEntry, summaries: &[RouteFeedbackSummary]) -> Option<String> {
    summaries
        .iter()
        .filter(|summary| summary.phase == entry.phase)
        .filter(|summary| summary.model != entry.model || summary.provider != entry.provider)
        .max_by(|left, right| {
            route_summary_score(left)
                .total_cmp(&route_summary_score(right))
                .then_with(|| left.model.cmp(&right.model))
        })
        .map(|summary| summary.model.clone())
}

fn route_summary_score(summary: &RouteFeedbackSummary) -> f32 {
    let recovery_penalty = if summary.total == 0 {
        0.0
    } else {
        summary.recovery_triggered as f32 / summary.total as f32
    };
    (summary.success_rate - recovery_penalty * 0.25).clamp(0.0, 1.0)
}

fn observed_phases(feedback: &[ModelRouteFeedback]) -> Vec<ModelRoutePhase> {
    let mut phases = feedback
        .iter()
        .map(|entry| entry.route.phase)
        .collect::<Vec<_>>();
    phases.sort();
    phases.dedup();
    phases
}

fn dedupe_replay_changes(changes: &mut Vec<RoutingReplayChange>) {
    let mut deduped = Vec::new();
    for change in changes.drain(..) {
        if deduped.iter().any(|existing: &RoutingReplayChange| {
            existing.phase == change.phase
                && existing.candidate_model == change.candidate_model
                && existing.candidate_provider == change.candidate_provider
        }) {
            continue;
        }
        deduped.push(change);
    }
    *changes = deduped;
}

fn average_delta(values: impl Iterator<Item = f32>) -> f32 {
    let mut total = 0.0;
    let mut count = 0_usize;
    for value in values {
        total += value;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        (total / count as f32).clamp(0.0, 1.0)
    }
}

fn evaluation_recommendations(
    feedback_count: usize,
    health: &[RoutingHealthEntry],
    candidates: &[RoutingPolicyCandidate],
) -> Vec<String> {
    let mut recommendations = Vec::new();
    if feedback_count == 0 {
        recommendations.push(
            "No route feedback is available; run tasks before optimizing MoE routing.".to_string(),
        );
    }
    let needs_fallback = health
        .iter()
        .filter(|entry| entry.health == RoutingHealthStatus::NeedsFallback)
        .count();
    if needs_fallback > 0 {
        recommendations.push(format!(
            "{needs_fallback} route(s) exceed failure/recovery threshold; review fallback candidates."
        ));
    }
    if candidates
        .iter()
        .any(|candidate| candidate.kind == RoutingPolicyCandidateKind::ObserveMore)
    {
        recommendations.push(
            "Some routes have insufficient samples; keep observing before applying policy changes."
                .to_string(),
        );
    }
    if recommendations.is_empty() {
        recommendations.push("Route feedback does not require optimizer intervention.".to_string());
    }
    recommendations
}

fn replay_recommendations(changed_routes: &[RoutingReplayChange]) -> Vec<String> {
    if changed_routes.is_empty() {
        return vec!["Current router decisions match optimizer candidates.".to_string()];
    }
    vec![format!(
        "{} route replay change(s) would be recommended by the optimizer.",
        changed_routes.len()
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(model: &str) -> ModelRouteDecision {
        ModelRouteDecision {
            phase: ModelRoutePhase::Verification,
            model: model.to_string(),
            provider: None,
            reason: "test".to_string(),
            confidence: Some(0.8),
            fallback_model: None,
        }
    }

    #[test]
    fn evaluation_marks_failed_route_as_needing_fallback() {
        let store = RouteFeedbackStore::from_feedback(vec![
            ModelRouteFeedback::pending("task-1", decision("weak"), 1).with_outcome(
                false,
                Some(false),
                true,
                Some("failed".to_string()),
            ),
            ModelRouteFeedback::pending("task-2", decision("weak"), 2).with_outcome(
                false,
                Some(false),
                true,
                Some("failed".to_string()),
            ),
            ModelRouteFeedback::pending("task-3", decision("strong"), 3).with_outcome(
                true,
                Some(true),
                false,
                Some("ok".to_string()),
            ),
            ModelRouteFeedback::pending("task-4", decision("strong"), 4).with_outcome(
                true,
                Some(true),
                false,
                Some("ok".to_string()),
            ),
        ]);

        let report = evaluate_routing_feedback(&store, 2, 0.5);

        assert!(report.health.iter().any(|entry| {
            entry.model == "weak" && entry.health == RoutingHealthStatus::NeedsFallback
        }));
        assert!(report.candidates.iter().any(|candidate| {
            candidate.model == "weak"
                && candidate.kind == RoutingPolicyCandidateKind::RecommendFallback
                && candidate.fallback_model.as_deref() == Some("strong")
        }));
    }

    #[test]
    fn replay_reports_candidate_route_changes() {
        let store = RouteFeedbackStore::from_feedback(vec![
            ModelRouteFeedback::pending("task-1", decision("weak"), 1).with_outcome(
                false,
                Some(false),
                true,
                Some("failed".to_string()),
            ),
            ModelRouteFeedback::pending("task-2", decision("weak"), 2).with_outcome(
                false,
                Some(false),
                true,
                Some("failed".to_string()),
            ),
            ModelRouteFeedback::pending("task-3", decision("strong"), 3).with_outcome(
                true,
                Some(true),
                false,
                Some("ok".to_string()),
            ),
        ]);
        let policy = MoERoutingPolicy::new(
            "weak",
            vec![
                crate::ModelRoute::new(ModelRoutePhase::Verification, "weak").with_weights(1, 1, 5),
                crate::ModelRoute::new(ModelRoutePhase::Verification, "strong")
                    .with_weights(1, 1, 4),
            ],
        )
        .with_adaptive(false, 2, 50);

        let replay = replay_routing_optimizer(policy, &store, 2, 0.5);

        assert!(!replay.changed_routes.is_empty());
        assert_eq!(replay.changed_routes[0].candidate_model, "strong");
        assert!(replay.estimated_success_delta > 0.0);
    }

    #[test]
    fn low_sample_routes_only_recommend_observation() {
        let store = RouteFeedbackStore::from_feedback(vec![ModelRouteFeedback::pending(
            "task-1",
            decision("new"),
            1,
        )
        .with_outcome(false, Some(false), false, Some("failed".to_string()))]);

        let report = evaluate_routing_feedback(&store, 2, 0.5);

        assert_eq!(
            report.health[0].health,
            RoutingHealthStatus::InsufficientData
        );
        assert_eq!(
            report.candidates[0].kind,
            RoutingPolicyCandidateKind::ObserveMore
        );
    }
}
