use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{ModelRouteFeedback, ModelRoutePhase};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteFeedbackSnapshot {
    pub feedback: Vec<ModelRouteFeedback>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteFeedbackSummary {
    pub phase: ModelRoutePhase,
    pub model: String,
    pub total: usize,
    pub failures: usize,
    pub recovery_triggered: usize,
    pub success_rate: f32,
    pub avg_latency_ms: Option<f32>,
    pub avg_tokens: Option<f32>,
    pub avg_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct RouteFeedbackStore {
    feedback: Vec<ModelRouteFeedback>,
}

impl RouteFeedbackStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_feedback(feedback: Vec<ModelRouteFeedback>) -> Self {
        Self { feedback }
    }

    pub fn load_from_dir(dir: &Path) -> io::Result<Self> {
        let path = dir.join("feedback.json");
        if !path.exists() {
            return Ok(Self::new());
        }
        let contents = fs::read_to_string(path)?;
        let snapshot = serde_json::from_str::<RouteFeedbackSnapshot>(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Self::from_feedback(snapshot.feedback))
    }

    pub fn save_to_dir(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(&RouteFeedbackSnapshot {
            feedback: self.feedback.clone(),
        })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(dir.join("feedback.json"), format!("{json}\n"))
    }

    pub fn record(&mut self, feedback: ModelRouteFeedback) {
        let exists = self.feedback.iter().any(|entry| {
            entry.task_id == feedback.task_id
                && entry.timestamp == feedback.timestamp
                && entry.route.phase == feedback.route.phase
                && entry.route.model == feedback.route.model
                && entry.route.provider == feedback.route.provider
                && entry.succeeded == feedback.succeeded
                && entry.latency_ms == feedback.latency_ms
                && entry.input_tokens == feedback.input_tokens
                && entry.output_tokens == feedback.output_tokens
                && entry.cost_usd == feedback.cost_usd
                && entry.verification_passed == feedback.verification_passed
                && entry.recovery_triggered == feedback.recovery_triggered
                && entry.note == feedback.note
        });
        if !exists {
            self.feedback.push(feedback);
        }
    }

    #[must_use]
    pub fn feedback(&self) -> &[ModelRouteFeedback] {
        &self.feedback
    }

    #[must_use]
    pub fn summaries(&self) -> Vec<RouteFeedbackSummary> {
        let mut summaries = Vec::new();
        for entry in &self.feedback {
            if summaries.iter().any(|summary: &RouteFeedbackSummary| {
                summary.phase == entry.route.phase && summary.model == entry.route.model
            }) {
                continue;
            }
            let related = self
                .feedback
                .iter()
                .filter(|candidate| {
                    candidate.route.phase == entry.route.phase
                        && candidate.route.model == entry.route.model
                })
                .collect::<Vec<_>>();
            let total = related.len();
            let failures = related
                .iter()
                .filter(|candidate| candidate.succeeded == Some(false))
                .count();
            let recovery_triggered = related
                .iter()
                .filter(|candidate| candidate.recovery_triggered)
                .count();
            let avg_latency_ms = average_f32(
                related
                    .iter()
                    .filter_map(|candidate| candidate.latency_ms.map(|value| value as f32)),
            );
            let avg_tokens = average_f32(
                related
                    .iter()
                    .filter_map(|candidate| candidate.total_tokens().map(|value| value as f32)),
            );
            let avg_cost_usd =
                average_f64(related.iter().filter_map(|candidate| candidate.cost_usd));
            summaries.push(RouteFeedbackSummary {
                phase: entry.route.phase,
                model: entry.route.model.clone(),
                total,
                failures,
                recovery_triggered,
                success_rate: if total == 0 {
                    0.0
                } else {
                    (total - failures) as f32 / total as f32
                },
                avg_latency_ms,
                avg_tokens,
                avg_cost_usd,
            });
        }
        summaries
    }
}

fn average_f32(values: impl Iterator<Item = f32>) -> Option<f32> {
    let mut total = 0.0;
    let mut count = 0_u32;
    for value in values {
        total += value;
        count = count.saturating_add(1);
    }
    (count > 0).then_some(total / count as f32)
}

fn average_f64(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut total = 0.0;
    let mut count = 0_u32;
    for value in values {
        total += value;
        count = count.saturating_add(1);
    }
    (count > 0).then_some(total / f64::from(count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelRouteDecision, ModelRoutePhase};

    #[test]
    fn record_deduplicates_replayed_task_feedback() {
        let route = ModelRouteDecision {
            phase: ModelRoutePhase::Verification,
            model: "opus".to_string(),
            provider: Some("anthropic".to_string()),
            reason: "test".to_string(),
            confidence: Some(0.8),
            fallback_model: Some("sonnet".to_string()),
        };
        let feedback = ModelRouteFeedback::pending("task-1", route, 7)
            .with_metrics(Some(900), Some(1_000), Some(200), Some(0.02))
            .with_outcome(false, Some(false), true, Some("failed".to_string()));
        let mut store = RouteFeedbackStore::new();
        store.record(feedback.clone());
        store.record(feedback);

        assert_eq!(store.feedback().len(), 1);
    }

    #[test]
    fn saves_and_loads_feedback_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "Himalaya-route-feedback-store-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let route = ModelRouteDecision {
            phase: ModelRoutePhase::Verification,
            model: "sonnet".to_string(),
            provider: None,
            reason: "test".to_string(),
            confidence: Some(0.7),
            fallback_model: Some("opus".to_string()),
        };
        let mut store = RouteFeedbackStore::new();
        store.record(
            ModelRouteFeedback::pending("task-1", route, 11).with_outcome(
                false,
                Some(false),
                true,
                Some("verification failed".to_string()),
            ),
        );

        store.save_to_dir(&dir).expect("store should save");
        let loaded = RouteFeedbackStore::load_from_dir(&dir).expect("store should load");

        assert_eq!(loaded.feedback(), store.feedback());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn summarizes_feedback_by_phase_and_model() {
        let route = ModelRouteDecision {
            phase: ModelRoutePhase::Coding,
            model: "sonnet".to_string(),
            provider: None,
            reason: "test".to_string(),
            confidence: Some(0.8),
            fallback_model: None,
        };
        let store = RouteFeedbackStore::from_feedback(vec![
            ModelRouteFeedback::pending("task-1", route.clone(), 1)
                .with_metrics(Some(1_000), Some(1_500), Some(500), Some(0.01))
                .with_outcome(true, None, false, None),
            ModelRouteFeedback::pending("task-2", route, 2)
                .with_metrics(Some(3_000), Some(2_500), Some(1_500), Some(0.03))
                .with_outcome(false, None, true, None),
        ]);
        let summary = store.summaries().pop().expect("summary");

        assert_eq!(summary.total, 2);
        assert_eq!(summary.failures, 1);
        assert_eq!(summary.recovery_triggered, 1);
        assert_eq!(summary.avg_latency_ms, Some(2_000.0));
        assert_eq!(summary.avg_tokens, Some(3_000.0));
        assert_eq!(summary.avg_cost_usd, Some(0.02));
    }
}
