use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{
    evaluate_routing_feedback, replay_routing_optimizer, MoERoutingPolicy, ModelCapability,
    ModelRoute, ModelRoutePhase, RouteFeedbackStore, RoutingEvaluationReport,
    RoutingOptimizerReplayReport, RoutingPolicyCandidateKind, RoutingReplayChange,
};

pub const ROUTING_POLICY_PROPOSAL_VERSION: u32 = 1;
pub const ROUTING_POLICY_PROPOSALS_FILE: &str = "policy-proposals.json";
pub const APPLIED_ROUTING_POLICY_FILE: &str = "applied-policy.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPolicyProposalStatus {
    Draft,
    Approved,
    Applied,
    Rejected,
    RolledBack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPolicySafetyLevel {
    Info,
    Warning,
    Blocker,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingPolicySafetyGate {
    pub name: String,
    pub passed: bool,
    pub level: RoutingPolicySafetyLevel,
    pub requires_review: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingPolicyProposalChange {
    pub phase: ModelRoutePhase,
    pub current_model: String,
    pub current_provider: Option<String>,
    pub proposed_model: String,
    pub proposed_provider: Option<String>,
    pub fallback_model: Option<String>,
    pub estimated_success_delta: f32,
    pub recovery_reduction_estimate: f32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingPolicyProposal {
    pub version: u32,
    pub id: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub status: RoutingPolicyProposalStatus,
    pub min_samples: usize,
    pub switch_failure_threshold: f32,
    pub feedback_count: usize,
    pub estimated_success_delta: f32,
    pub recovery_reduction_estimate: f32,
    pub gates: Vec<RoutingPolicySafetyGate>,
    pub changes: Vec<RoutingPolicyProposalChange>,
    pub baseline_policy: MoERoutingPolicy,
    pub proposed_policy: MoERoutingPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_policy: Option<MoERoutingPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_back_at: Option<u64>,
    pub evaluation: RoutingEvaluationReport,
    pub replay: RoutingOptimizerReplayReport,
    pub audit: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingPolicyProposalSnapshot {
    pub proposals: Vec<RoutingPolicyProposal>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedRoutingPolicy {
    pub version: u32,
    pub proposal_id: String,
    pub applied_at: u64,
    pub policy: MoERoutingPolicy,
    pub previous_policy: MoERoutingPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_applied: Option<Box<AppliedRoutingPolicy>>,
    pub changes: Vec<RoutingPolicyProposalChange>,
    pub audit: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingPolicyApplyReport {
    pub proposal_id: String,
    pub dry_run: bool,
    pub applied: bool,
    pub status: RoutingPolicyProposalStatus,
    pub policy_path: PathBuf,
    pub changed_routes: Vec<RoutingPolicyProposalChange>,
    pub blockers: Vec<String>,
    pub gates: Vec<RoutingPolicySafetyGate>,
    pub previous_policy: MoERoutingPolicy,
    pub proposed_policy: MoERoutingPolicy,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingPolicyRollbackReport {
    pub proposal_id: String,
    pub rolled_back: bool,
    pub status: RoutingPolicyProposalStatus,
    pub policy_path: PathBuf,
    pub restored_policy: Option<MoERoutingPolicy>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RoutingPolicyProposalStore {
    dir: PathBuf,
}

impl RoutingPolicyProposalStore {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    #[must_use]
    pub fn proposals_path(&self) -> PathBuf {
        routing_policy_proposals_path(&self.dir)
    }

    #[must_use]
    pub fn applied_policy_path(&self) -> PathBuf {
        applied_routing_policy_path(&self.dir)
    }

    pub fn load(&self) -> io::Result<RoutingPolicyProposalSnapshot> {
        load_routing_policy_proposals(&self.dir)
    }

    pub fn list(&self) -> io::Result<Vec<RoutingPolicyProposal>> {
        Ok(self.load()?.proposals)
    }

    pub fn propose(
        &self,
        baseline_policy: MoERoutingPolicy,
        feedback: &RouteFeedbackStore,
        min_samples: usize,
        switch_failure_threshold: f32,
    ) -> io::Result<RoutingPolicyProposal> {
        let mut snapshot = self.load()?;
        let proposal = build_routing_policy_proposal(
            baseline_policy,
            feedback,
            min_samples,
            switch_failure_threshold,
            snapshot.proposals.len().saturating_add(1),
        );
        snapshot.proposals.push(proposal.clone());
        save_routing_policy_proposals(&self.dir, &snapshot)?;
        Ok(proposal)
    }

    pub fn apply(&self, proposal_id: &str, dry_run: bool) -> io::Result<RoutingPolicyApplyReport> {
        let mut snapshot = self.load()?;
        let Some(index) = snapshot
            .proposals
            .iter()
            .position(|proposal| proposal.id == proposal_id)
        else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("routing policy proposal not found: {proposal_id}"),
            ));
        };
        let mut proposal = snapshot.proposals[index].clone();
        let blockers = proposal_apply_blockers(&proposal);
        let current_applied = load_applied_routing_policy(&self.dir)?;
        let previous_policy = current_applied.as_ref().map_or_else(
            || proposal.baseline_policy.clone(),
            |applied| applied.policy.clone(),
        );
        let mut recommendations = Vec::new();
        if blockers.is_empty() {
            recommendations.push(if dry_run {
                "Dry run passed; apply without --dry-run to persist the routing policy overlay."
                    .to_string()
            } else {
                "Routing policy overlay applied; future runtime routing will use it.".to_string()
            });
        } else {
            recommendations.push(
                "Routing policy proposal is blocked; inspect gates before applying.".to_string(),
            );
        }

        let mut applied = false;
        if !dry_run && blockers.is_empty() {
            let applied_at = now_secs();
            proposal.status = RoutingPolicyProposalStatus::Applied;
            proposal.updated_at = applied_at;
            proposal.applied_at = Some(applied_at);
            proposal.previous_policy = Some(previous_policy.clone());
            proposal
                .audit
                .push(format!("applied routing policy overlay at {applied_at}"));
            save_applied_routing_policy(
                &self.dir,
                &AppliedRoutingPolicy {
                    version: ROUTING_POLICY_PROPOSAL_VERSION,
                    proposal_id: proposal.id.clone(),
                    applied_at,
                    policy: proposal.proposed_policy.clone(),
                    previous_policy: previous_policy.clone(),
                    previous_applied: current_applied.map(Box::new),
                    changes: proposal.changes.clone(),
                    audit: proposal.audit.clone(),
                },
            )?;
            snapshot.proposals[index] = proposal.clone();
            save_routing_policy_proposals(&self.dir, &snapshot)?;
            applied = true;
        }

        Ok(RoutingPolicyApplyReport {
            proposal_id: proposal.id,
            dry_run,
            applied,
            status: proposal.status,
            policy_path: self.applied_policy_path(),
            changed_routes: proposal.changes,
            blockers,
            gates: proposal.gates,
            previous_policy,
            proposed_policy: proposal.proposed_policy,
            recommendations,
        })
    }

    pub fn rollback(&self, proposal_id: &str) -> io::Result<RoutingPolicyRollbackReport> {
        let mut snapshot = self.load()?;
        let Some(index) = snapshot
            .proposals
            .iter()
            .position(|proposal| proposal.id == proposal_id)
        else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("routing policy proposal not found: {proposal_id}"),
            ));
        };
        let mut proposal = snapshot.proposals[index].clone();
        let policy_path = self.applied_policy_path();
        let Some(applied) = load_applied_routing_policy(&self.dir)? else {
            return Ok(RoutingPolicyRollbackReport {
                proposal_id: proposal_id.to_string(),
                rolled_back: false,
                status: proposal.status,
                policy_path,
                restored_policy: None,
                recommendations: vec![
                    "No applied routing policy overlay exists; nothing was rolled back."
                        .to_string(),
                ],
            });
        };
        if applied.proposal_id != proposal_id {
            return Ok(RoutingPolicyRollbackReport {
                proposal_id: proposal_id.to_string(),
                rolled_back: false,
                status: proposal.status,
                policy_path,
                restored_policy: Some(applied.policy),
                recommendations: vec![format!(
                    "Applied routing policy belongs to proposal {}; rollback {proposal_id} is not active.",
                    applied.proposal_id
                )],
            });
        }

        let restored_policy = if let Some(previous_applied) = applied.previous_applied {
            let restored = previous_applied.policy.clone();
            save_applied_routing_policy(&self.dir, &previous_applied)?;
            Some(restored)
        } else {
            remove_applied_routing_policy(&self.dir)?;
            None
        };
        let rolled_back_at = now_secs();
        proposal.status = RoutingPolicyProposalStatus::RolledBack;
        proposal.updated_at = rolled_back_at;
        proposal.rolled_back_at = Some(rolled_back_at);
        proposal.audit.push(format!(
            "rolled back routing policy overlay at {rolled_back_at}"
        ));
        snapshot.proposals[index] = proposal.clone();
        save_routing_policy_proposals(&self.dir, &snapshot)?;

        Ok(RoutingPolicyRollbackReport {
            proposal_id: proposal.id,
            rolled_back: true,
            status: proposal.status,
            policy_path,
            restored_policy,
            recommendations: vec![
                "Routing policy overlay rollback completed; previous routing state was restored."
                    .to_string(),
            ],
        })
    }
}

pub fn routing_policy_proposals_path(dir: &Path) -> PathBuf {
    dir.join(ROUTING_POLICY_PROPOSALS_FILE)
}

pub fn applied_routing_policy_path(dir: &Path) -> PathBuf {
    dir.join(APPLIED_ROUTING_POLICY_FILE)
}

pub fn load_routing_policy_proposals(dir: &Path) -> io::Result<RoutingPolicyProposalSnapshot> {
    let path = routing_policy_proposals_path(dir);
    if !path.exists() {
        return Ok(RoutingPolicyProposalSnapshot {
            proposals: Vec::new(),
        });
    }
    let contents = fs::read_to_string(path)?;
    serde_json::from_str::<RoutingPolicyProposalSnapshot>(&contents)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn load_applied_routing_policy(dir: &Path) -> io::Result<Option<AppliedRoutingPolicy>> {
    let path = applied_routing_policy_path(dir);
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)?;
    serde_json::from_str::<AppliedRoutingPolicy>(&contents)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn build_routing_policy_proposal(
    baseline_policy: MoERoutingPolicy,
    feedback: &RouteFeedbackStore,
    min_samples: usize,
    switch_failure_threshold: f32,
    sequence: usize,
) -> RoutingPolicyProposal {
    let min_samples = min_samples.max(1);
    let switch_failure_threshold = switch_failure_threshold.clamp(0.0, 1.0);
    let evaluation = evaluate_routing_feedback(feedback, min_samples, switch_failure_threshold);
    let replay = replay_routing_optimizer(
        baseline_policy.clone(),
        feedback,
        min_samples,
        switch_failure_threshold,
    );
    let changes = proposal_changes(&evaluation, &replay);
    let proposed_policy = build_proposed_policy(
        &baseline_policy,
        &changes,
        min_samples,
        switch_failure_threshold,
    );
    let estimated_success_delta = proposal_average_delta(
        changes
            .iter()
            .map(|change| change.estimated_success_delta)
            .chain(std::iter::once(replay.estimated_success_delta)),
    );
    let recovery_reduction_estimate = proposal_average_delta(
        changes
            .iter()
            .map(|change| change.recovery_reduction_estimate)
            .chain(std::iter::once(replay.recovery_reduction_estimate)),
    );
    let gates = proposal_safety_gates(
        &evaluation,
        &replay,
        &changes,
        min_samples,
        estimated_success_delta,
    );
    let now = now_secs();
    let id = format!("route-proposal-{now}-{sequence}");
    let status = RoutingPolicyProposalStatus::Draft;
    let audit = vec![format!(
        "proposal generated from {} feedback entrie(s) with min_samples={min_samples} threshold={:.0}%",
        evaluation.feedback_count,
        switch_failure_threshold * 100.0
    )];

    RoutingPolicyProposal {
        version: ROUTING_POLICY_PROPOSAL_VERSION,
        id,
        created_at: now,
        updated_at: now,
        status,
        min_samples,
        switch_failure_threshold,
        feedback_count: evaluation.feedback_count,
        estimated_success_delta,
        recovery_reduction_estimate,
        gates,
        changes,
        baseline_policy,
        proposed_policy,
        previous_policy: None,
        applied_at: None,
        rolled_back_at: None,
        evaluation,
        replay,
        audit,
    }
}

fn save_routing_policy_proposals(
    dir: &Path,
    snapshot: &RoutingPolicyProposalSnapshot,
) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(snapshot)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let path = routing_policy_proposals_path(dir);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, format!("{json}\n"))?;
    fs::rename(&tmp, path)
}

fn save_applied_routing_policy(dir: &Path, policy: &AppliedRoutingPolicy) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(policy)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let path = applied_routing_policy_path(dir);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, format!("{json}\n"))?;
    fs::rename(&tmp, path)
}

fn remove_applied_routing_policy(dir: &Path) -> io::Result<()> {
    let path = applied_routing_policy_path(dir);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn change_from_replay(change: &RoutingReplayChange) -> RoutingPolicyProposalChange {
    RoutingPolicyProposalChange {
        phase: change.phase,
        current_model: change.current_model.clone(),
        current_provider: change.current_provider.clone(),
        proposed_model: change.candidate_model.clone(),
        proposed_provider: change.candidate_provider.clone(),
        fallback_model: change.fallback_model.clone(),
        estimated_success_delta: change.estimated_success_delta,
        recovery_reduction_estimate: change.recovery_reduction_estimate,
        reason: change.reason.clone(),
    }
}

fn proposal_changes(
    evaluation: &RoutingEvaluationReport,
    replay: &RoutingOptimizerReplayReport,
) -> Vec<RoutingPolicyProposalChange> {
    let mut changes = replay
        .changed_routes
        .iter()
        .map(change_from_replay)
        .collect::<Vec<_>>();
    for candidate in evaluation.candidates.iter().filter(|candidate| {
        matches!(
            candidate.kind,
            RoutingPolicyCandidateKind::RecommendFallback
                | RoutingPolicyCandidateKind::PreferQuality
        )
    }) {
        let Some(fallback_model) = candidate.fallback_model.clone() else {
            continue;
        };
        changes.push(RoutingPolicyProposalChange {
            phase: candidate.phase,
            current_model: candidate.model.clone(),
            current_provider: candidate.provider.clone(),
            proposed_model: fallback_model.clone(),
            proposed_provider: None,
            fallback_model: Some(fallback_model),
            estimated_success_delta: candidate.estimated_success_delta,
            recovery_reduction_estimate: candidate.recovery_reduction_estimate,
            reason: candidate.reason.clone(),
        });
    }
    dedupe_policy_changes(&mut changes);
    changes
}

fn dedupe_policy_changes(changes: &mut Vec<RoutingPolicyProposalChange>) {
    let mut deduped = Vec::new();
    for change in changes.drain(..) {
        if deduped
            .iter()
            .any(|existing: &RoutingPolicyProposalChange| {
                existing.phase == change.phase
                    && existing.current_model == change.current_model
                    && existing.current_provider == change.current_provider
                    && existing.proposed_model == change.proposed_model
                    && existing.proposed_provider == change.proposed_provider
            })
        {
            continue;
        }
        deduped.push(change);
    }
    *changes = deduped;
}

fn proposal_average_delta(values: impl Iterator<Item = f32>) -> f32 {
    let mut total = 0.0;
    let mut count = 0_usize;
    for value in values.filter(|value| *value > 0.0) {
        total += value;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        (total / count as f32).clamp(0.0, 1.0)
    }
}

fn build_proposed_policy(
    baseline: &MoERoutingPolicy,
    changes: &[RoutingPolicyProposalChange],
    min_samples: usize,
    switch_failure_threshold: f32,
) -> MoERoutingPolicy {
    let mut policy = baseline.clone();
    policy.adaptive = true;
    policy.min_feedback_samples = min_samples.max(1);
    policy.switch_failure_threshold_percent =
        ((switch_failure_threshold.clamp(0.0, 1.0) * 100.0).round() as u8).min(100);
    for change in changes {
        if let Some(route) = policy.routes.iter_mut().find(|route| {
            route.role == change.phase
                && route.model == change.current_model
                && route.provider == change.current_provider
        }) {
            route.cost_weight = route.cost_weight.min(1);
            route.latency_weight = route.latency_weight.min(1);
            route.quality_weight = route.quality_weight.min(2);
        }
        if let Some(route) = policy.routes.iter_mut().find(|route| {
            route.role == change.phase
                && route.model == change.proposed_model
                && route.provider == change.proposed_provider
        }) {
            route.cost_weight = route.cost_weight.max(5);
            route.latency_weight = route.latency_weight.max(5);
            route.quality_weight = route.quality_weight.max(5);
            continue;
        }
        let mut route = ModelRoute::new(change.phase, change.proposed_model.clone())
            .with_capabilities(capabilities_for_phase(change.phase))
            .with_weights(5, 5, 5);
        if let Some(provider) = &change.proposed_provider {
            route = route.with_provider(provider.clone());
        }
        policy.routes.push(route);
    }
    policy
}

fn capabilities_for_phase(phase: ModelRoutePhase) -> Vec<ModelCapability> {
    match phase {
        ModelRoutePhase::Planning => vec![ModelCapability::Planning, ModelCapability::LongContext],
        ModelRoutePhase::Coding => vec![ModelCapability::Coding, ModelCapability::Refactor],
        ModelRoutePhase::Verification => {
            vec![
                ModelCapability::Verification,
                ModelCapability::TestGeneration,
            ]
        }
        ModelRoutePhase::Summarization => {
            vec![ModelCapability::Summarization, ModelCapability::Cheap]
        }
        ModelRoutePhase::Vision => vec![ModelCapability::Vision],
        ModelRoutePhase::LocalFast => vec![ModelCapability::LocalFast, ModelCapability::Cheap],
    }
}

fn proposal_safety_gates(
    evaluation: &RoutingEvaluationReport,
    replay: &RoutingOptimizerReplayReport,
    changes: &[RoutingPolicyProposalChange],
    min_samples: usize,
    estimated_success_delta: f32,
) -> Vec<RoutingPolicySafetyGate> {
    let mut gates = Vec::new();
    gates.push(RoutingPolicySafetyGate {
        name: "minimum_feedback".to_string(),
        passed: evaluation.feedback_count >= min_samples,
        level: RoutingPolicySafetyLevel::Blocker,
        requires_review: false,
        reason: format!(
            "feedback_count={} min_samples={min_samples}",
            evaluation.feedback_count
        ),
    });
    gates.push(RoutingPolicySafetyGate {
        name: "has_route_changes".to_string(),
        passed: !changes.is_empty(),
        level: RoutingPolicySafetyLevel::Blocker,
        requires_review: false,
        reason: format!("{} route change(s) proposed", changes.len()),
    });
    gates.push(RoutingPolicySafetyGate {
        name: "estimated_success_delta".to_string(),
        passed: estimated_success_delta >= 0.20,
        level: RoutingPolicySafetyLevel::Blocker,
        requires_review: false,
        reason: format!(
            "estimated_success_delta={:.0}%",
            estimated_success_delta * 100.0
        ),
    });
    let critical_min_delta = changes
        .iter()
        .filter(|change| {
            matches!(
                change.phase,
                ModelRoutePhase::Planning | ModelRoutePhase::Coding | ModelRoutePhase::Verification
            )
        })
        .map(|change| change.estimated_success_delta)
        .fold(1.0_f32, f32::min);
    gates.push(RoutingPolicySafetyGate {
        name: "critical_phase_margin".to_string(),
        passed: critical_min_delta >= 0.35 || critical_min_delta == 1.0 && changes.is_empty(),
        level: RoutingPolicySafetyLevel::Blocker,
        requires_review: false,
        reason: format!(
            "critical_phase_min_delta={:.0}%",
            critical_min_delta * 100.0
        ),
    });
    let high_cost_latency_risk = replay
        .cost_latency_tradeoff
        .contains("quality-biased fallback may increase cost or latency");
    gates.push(RoutingPolicySafetyGate {
        name: "cost_latency_risk".to_string(),
        passed: !high_cost_latency_risk,
        level: if high_cost_latency_risk {
            RoutingPolicySafetyLevel::Blocker
        } else {
            RoutingPolicySafetyLevel::Info
        },
        requires_review: high_cost_latency_risk,
        reason: replay.cost_latency_tradeoff.clone(),
    });
    gates
}

fn proposal_apply_blockers(proposal: &RoutingPolicyProposal) -> Vec<String> {
    let mut blockers = proposal
        .gates
        .iter()
        .filter(|gate| !gate.passed && gate.level == RoutingPolicySafetyLevel::Blocker)
        .map(|gate| format!("{}: {}", gate.name, gate.reason))
        .collect::<Vec<_>>();
    if !matches!(
        proposal.status,
        RoutingPolicyProposalStatus::Draft | RoutingPolicyProposalStatus::Approved
    ) {
        blockers.push(format!(
            "proposal status {:?} cannot be applied",
            proposal.status
        ));
    }
    blockers
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelRouteDecision, ModelRouteFeedback};

    fn decision(phase: ModelRoutePhase, model: &str) -> ModelRouteDecision {
        ModelRouteDecision {
            phase,
            model: model.to_string(),
            provider: None,
            reason: "test".to_string(),
            confidence: Some(0.8),
            fallback_model: None,
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("himalaya-route-policy-{label}-{}", now_secs()))
    }

    #[test]
    fn proposal_builds_applyable_policy_change() {
        let feedback = RouteFeedbackStore::from_feedback(vec![
            ModelRouteFeedback::pending("task-1", decision(ModelRoutePhase::Coding, "weak"), 1)
                .with_outcome(false, Some(false), true, Some("failed".to_string())),
            ModelRouteFeedback::pending("task-2", decision(ModelRoutePhase::Coding, "weak"), 2)
                .with_outcome(false, Some(false), true, Some("failed".to_string())),
            ModelRouteFeedback::pending("task-3", decision(ModelRoutePhase::Coding, "strong"), 3)
                .with_outcome(true, Some(true), false, Some("ok".to_string())),
            ModelRouteFeedback::pending("task-4", decision(ModelRoutePhase::Coding, "strong"), 4)
                .with_outcome(true, Some(true), false, Some("ok".to_string())),
        ]);
        let policy = MoERoutingPolicy::new(
            "weak",
            vec![
                ModelRoute::new(ModelRoutePhase::Coding, "weak").with_weights(5, 5, 5),
                ModelRoute::new(ModelRoutePhase::Coding, "strong").with_weights(1, 1, 4),
            ],
        );

        let proposal = build_routing_policy_proposal(policy, &feedback, 2, 0.5, 1);

        assert!(!proposal.changes.is_empty());
        assert!(proposal.gates.iter().all(|gate| gate.passed));
        assert!(proposal
            .proposed_policy
            .routes
            .iter()
            .any(|route| route.model == "strong" && route.quality_weight == 5));
    }

    #[test]
    fn store_applies_and_rolls_back_policy_overlay() {
        let dir = temp_dir("apply-rollback");
        let store = RoutingPolicyProposalStore::new(&dir);
        let feedback = RouteFeedbackStore::from_feedback(vec![
            ModelRouteFeedback::pending(
                "task-1",
                decision(ModelRoutePhase::Verification, "weak"),
                1,
            )
            .with_outcome(false, Some(false), true, Some("failed".to_string())),
            ModelRouteFeedback::pending(
                "task-2",
                decision(ModelRoutePhase::Verification, "weak"),
                2,
            )
            .with_outcome(false, Some(false), true, Some("failed".to_string())),
            ModelRouteFeedback::pending(
                "task-3",
                decision(ModelRoutePhase::Verification, "strong"),
                3,
            )
            .with_outcome(true, Some(true), false, Some("ok".to_string())),
        ]);
        let policy = MoERoutingPolicy::new(
            "weak",
            vec![
                ModelRoute::new(ModelRoutePhase::Verification, "weak").with_weights(5, 5, 5),
                ModelRoute::new(ModelRoutePhase::Verification, "strong").with_weights(1, 1, 4),
            ],
        );
        let proposal = store
            .propose(policy, &feedback, 2, 0.5)
            .expect("proposal should persist");

        let dry_run = store
            .apply(&proposal.id, true)
            .expect("dry run should report");
        assert!(!dry_run.applied);
        assert!(dry_run.blockers.is_empty());

        let applied = store
            .apply(&proposal.id, false)
            .expect("apply should persist overlay");
        assert!(applied.applied);
        assert!(load_applied_routing_policy(&dir)
            .expect("applied policy should load")
            .is_some());

        let rollback = store
            .rollback(&proposal.id)
            .expect("rollback should succeed");
        assert!(rollback.rolled_back);
        assert!(load_applied_routing_policy(&dir)
            .expect("applied policy lookup should succeed")
            .is_none());
    }

    #[test]
    fn proposal_with_insufficient_data_is_blocked() {
        let feedback = RouteFeedbackStore::from_feedback(vec![ModelRouteFeedback::pending(
            "task-1",
            decision(ModelRoutePhase::Coding, "weak"),
            1,
        )
        .with_outcome(false, Some(false), true, Some("failed".to_string()))]);
        let proposal =
            build_routing_policy_proposal(MoERoutingPolicy::balanced("weak"), &feedback, 3, 0.5, 1);

        assert!(proposal_apply_blockers(&proposal)
            .iter()
            .any(|blocker| blocker.contains("minimum_feedback")));
    }
}
