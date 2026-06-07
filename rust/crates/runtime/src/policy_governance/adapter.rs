use std::io;

use crate::{
    AutonomousEvaluationReport, RoutingPolicyApplyReport, RoutingPolicyProposalStore,
    RoutingPolicyRollbackReport, SchedulerDaemonState, SchedulerDaemonStatus,
};

use super::coordinator::{policy_blocker, routing_action_snapshot};
use super::review::{
    map_routing_status, memory_policy_proposal, memory_policy_simulation_recommendations,
    recovery_policy_proposal, recovery_policy_simulation_recommendations,
    scheduler_policy_proposal,
};
use super::{
    MemoryPolicyDryRunReport, PolicyAdapterDescriptor, PolicyAdapterReport, PolicyApplyAction,
    PolicyApplyOperation, PolicyBlocker, PolicyBlockerKind, PolicyDomain, PolicyLedgerStatus,
    PolicyRiskLevel, RecoveryPolicyDryRunReport, SchedulerPolicyDryRunReport,
};

#[derive(Debug, Clone)]
pub(super) struct PolicyAdapterValidation {
    pub(super) proposal_id: String,
    pub(super) before_status: PolicyLedgerStatus,
}

#[derive(Debug, Clone)]
pub(super) struct PolicyAdapterApplyResult {
    pub(super) status: PolicyLedgerStatus,
    pub(super) applied: bool,
    pub(super) executed: bool,
    pub(super) after_status: Option<PolicyLedgerStatus>,
    pub(super) adapter_report_id: Option<String>,
    pub(super) adapter_report: Option<PolicyAdapterReport>,
    pub(super) routing_report: Option<RoutingPolicyApplyReport>,
    pub(super) blockers: Vec<PolicyBlocker>,
    pub(super) recommendations: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct PolicyAdapterRollbackResult {
    pub(super) status: PolicyLedgerStatus,
    pub(super) rolled_back: bool,
    pub(super) executed: bool,
    pub(super) after_status: Option<PolicyLedgerStatus>,
    pub(super) adapter_report_id: Option<String>,
    pub(super) adapter_report: Option<PolicyAdapterReport>,
    pub(super) routing_report: Option<RoutingPolicyRollbackReport>,
    pub(super) blockers: Vec<PolicyBlocker>,
    pub(super) recommendations: Vec<String>,
}

pub(super) trait PolicyAdapter {
    fn descriptor(&self) -> PolicyAdapterDescriptor;

    fn validate(
        &self,
        action: &PolicyApplyAction,
        operation: PolicyApplyOperation,
    ) -> io::Result<Result<PolicyAdapterValidation, Vec<PolicyBlocker>>>;

    fn apply(
        &self,
        validation: &PolicyAdapterValidation,
        dry_run: bool,
    ) -> io::Result<PolicyAdapterApplyResult>;

    fn rollback(
        &self,
        validation: &PolicyAdapterValidation,
    ) -> io::Result<PolicyAdapterRollbackResult>;
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PolicyAdapterContext<'a> {
    pub(super) routing_store: &'a RoutingPolicyProposalStore,
    pub(super) scheduler_state: Option<&'a SchedulerDaemonState>,
    pub(super) autonomous_evaluation: Option<&'a AutonomousEvaluationReport>,
}

pub(super) struct PolicyAdapterRegistry<'a> {
    context: PolicyAdapterContext<'a>,
}

impl<'a> PolicyAdapterRegistry<'a> {
    pub(super) fn new(context: PolicyAdapterContext<'a>) -> Self {
        Self { context }
    }

    pub(super) fn adapter_for_domain(
        &self,
        domain: PolicyDomain,
    ) -> Option<Box<dyn PolicyAdapter + 'a>> {
        match domain {
            PolicyDomain::Routing => Some(Box::new(RoutingPolicyAdapter::new(
                self.context.routing_store,
            ))),
            PolicyDomain::Scheduler => Some(Box::new(SchedulerPolicyAdapter::new(
                self.context.scheduler_state,
            ))),
            PolicyDomain::Memory => Some(Box::new(MemoryPolicyAdapter::new(
                self.context.autonomous_evaluation,
            ))),
            PolicyDomain::Recovery => Some(Box::new(RecoveryPolicyAdapter::new(
                self.context.autonomous_evaluation,
            ))),
            PolicyDomain::AutonomousRun => None,
        }
    }
}

#[derive(Debug, Clone)]
struct RoutingPolicyAdapter<'a> {
    store: &'a RoutingPolicyProposalStore,
}

impl<'a> RoutingPolicyAdapter<'a> {
    const NAME: &'static str = "routing_policy";

    fn new(store: &'a RoutingPolicyProposalStore) -> Self {
        Self { store }
    }

    fn descriptor_static() -> PolicyAdapterDescriptor {
        PolicyAdapterDescriptor {
            name: Self::NAME.to_string(),
            domain: PolicyDomain::Routing,
            supports_apply: true,
            supports_persistent_apply: true,
            supports_dry_run: true,
            supports_rollback: true,
            planned_only: false,
            reason: "routing policy adapter can apply and rollback MoE routing overlays"
                .to_string(),
        }
    }
}

impl PolicyAdapter for RoutingPolicyAdapter<'_> {
    fn descriptor(&self) -> PolicyAdapterDescriptor {
        Self::descriptor_static()
    }

    fn validate(
        &self,
        action: &PolicyApplyAction,
        operation: PolicyApplyOperation,
    ) -> io::Result<Result<PolicyAdapterValidation, Vec<PolicyBlocker>>> {
        let Some(proposal_id) = action.proposal_id.as_deref() else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::MissingTarget,
                Some(PolicyDomain::Routing),
                None,
                PolicyRiskLevel::High,
                "routing policy action is missing a proposal id",
                Self::NAME,
            )]));
        };
        let snapshot = self.store.load()?;
        let Some(proposal) = snapshot
            .proposals
            .iter()
            .find(|proposal| proposal.id == proposal_id)
        else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::MissingProposal,
                Some(PolicyDomain::Routing),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                format!("routing policy proposal not found: {proposal_id}"),
                Self::NAME,
            )]));
        };
        let Some(current_action) = routing_action_snapshot(proposal, operation) else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::AdapterRejected,
                Some(PolicyDomain::Routing),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                "routing policy proposal cannot be projected into a governed action",
                Self::NAME,
            )]));
        };
        if current_action.source_fingerprint != action.source_fingerprint {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::StalePlan,
                Some(PolicyDomain::Routing),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                format!(
                    "routing proposal state changed since plan creation: planned={} current={}",
                    action.source_fingerprint, current_action.source_fingerprint
                ),
                Self::NAME,
            )]));
        }
        Ok(Ok(PolicyAdapterValidation {
            proposal_id: proposal_id.to_string(),
            before_status: map_routing_status(proposal.status),
        }))
    }

    fn apply(
        &self,
        validation: &PolicyAdapterValidation,
        dry_run: bool,
    ) -> io::Result<PolicyAdapterApplyResult> {
        let routing_report = self.store.apply(&validation.proposal_id, dry_run)?;
        let mut blockers = routing_report
            .blockers
            .iter()
            .map(|blocker| {
                policy_blocker(
                    PolicyBlockerKind::AdapterRejected,
                    Some(PolicyDomain::Routing),
                    Some(validation.proposal_id.clone()),
                    PolicyRiskLevel::High,
                    blocker.clone(),
                    Self::NAME,
                )
            })
            .collect::<Vec<_>>();
        let (status, applied) = if !blockers.is_empty() {
            (PolicyLedgerStatus::ApplyBlocked, false)
        } else if dry_run {
            (PolicyLedgerStatus::DryRunPassed, false)
        } else if routing_report.applied {
            (PolicyLedgerStatus::Applied, true)
        } else {
            blockers.push(policy_blocker(
                PolicyBlockerKind::AdapterRejected,
                Some(PolicyDomain::Routing),
                Some(validation.proposal_id.clone()),
                PolicyRiskLevel::High,
                "routing policy store did not apply the proposal",
                Self::NAME,
            ));
            (PolicyLedgerStatus::ApplyBlocked, false)
        };
        let mut recommendations = routing_report.recommendations.clone();
        if status == PolicyLedgerStatus::DryRunPassed {
            recommendations.push(
                "Governance dry run passed; rerun with --domain routing --proposal-id to apply."
                    .to_string(),
            );
        }
        Ok(PolicyAdapterApplyResult {
            status,
            applied,
            executed: applied || dry_run && status == PolicyLedgerStatus::DryRunPassed,
            after_status: Some(map_routing_status(routing_report.status)),
            adapter_report_id: Some(format!(
                "routing_policy_apply:{}",
                routing_report.proposal_id
            )),
            adapter_report: Some(PolicyAdapterReport::RoutingApply(routing_report.clone())),
            routing_report: Some(routing_report),
            blockers,
            recommendations,
        })
    }

    fn rollback(
        &self,
        validation: &PolicyAdapterValidation,
    ) -> io::Result<PolicyAdapterRollbackResult> {
        let routing_report = self.store.rollback(&validation.proposal_id)?;
        let status = if routing_report.rolled_back {
            PolicyLedgerStatus::RolledBack
        } else {
            PolicyLedgerStatus::RollbackBlocked
        };
        let blockers = if routing_report.rolled_back {
            Vec::new()
        } else {
            routing_report
                .recommendations
                .iter()
                .map(|recommendation| {
                    policy_blocker(
                        PolicyBlockerKind::AdapterRejected,
                        Some(PolicyDomain::Routing),
                        Some(validation.proposal_id.clone()),
                        PolicyRiskLevel::High,
                        recommendation.clone(),
                        Self::NAME,
                    )
                })
                .collect::<Vec<_>>()
        };
        Ok(PolicyAdapterRollbackResult {
            status,
            rolled_back: routing_report.rolled_back,
            executed: routing_report.rolled_back,
            after_status: Some(map_routing_status(routing_report.status)),
            adapter_report_id: Some(format!(
                "routing_policy_rollback:{}",
                routing_report.proposal_id
            )),
            adapter_report: Some(PolicyAdapterReport::RoutingRollback(routing_report.clone())),
            routing_report: Some(routing_report.clone()),
            blockers,
            recommendations: routing_report.recommendations,
        })
    }
}

#[derive(Debug, Clone)]
struct SchedulerPolicyAdapter<'a> {
    scheduler_state: Option<&'a SchedulerDaemonState>,
}

impl<'a> SchedulerPolicyAdapter<'a> {
    const NAME: &'static str = "scheduler_policy";

    fn new(scheduler_state: Option<&'a SchedulerDaemonState>) -> Self {
        Self { scheduler_state }
    }

    fn descriptor_static() -> PolicyAdapterDescriptor {
        PolicyAdapterDescriptor {
            name: Self::NAME.to_string(),
            domain: PolicyDomain::Scheduler,
            supports_apply: true,
            supports_persistent_apply: false,
            supports_dry_run: true,
            supports_rollback: false,
            planned_only: false,
            reason: "scheduler policy adapter supports dry-run validation only".to_string(),
        }
    }
}

impl PolicyAdapter for SchedulerPolicyAdapter<'_> {
    fn descriptor(&self) -> PolicyAdapterDescriptor {
        Self::descriptor_static()
    }

    fn validate(
        &self,
        action: &PolicyApplyAction,
        operation: PolicyApplyOperation,
    ) -> io::Result<Result<PolicyAdapterValidation, Vec<PolicyBlocker>>> {
        let Some(proposal_id) = action.proposal_id.as_deref() else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::MissingTarget,
                Some(PolicyDomain::Scheduler),
                None,
                PolicyRiskLevel::High,
                "scheduler policy action is missing a proposal id",
                Self::NAME,
            )]));
        };
        if operation != PolicyApplyOperation::Apply {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::UnsupportedDomain,
                Some(PolicyDomain::Scheduler),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::Medium,
                "scheduler policy rollback is not executable",
                Self::NAME,
            )]));
        }
        if action.domain != PolicyDomain::Scheduler {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::UnsupportedDomain,
                Some(action.domain),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                "scheduler adapter received a non-scheduler policy action",
                Self::NAME,
            )]));
        }
        let Some(state) = self.scheduler_state else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::MissingProposal,
                Some(PolicyDomain::Scheduler),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                "scheduler state is unavailable for policy dry-run validation",
                Self::NAME,
            )]));
        };
        let current = scheduler_policy_proposal(state);
        if current.id != proposal_id || current.source_fingerprint != action.source_fingerprint {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::StalePlan,
                Some(PolicyDomain::Scheduler),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                format!(
                    "scheduler state changed since plan creation: planned={} current={}",
                    action.source_fingerprint, current.source_fingerprint
                ),
                Self::NAME,
            )]));
        }
        Ok(Ok(PolicyAdapterValidation {
            proposal_id: proposal_id.to_string(),
            before_status: action.status,
        }))
    }

    fn apply(
        &self,
        validation: &PolicyAdapterValidation,
        dry_run: bool,
    ) -> io::Result<PolicyAdapterApplyResult> {
        if !dry_run {
            let blocker = policy_blocker(
                PolicyBlockerKind::UnsupportedDomain,
                Some(PolicyDomain::Scheduler),
                Some(validation.proposal_id.clone()),
                PolicyRiskLevel::High,
                "scheduler policy adapter is dry-run only; persistent apply is not supported",
                Self::NAME,
            );
            return Ok(PolicyAdapterApplyResult {
                status: PolicyLedgerStatus::ApplyBlocked,
                applied: false,
                executed: false,
                after_status: Some(PolicyLedgerStatus::ApplyBlocked),
                adapter_report_id: Some(format!(
                    "scheduler_policy_apply_blocked:{}",
                    validation.proposal_id
                )),
                adapter_report: Some(PolicyAdapterReport::SchedulerDryRun(
                    SchedulerPolicyDryRunReport {
                        proposal_id: validation.proposal_id.clone(),
                        dry_run,
                        status: PolicyLedgerStatus::ApplyBlocked,
                        scheduler_status: self.scheduler_state.map(|state| state.status),
                        blockers: vec![blocker.clone()],
                        recommendations: vec![
                            "Keep scheduler policy changes in governance review until a persistent adapter is available."
                                .to_string(),
                        ],
                    },
                )),
                routing_report: None,
                blockers: vec![blocker],
                recommendations: vec![
                    "Scheduler policy dry-run adapter rejected persistent apply.".to_string(),
                ],
            });
        }
        let recommendations = vec![
            "Scheduler policy dry run passed; no daemon state or scheduler configuration was changed."
                .to_string(),
        ];
        let report = SchedulerPolicyDryRunReport {
            proposal_id: validation.proposal_id.clone(),
            dry_run,
            status: PolicyLedgerStatus::DryRunPassed,
            scheduler_status: self.scheduler_state.map(|state| state.status),
            blockers: Vec::new(),
            recommendations: recommendations.clone(),
        };
        Ok(PolicyAdapterApplyResult {
            status: PolicyLedgerStatus::DryRunPassed,
            applied: false,
            executed: true,
            after_status: Some(PolicyLedgerStatus::DryRunPassed),
            adapter_report_id: Some(format!(
                "scheduler_policy_dry_run:{}",
                validation.proposal_id
            )),
            adapter_report: Some(PolicyAdapterReport::SchedulerDryRun(report)),
            routing_report: None,
            blockers: Vec::new(),
            recommendations,
        })
    }

    fn rollback(
        &self,
        validation: &PolicyAdapterValidation,
    ) -> io::Result<PolicyAdapterRollbackResult> {
        let blocker = policy_blocker(
            PolicyBlockerKind::UnsupportedDomain,
            Some(PolicyDomain::Scheduler),
            Some(validation.proposal_id.clone()),
            PolicyRiskLevel::High,
            "scheduler policy rollback is not supported",
            Self::NAME,
        );
        Ok(PolicyAdapterRollbackResult {
            status: PolicyLedgerStatus::RollbackBlocked,
            rolled_back: false,
            executed: false,
            after_status: Some(PolicyLedgerStatus::RollbackBlocked),
            adapter_report_id: None,
            adapter_report: None,
            routing_report: None,
            blockers: vec![blocker],
            recommendations: vec![
                "Scheduler policy rollback remains governance-planned only.".to_string()
            ],
        })
    }
}

#[derive(Debug, Clone)]
struct MemoryPolicyAdapter<'a> {
    evaluation: Option<&'a AutonomousEvaluationReport>,
}

impl<'a> MemoryPolicyAdapter<'a> {
    const NAME: &'static str = "memory_policy";

    fn new(evaluation: Option<&'a AutonomousEvaluationReport>) -> Self {
        Self { evaluation }
    }

    fn descriptor_static() -> PolicyAdapterDescriptor {
        PolicyAdapterDescriptor {
            name: Self::NAME.to_string(),
            domain: PolicyDomain::Memory,
            supports_apply: true,
            supports_persistent_apply: false,
            supports_dry_run: true,
            supports_rollback: false,
            planned_only: false,
            reason: "memory policy adapter supports dry-run simulation only".to_string(),
        }
    }
}

impl PolicyAdapter for MemoryPolicyAdapter<'_> {
    fn descriptor(&self) -> PolicyAdapterDescriptor {
        Self::descriptor_static()
    }

    fn validate(
        &self,
        action: &PolicyApplyAction,
        operation: PolicyApplyOperation,
    ) -> io::Result<Result<PolicyAdapterValidation, Vec<PolicyBlocker>>> {
        let Some(proposal_id) = action.proposal_id.as_deref() else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::MissingTarget,
                Some(PolicyDomain::Memory),
                None,
                PolicyRiskLevel::High,
                "memory policy action is missing a proposal id",
                Self::NAME,
            )]));
        };
        if operation != PolicyApplyOperation::Apply {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::UnsupportedDomain,
                Some(PolicyDomain::Memory),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::Medium,
                "memory policy rollback is not executable",
                Self::NAME,
            )]));
        }
        if action.domain != PolicyDomain::Memory {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::UnsupportedDomain,
                Some(action.domain),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                "memory adapter received a non-memory policy action",
                Self::NAME,
            )]));
        }
        let Some(evaluation) = self.evaluation else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::MissingProposal,
                Some(PolicyDomain::Memory),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                "autonomous evaluation is unavailable for memory policy simulation",
                Self::NAME,
            )]));
        };
        let current = memory_policy_proposal(evaluation);
        if current.id != proposal_id || current.source_fingerprint != action.source_fingerprint {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::StalePlan,
                Some(PolicyDomain::Memory),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                format!(
                    "memory policy inputs changed since plan creation: planned={} current={}",
                    action.source_fingerprint, current.source_fingerprint
                ),
                Self::NAME,
            )]));
        }
        Ok(Ok(PolicyAdapterValidation {
            proposal_id: proposal_id.to_string(),
            before_status: action.status,
        }))
    }

    fn apply(
        &self,
        validation: &PolicyAdapterValidation,
        dry_run: bool,
    ) -> io::Result<PolicyAdapterApplyResult> {
        let Some(evaluation) = self.evaluation else {
            let blocker = policy_blocker(
                PolicyBlockerKind::MissingProposal,
                Some(PolicyDomain::Memory),
                Some(validation.proposal_id.clone()),
                PolicyRiskLevel::High,
                "autonomous evaluation is unavailable for memory policy simulation",
                Self::NAME,
            );
            return Ok(memory_policy_blocked_result(validation, dry_run, blocker));
        };
        if !dry_run {
            let blocker = policy_blocker(
                PolicyBlockerKind::UnsupportedDomain,
                Some(PolicyDomain::Memory),
                Some(validation.proposal_id.clone()),
                PolicyRiskLevel::High,
                "memory policy adapter is dry-run only; persistent apply is not supported",
                Self::NAME,
            );
            return Ok(memory_policy_blocked_result(validation, dry_run, blocker));
        }
        let recommendations = memory_policy_simulation_recommendations(evaluation);
        let report = MemoryPolicyDryRunReport {
            proposal_id: validation.proposal_id.clone(),
            dry_run,
            status: PolicyLedgerStatus::DryRunPassed,
            task_count: evaluation.counters.tasks,
            task_memory_entries: evaluation.counters.task_memory_entries,
            memory_reuse_score: evaluation.scores.memory_reuse_score,
            blockers: Vec::new(),
            recommendations: recommendations.clone(),
        };
        Ok(PolicyAdapterApplyResult {
            status: PolicyLedgerStatus::DryRunPassed,
            applied: false,
            executed: true,
            after_status: Some(PolicyLedgerStatus::DryRunPassed),
            adapter_report_id: Some(format!("memory_policy_dry_run:{}", validation.proposal_id)),
            adapter_report: Some(PolicyAdapterReport::MemoryDryRun(report)),
            routing_report: None,
            blockers: Vec::new(),
            recommendations,
        })
    }

    fn rollback(
        &self,
        validation: &PolicyAdapterValidation,
    ) -> io::Result<PolicyAdapterRollbackResult> {
        let blocker = policy_blocker(
            PolicyBlockerKind::UnsupportedDomain,
            Some(PolicyDomain::Memory),
            Some(validation.proposal_id.clone()),
            PolicyRiskLevel::High,
            "memory policy rollback is not supported",
            Self::NAME,
        );
        Ok(PolicyAdapterRollbackResult {
            status: PolicyLedgerStatus::RollbackBlocked,
            rolled_back: false,
            executed: false,
            after_status: Some(PolicyLedgerStatus::RollbackBlocked),
            adapter_report_id: None,
            adapter_report: None,
            routing_report: None,
            blockers: vec![blocker],
            recommendations: vec![
                "Memory policy rollback remains governance-planned only.".to_string()
            ],
        })
    }
}

#[derive(Debug, Clone)]
struct RecoveryPolicyAdapter<'a> {
    evaluation: Option<&'a AutonomousEvaluationReport>,
}

impl<'a> RecoveryPolicyAdapter<'a> {
    const NAME: &'static str = "recovery_policy";

    fn new(evaluation: Option<&'a AutonomousEvaluationReport>) -> Self {
        Self { evaluation }
    }

    fn descriptor_static() -> PolicyAdapterDescriptor {
        PolicyAdapterDescriptor {
            name: Self::NAME.to_string(),
            domain: PolicyDomain::Recovery,
            supports_apply: true,
            supports_persistent_apply: false,
            supports_dry_run: true,
            supports_rollback: false,
            planned_only: false,
            reason: "recovery policy adapter supports dry-run simulation only".to_string(),
        }
    }
}

impl PolicyAdapter for RecoveryPolicyAdapter<'_> {
    fn descriptor(&self) -> PolicyAdapterDescriptor {
        Self::descriptor_static()
    }

    fn validate(
        &self,
        action: &PolicyApplyAction,
        operation: PolicyApplyOperation,
    ) -> io::Result<Result<PolicyAdapterValidation, Vec<PolicyBlocker>>> {
        let Some(proposal_id) = action.proposal_id.as_deref() else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::MissingTarget,
                Some(PolicyDomain::Recovery),
                None,
                PolicyRiskLevel::High,
                "recovery policy action is missing a proposal id",
                Self::NAME,
            )]));
        };
        if operation != PolicyApplyOperation::Apply {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::UnsupportedDomain,
                Some(PolicyDomain::Recovery),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::Medium,
                "recovery policy rollback is not executable",
                Self::NAME,
            )]));
        }
        if action.domain != PolicyDomain::Recovery {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::UnsupportedDomain,
                Some(action.domain),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                "recovery adapter received a non-recovery policy action",
                Self::NAME,
            )]));
        }
        let Some(evaluation) = self.evaluation else {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::MissingProposal,
                Some(PolicyDomain::Recovery),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                "autonomous evaluation is unavailable for recovery policy simulation",
                Self::NAME,
            )]));
        };
        let current = recovery_policy_proposal(evaluation);
        if current.id != proposal_id || current.source_fingerprint != action.source_fingerprint {
            return Ok(Err(vec![policy_blocker(
                PolicyBlockerKind::StalePlan,
                Some(PolicyDomain::Recovery),
                Some(proposal_id.to_string()),
                PolicyRiskLevel::High,
                format!(
                    "recovery policy inputs changed since plan creation: planned={} current={}",
                    action.source_fingerprint, current.source_fingerprint
                ),
                Self::NAME,
            )]));
        }
        Ok(Ok(PolicyAdapterValidation {
            proposal_id: proposal_id.to_string(),
            before_status: action.status,
        }))
    }

    fn apply(
        &self,
        validation: &PolicyAdapterValidation,
        dry_run: bool,
    ) -> io::Result<PolicyAdapterApplyResult> {
        let Some(evaluation) = self.evaluation else {
            let blocker = policy_blocker(
                PolicyBlockerKind::MissingProposal,
                Some(PolicyDomain::Recovery),
                Some(validation.proposal_id.clone()),
                PolicyRiskLevel::High,
                "autonomous evaluation is unavailable for recovery policy simulation",
                Self::NAME,
            );
            return Ok(recovery_policy_blocked_result(validation, dry_run, blocker));
        };
        if !dry_run {
            let blocker = policy_blocker(
                PolicyBlockerKind::UnsupportedDomain,
                Some(PolicyDomain::Recovery),
                Some(validation.proposal_id.clone()),
                PolicyRiskLevel::High,
                "recovery policy adapter is dry-run only; persistent apply is not supported",
                Self::NAME,
            );
            return Ok(recovery_policy_blocked_result(validation, dry_run, blocker));
        }
        let recommendations = recovery_policy_simulation_recommendations(evaluation);
        let report = RecoveryPolicyDryRunReport {
            proposal_id: validation.proposal_id.clone(),
            dry_run,
            status: PolicyLedgerStatus::DryRunPassed,
            recovery_triggered_tasks: evaluation.counters.recovery_triggered_tasks,
            recovered_tasks: evaluation.counters.recovered_tasks,
            recovery_quality_score: evaluation.scores.recovery_quality_score,
            blockers: Vec::new(),
            recommendations: recommendations.clone(),
        };
        Ok(PolicyAdapterApplyResult {
            status: PolicyLedgerStatus::DryRunPassed,
            applied: false,
            executed: true,
            after_status: Some(PolicyLedgerStatus::DryRunPassed),
            adapter_report_id: Some(format!(
                "recovery_policy_dry_run:{}",
                validation.proposal_id
            )),
            adapter_report: Some(PolicyAdapterReport::RecoveryDryRun(report)),
            routing_report: None,
            blockers: Vec::new(),
            recommendations,
        })
    }

    fn rollback(
        &self,
        validation: &PolicyAdapterValidation,
    ) -> io::Result<PolicyAdapterRollbackResult> {
        let blocker = policy_blocker(
            PolicyBlockerKind::UnsupportedDomain,
            Some(PolicyDomain::Recovery),
            Some(validation.proposal_id.clone()),
            PolicyRiskLevel::High,
            "recovery policy rollback is not supported",
            Self::NAME,
        );
        Ok(PolicyAdapterRollbackResult {
            status: PolicyLedgerStatus::RollbackBlocked,
            rolled_back: false,
            executed: false,
            after_status: Some(PolicyLedgerStatus::RollbackBlocked),
            adapter_report_id: None,
            adapter_report: None,
            routing_report: None,
            blockers: vec![blocker],
            recommendations: vec![
                "Recovery policy rollback remains governance-planned only.".to_string()
            ],
        })
    }
}

pub fn policy_adapter_descriptors(
    _scheduler_status: Option<SchedulerDaemonStatus>,
) -> Vec<PolicyAdapterDescriptor> {
    vec![
        RoutingPolicyAdapter::descriptor_static(),
        SchedulerPolicyAdapter::descriptor_static(),
        MemoryPolicyAdapter::descriptor_static(),
        RecoveryPolicyAdapter::descriptor_static(),
        planned_only_adapter_descriptor(PolicyDomain::AutonomousRun),
    ]
}

fn memory_policy_blocked_result(
    validation: &PolicyAdapterValidation,
    dry_run: bool,
    blocker: PolicyBlocker,
) -> PolicyAdapterApplyResult {
    let recommendations = vec![
        "Memory policy changes remain dry-run only until a persistent store is available."
            .to_string(),
    ];
    let report = MemoryPolicyDryRunReport {
        proposal_id: validation.proposal_id.clone(),
        dry_run,
        status: PolicyLedgerStatus::ApplyBlocked,
        task_count: 0,
        task_memory_entries: 0,
        memory_reuse_score: 0.0,
        blockers: vec![blocker.clone()],
        recommendations: recommendations.clone(),
    };
    PolicyAdapterApplyResult {
        status: PolicyLedgerStatus::ApplyBlocked,
        applied: false,
        executed: false,
        after_status: Some(PolicyLedgerStatus::ApplyBlocked),
        adapter_report_id: Some(format!(
            "memory_policy_apply_blocked:{}",
            validation.proposal_id
        )),
        adapter_report: Some(PolicyAdapterReport::MemoryDryRun(report)),
        routing_report: None,
        blockers: vec![blocker],
        recommendations,
    }
}

fn recovery_policy_blocked_result(
    validation: &PolicyAdapterValidation,
    dry_run: bool,
    blocker: PolicyBlocker,
) -> PolicyAdapterApplyResult {
    let recommendations = vec![
        "Recovery policy changes remain dry-run only until a persistent store is available."
            .to_string(),
    ];
    let report = RecoveryPolicyDryRunReport {
        proposal_id: validation.proposal_id.clone(),
        dry_run,
        status: PolicyLedgerStatus::ApplyBlocked,
        recovery_triggered_tasks: 0,
        recovered_tasks: 0,
        recovery_quality_score: 0.0,
        blockers: vec![blocker.clone()],
        recommendations: recommendations.clone(),
    };
    PolicyAdapterApplyResult {
        status: PolicyLedgerStatus::ApplyBlocked,
        applied: false,
        executed: false,
        after_status: Some(PolicyLedgerStatus::ApplyBlocked),
        adapter_report_id: Some(format!(
            "recovery_policy_apply_blocked:{}",
            validation.proposal_id
        )),
        adapter_report: Some(PolicyAdapterReport::RecoveryDryRun(report)),
        routing_report: None,
        blockers: vec![blocker],
        recommendations,
    }
}

fn planned_only_adapter_descriptor(domain: PolicyDomain) -> PolicyAdapterDescriptor {
    let name = match domain {
        PolicyDomain::Routing => "routing_policy",
        PolicyDomain::AutonomousRun => "autonomous_run_policy",
        PolicyDomain::Scheduler => "scheduler_policy",
        PolicyDomain::Memory => "memory_policy",
        PolicyDomain::Recovery => "recovery_policy",
    };
    PolicyAdapterDescriptor {
        name: name.to_string(),
        domain,
        supports_apply: false,
        supports_persistent_apply: false,
        supports_dry_run: false,
        supports_rollback: false,
        planned_only: true,
        reason: format!(
            "{domain:?} policy remains governance-planned until an adapter is registered"
        ),
    }
}
