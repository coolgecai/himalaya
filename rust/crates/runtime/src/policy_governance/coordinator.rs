use std::io;

use serde_json::json;

use crate::{
    AutonomousEvaluationReport, RoutingPolicyProposal, RoutingPolicyProposalStore,
    SchedulerDaemonState,
};

use super::adapter::{PolicyAdapterContext, PolicyAdapterRegistry};
use super::review::routing_policy_proposal;
use super::{
    now_millis, now_secs, policy_adapter_descriptors, stable_hash_json, PolicyActionReceipt,
    PolicyApplyAction, PolicyApplyOperation, PolicyApplyPlan, PolicyApplyReport, PolicyBlocker,
    PolicyBlockerKind, PolicyDecision, PolicyDomain, PolicyGovernanceReview, PolicyLedgerEntry,
    PolicyLedgerStatus, PolicyProposal, PolicyReviewSummary, PolicyRiskLevel, PolicyRollbackReport,
    POLICY_GOVERNANCE_VERSION,
};

#[derive(Debug, Clone)]
pub struct PolicyApplyCoordinator {
    routing_store: RoutingPolicyProposalStore,
    scheduler_state: Option<SchedulerDaemonState>,
    autonomous_evaluation: Option<AutonomousEvaluationReport>,
}

impl PolicyApplyCoordinator {
    #[must_use]
    pub fn new(routing_store: RoutingPolicyProposalStore) -> Self {
        Self {
            routing_store,
            scheduler_state: None,
            autonomous_evaluation: None,
        }
    }

    #[must_use]
    pub fn with_scheduler_state(mut self, scheduler_state: Option<SchedulerDaemonState>) -> Self {
        self.scheduler_state = scheduler_state;
        self
    }

    #[must_use]
    pub fn with_autonomous_evaluation(
        mut self,
        autonomous_evaluation: Option<AutonomousEvaluationReport>,
    ) -> Self {
        self.autonomous_evaluation = autonomous_evaluation;
        self
    }

    #[must_use]
    pub fn plan_apply(
        &self,
        review: PolicyGovernanceReview,
        domain_filter: Option<PolicyDomain>,
        proposal_id: Option<&str>,
        dry_run: bool,
    ) -> PolicyApplyPlan {
        build_policy_apply_plan(
            review,
            PolicyApplyOperation::Apply,
            domain_filter,
            proposal_id,
            dry_run,
        )
    }

    #[must_use]
    pub fn plan_rollback(
        &self,
        review: PolicyGovernanceReview,
        domain_filter: Option<PolicyDomain>,
        proposal_id: Option<&str>,
    ) -> PolicyApplyPlan {
        build_policy_apply_plan(
            review,
            PolicyApplyOperation::Rollback,
            domain_filter,
            proposal_id,
            false,
        )
    }

    pub fn apply(&self, plan: &PolicyApplyPlan) -> io::Result<PolicyApplyReport> {
        if plan.operation != PolicyApplyOperation::Apply {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "policy apply requires an apply plan",
            ));
        }
        let mut structured_blockers = plan.structured_blockers.clone();
        structured_blockers.extend(
            plan.actions
                .iter()
                .flat_map(|action| action.structured_blockers.clone()),
        );
        if !plan.dry_run && (plan.domain_filter.is_none() || plan.proposal_id.is_none()) {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::MissingTarget,
                plan.domain_filter,
                plan.proposal_id.clone(),
                PolicyRiskLevel::High,
                "governed policy apply requires explicit --domain and --proposal-id".to_string(),
                "governance_apply",
            ));
        }
        let executable_actions = executable_policy_actions(plan);
        if executable_actions.is_empty() {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::MissingTarget,
                plan.domain_filter,
                plan.proposal_id.clone(),
                PolicyRiskLevel::High,
                "no executable policy apply action is available",
                "governance_apply",
            ));
        } else if executable_actions.len() > 1 {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::AmbiguousTarget,
                plan.domain_filter,
                None,
                PolicyRiskLevel::High,
                "multiple executable policy actions are available; specify --domain and --proposal-id"
                    .to_string(),
                "governance_apply",
            ));
        }
        if !structured_blockers.is_empty() {
            return Ok(blocked_apply_report(
                plan,
                structured_blockers,
                "Resolve governance blockers before applying policy changes.",
            ));
        }

        let action = executable_actions[0];
        let registry = self.adapter_registry();
        let Some(adapter) = registry.adapter_for_domain(action.domain) else {
            return Ok(blocked_apply_report(
                plan,
                vec![policy_blocker(
                    PolicyBlockerKind::UnsupportedDomain,
                    Some(action.domain),
                    action.proposal_id.clone(),
                    PolicyRiskLevel::High,
                    format!("{:?} policy adapter is not registered", action.domain),
                    "policy_adapter_registry",
                )],
                "Register a policy adapter before applying this domain.",
            ));
        };
        let descriptor = adapter.descriptor();
        let validated = match adapter.validate(action, PolicyApplyOperation::Apply)? {
            Ok(validated) => validated,
            Err(blockers) => {
                return Ok(blocked_apply_report(
                    plan,
                    blockers,
                    "Refresh the policy plan before applying changed routing policy state.",
                ));
            }
        };
        let adapter_result = adapter.apply(&validated, plan.dry_run)?;
        let blockers = blocker_messages(&adapter_result.blockers);
        let report_id = format!("policy-apply-{}", now_millis());
        let receipt = policy_action_receipt(
            report_id.clone(),
            plan,
            adapter_result.status,
            adapter_result.executed,
            Some(validated.before_status),
            adapter_result.after_status,
            Some(descriptor.name),
            adapter_result.adapter_report_id.clone(),
            adapter_result.blockers.clone(),
            adapter_result.recommendations.clone(),
        );
        Ok(PolicyApplyReport {
            version: POLICY_GOVERNANCE_VERSION,
            id: report_id,
            executed_at: now_secs(),
            dry_run: plan.dry_run,
            status: adapter_result.status,
            applied: adapter_result.applied,
            plan: plan.clone(),
            adapter_report: adapter_result.adapter_report,
            routing_report: adapter_result.routing_report,
            blockers,
            structured_blockers: adapter_result.blockers,
            receipt,
            recommendations: adapter_result.recommendations,
        })
    }

    pub fn rollback(&self, plan: &PolicyApplyPlan) -> io::Result<PolicyRollbackReport> {
        if plan.operation != PolicyApplyOperation::Rollback {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "policy rollback requires a rollback plan",
            ));
        }
        let mut structured_blockers = plan.structured_blockers.clone();
        structured_blockers.extend(
            plan.actions
                .iter()
                .flat_map(|action| action.structured_blockers.clone()),
        );
        if plan.domain_filter != Some(PolicyDomain::Routing) || plan.proposal_id.is_none() {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::MissingTarget,
                Some(PolicyDomain::Routing),
                plan.proposal_id.clone(),
                PolicyRiskLevel::High,
                "governed policy rollback requires explicit --domain routing and --proposal-id"
                    .to_string(),
                "governance_rollback",
            ));
        }
        let executable_actions = executable_policy_actions(plan);
        if executable_actions.is_empty() {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::MissingTarget,
                plan.domain_filter,
                plan.proposal_id.clone(),
                PolicyRiskLevel::High,
                "no executable policy rollback action is available",
                "governance_rollback",
            ));
        } else if executable_actions.len() > 1 {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::AmbiguousTarget,
                plan.domain_filter,
                None,
                PolicyRiskLevel::High,
                "multiple executable policy rollback actions are available; specify --domain and --proposal-id"
                    .to_string(),
                "governance_rollback",
            ));
        }
        if !structured_blockers.is_empty() {
            return Ok(blocked_rollback_report(
                plan,
                structured_blockers,
                "Resolve governance blockers before rolling back policy changes.",
            ));
        }

        let action = executable_actions[0];
        let registry = self.adapter_registry();
        let Some(adapter) = registry.adapter_for_domain(action.domain) else {
            return Ok(blocked_rollback_report(
                plan,
                vec![policy_blocker(
                    PolicyBlockerKind::UnsupportedDomain,
                    Some(action.domain),
                    action.proposal_id.clone(),
                    PolicyRiskLevel::High,
                    format!("{:?} policy adapter is not registered", action.domain),
                    "policy_adapter_registry",
                )],
                "Register a policy adapter before rolling back this domain.",
            ));
        };
        let descriptor = adapter.descriptor();
        let validated = match adapter.validate(action, PolicyApplyOperation::Rollback)? {
            Ok(validated) => validated,
            Err(blockers) => {
                return Ok(blocked_rollback_report(
                    plan,
                    blockers,
                    "Refresh the policy plan before rolling back changed routing policy state.",
                ));
            }
        };
        let adapter_result = adapter.rollback(&validated)?;
        let blockers = blocker_messages(&adapter_result.blockers);
        let report_id = format!("policy-rollback-{}", now_millis());
        let receipt = policy_action_receipt(
            report_id.clone(),
            plan,
            adapter_result.status,
            adapter_result.executed,
            Some(validated.before_status),
            adapter_result.after_status,
            Some(descriptor.name),
            adapter_result.adapter_report_id.clone(),
            adapter_result.blockers.clone(),
            adapter_result.recommendations.clone(),
        );
        Ok(PolicyRollbackReport {
            version: POLICY_GOVERNANCE_VERSION,
            id: report_id,
            executed_at: now_secs(),
            status: adapter_result.status,
            rolled_back: adapter_result.rolled_back,
            plan: plan.clone(),
            adapter_report: adapter_result.adapter_report,
            routing_report: adapter_result.routing_report,
            blockers,
            structured_blockers: adapter_result.blockers,
            receipt,
            recommendations: adapter_result.recommendations,
        })
    }

    fn adapter_registry(&self) -> PolicyAdapterRegistry<'_> {
        PolicyAdapterRegistry::new(PolicyAdapterContext {
            routing_store: &self.routing_store,
            scheduler_state: self.scheduler_state.as_ref(),
            autonomous_evaluation: self.autonomous_evaluation.as_ref(),
        })
    }
}

pub fn policy_apply_plan_ledger_entry(plan: &PolicyApplyPlan) -> PolicyLedgerEntry {
    policy_apply_ledger_entry(
        plan.id.clone(),
        plan.planned_at,
        plan.status,
        plan,
        plan.blockers.clone(),
        plan.recommendations.clone(),
    )
}

pub fn policy_apply_report_ledger_entry(report: &PolicyApplyReport) -> PolicyLedgerEntry {
    policy_apply_ledger_entry(
        report.id.clone(),
        report.executed_at,
        report.status,
        &report.plan,
        report.blockers.clone(),
        report.recommendations.clone(),
    )
}

pub fn policy_rollback_report_ledger_entry(report: &PolicyRollbackReport) -> PolicyLedgerEntry {
    policy_apply_ledger_entry(
        report.id.clone(),
        report.executed_at,
        report.status,
        &report.plan,
        report.blockers.clone(),
        report.recommendations.clone(),
    )
}

fn build_policy_apply_plan(
    review: PolicyGovernanceReview,
    operation: PolicyApplyOperation,
    domain_filter: Option<PolicyDomain>,
    proposal_id: Option<&str>,
    dry_run: bool,
) -> PolicyApplyPlan {
    let proposal_id = proposal_id.map(str::to_string);
    let adapters = policy_adapter_descriptors(review.ledger_entry.summary.scheduler_status);
    let mut structured_blockers = Vec::new();
    if operation == PolicyApplyOperation::Apply {
        structured_blockers.extend(
            review
                .ledger_entry
                .gates
                .iter()
                .filter(|gate| !gate.passed && gate.blocks_apply)
                .map(|gate| {
                    policy_blocker(
                        PolicyBlockerKind::GateFailed,
                        Some(gate.domain),
                        None,
                        gate.severity,
                        format!("{} gate failed: {}", gate.name, gate.reason),
                        "governance_gate",
                    )
                }),
        );
        structured_blockers.extend(
            review
                .ledger_entry
                .conflicts
                .iter()
                .filter(|conflict| conflict.blocks_apply)
                .map(|conflict| {
                    policy_blocker(
                        PolicyBlockerKind::ConflictBlocked,
                        conflict.domains.first().copied(),
                        None,
                        conflict.severity,
                        format!("blocking policy conflict: {}", conflict.reason),
                        "governance_conflict",
                    )
                }),
        );
    }

    let mut actions = Vec::new();
    for proposal in &review.ledger_entry.proposals {
        if !domain_filter.map_or(true, |domain| domain == proposal.domain) {
            continue;
        }
        if !proposal_id
            .as_deref()
            .map_or(true, |wanted| wanted == proposal.id)
        {
            continue;
        }
        actions.push(policy_apply_action_from_proposal(proposal, operation));
    }

    if operation == PolicyApplyOperation::Apply {
        for decision in &review.ledger_entry.decisions {
            if proposal_id.is_some() {
                continue;
            }
            if !matches!(
                decision.status,
                PolicyLedgerStatus::Proposed | PolicyLedgerStatus::Approved
            ) {
                continue;
            }
            if !domain_filter.map_or(true, |domain| domain == decision.domain) {
                continue;
            }
            actions.push(policy_apply_action_from_decision(decision, operation));
        }
    }

    if let Some(wanted) = &proposal_id {
        if !actions
            .iter()
            .any(|action| action.proposal_id.as_deref() == Some(wanted.as_str()))
        {
            structured_blockers.push(policy_blocker(
                PolicyBlockerKind::MissingProposal,
                domain_filter,
                Some(wanted.clone()),
                PolicyRiskLevel::High,
                format!("policy proposal not found in governance review: {wanted}"),
                "governance_review",
            ));
        }
    }

    let blockers = blocker_messages(&structured_blockers);
    let has_executable = actions
        .iter()
        .any(|action| action.executable && action.blockers.is_empty());
    let status = if !blockers.is_empty() {
        match operation {
            PolicyApplyOperation::Apply => PolicyLedgerStatus::ApplyBlocked,
            PolicyApplyOperation::Rollback => PolicyLedgerStatus::RollbackBlocked,
        }
    } else if has_executable || !actions.is_empty() {
        match operation {
            PolicyApplyOperation::Apply => PolicyLedgerStatus::Planned,
            PolicyApplyOperation::Rollback => PolicyLedgerStatus::RollbackPlanned,
        }
    } else {
        PolicyLedgerStatus::Observed
    };

    let mut recommendations = Vec::new();
    if blockers.is_empty() {
        match operation {
            PolicyApplyOperation::Apply if has_executable && dry_run => recommendations.push(
                "Governed dry run is ready; no policy overlay will be persisted.".to_string(),
            ),
            PolicyApplyOperation::Apply if has_executable => {
                recommendations.push("Governed policy apply is ready for execution.".to_string())
            }
            PolicyApplyOperation::Rollback if has_executable => {
                recommendations.push("Governed policy rollback is ready for execution.".to_string())
            }
            _ => {
                recommendations.push("No executable governed policy action is pending.".to_string())
            }
        }
    } else {
        recommendations
            .push("Resolve apply blockers before executing governed policy actions.".to_string());
    }

    let mut plan = PolicyApplyPlan {
        version: POLICY_GOVERNANCE_VERSION,
        id: format!("policy-plan-{}", now_millis()),
        planned_at: now_secs(),
        operation,
        dry_run,
        status,
        review_status: review.ledger_entry.status,
        domain_filter,
        proposal_id,
        actions,
        adapters,
        blockers,
        structured_blockers,
        fingerprint: String::new(),
        recommendations,
        review,
    };
    plan.fingerprint = policy_plan_fingerprint(&plan);
    plan
}

pub(super) fn policy_apply_action_from_proposal(
    proposal: &PolicyProposal,
    operation: PolicyApplyOperation,
) -> PolicyApplyAction {
    let mut structured_blockers = Vec::new();
    if operation == PolicyApplyOperation::Apply {
        structured_blockers.extend(
            proposal
                .gates
                .iter()
                .filter(|gate| !gate.passed && gate.blocks_apply)
                .map(|gate| {
                    policy_blocker(
                        PolicyBlockerKind::GateFailed,
                        Some(gate.domain),
                        Some(proposal.id.clone()),
                        gate.severity,
                        format!("{} gate failed: {}", gate.name, gate.reason),
                        "proposal_gate",
                    )
                }),
        );
    }

    let (action, status, executable) = match operation {
        PolicyApplyOperation::Apply => {
            if !matches!(
                proposal.status,
                PolicyLedgerStatus::Proposed | PolicyLedgerStatus::Approved
            ) {
                structured_blockers.push(policy_blocker(
                    PolicyBlockerKind::AdapterRejected,
                    Some(proposal.domain),
                    Some(proposal.id.clone()),
                    PolicyRiskLevel::High,
                    format!("proposal status {:?} cannot be applied", proposal.status),
                    "proposal_status",
                ));
            }
            let executable = policy_operation_supports_domain(operation, proposal.domain);
            (
                proposal.action.clone(),
                if structured_blockers.is_empty() && executable {
                    PolicyLedgerStatus::Planned
                } else {
                    PolicyLedgerStatus::ApplyBlocked
                },
                executable,
            )
        }
        PolicyApplyOperation::Rollback => {
            if proposal.status != PolicyLedgerStatus::Applied {
                structured_blockers.push(policy_blocker(
                    PolicyBlockerKind::AdapterRejected,
                    Some(proposal.domain),
                    Some(proposal.id.clone()),
                    PolicyRiskLevel::High,
                    format!(
                        "proposal status {:?} cannot be rolled back",
                        proposal.status
                    ),
                    "proposal_status",
                ));
            }
            let executable = policy_operation_supports_domain(operation, proposal.domain);
            (
                "rollback_routing_policy_overlay".to_string(),
                if structured_blockers.is_empty() && executable {
                    PolicyLedgerStatus::RollbackPlanned
                } else {
                    PolicyLedgerStatus::RollbackBlocked
                },
                executable,
            )
        }
    };

    if !executable {
        structured_blockers.push(policy_blocker(
            PolicyBlockerKind::UnsupportedDomain,
            Some(proposal.domain),
            Some(proposal.id.clone()),
            PolicyRiskLevel::Medium,
            format!(
                "{:?} policy action is governance-planned only for {:?}",
                proposal.domain, operation
            ),
            "policy_adapter",
        ));
    }

    let blockers = blocker_messages(&structured_blockers);
    PolicyApplyAction {
        domain: proposal.domain,
        operation,
        action,
        proposal_id: Some(proposal.id.clone()),
        status,
        risk: proposal.risk,
        executable,
        blockers,
        structured_blockers,
        source_fingerprint: if proposal.source_fingerprint.is_empty() {
            policy_proposal_fingerprint(proposal)
        } else {
            proposal.source_fingerprint.clone()
        },
        reason: proposal.summary.clone(),
        references: proposal.references.clone(),
    }
}

fn policy_apply_action_from_decision(
    decision: &PolicyDecision,
    operation: PolicyApplyOperation,
) -> PolicyApplyAction {
    let status = match operation {
        PolicyApplyOperation::Apply => PolicyLedgerStatus::ApplyBlocked,
        PolicyApplyOperation::Rollback => PolicyLedgerStatus::RollbackBlocked,
    };
    let structured_blockers = vec![policy_blocker(
        PolicyBlockerKind::UnsupportedDomain,
        Some(decision.domain),
        None,
        decision.risk,
        format!(
            "{:?} decision '{}' is governance-planned only; no executable adapter is registered",
            decision.domain, decision.action
        ),
        "policy_adapter",
    )];
    PolicyApplyAction {
        domain: decision.domain,
        operation,
        action: decision.action.clone(),
        proposal_id: None,
        status,
        risk: decision.risk,
        executable: false,
        blockers: blocker_messages(&structured_blockers),
        structured_blockers,
        source_fingerprint: stable_hash_json(&json!({
            "domain": decision.domain,
            "action": decision.action,
            "status": decision.status,
            "reason": decision.reason,
        })),
        reason: decision.reason.clone(),
        references: Vec::new(),
    }
}

fn policy_operation_supports_domain(operation: PolicyApplyOperation, domain: PolicyDomain) -> bool {
    matches!(
        (operation, domain),
        (PolicyApplyOperation::Apply, PolicyDomain::Routing)
            | (PolicyApplyOperation::Apply, PolicyDomain::Scheduler)
            | (PolicyApplyOperation::Apply, PolicyDomain::Memory)
            | (PolicyApplyOperation::Apply, PolicyDomain::Recovery)
            | (PolicyApplyOperation::Rollback, PolicyDomain::Routing)
    )
}

fn executable_policy_actions(plan: &PolicyApplyPlan) -> Vec<&PolicyApplyAction> {
    plan.actions
        .iter()
        .filter(|action| action.executable && action.blockers.is_empty())
        .collect()
}

fn blocked_apply_report(
    plan: &PolicyApplyPlan,
    structured_blockers: Vec<PolicyBlocker>,
    recommendation: &str,
) -> PolicyApplyReport {
    let report_id = format!("policy-apply-{}", now_millis());
    let blockers = blocker_messages(&structured_blockers);
    let recommendations = vec![recommendation.to_string()];
    let receipt = policy_action_receipt(
        report_id.clone(),
        plan,
        PolicyLedgerStatus::ApplyBlocked,
        false,
        None,
        None,
        None,
        None,
        structured_blockers.clone(),
        recommendations.clone(),
    );
    PolicyApplyReport {
        version: POLICY_GOVERNANCE_VERSION,
        id: report_id,
        executed_at: now_secs(),
        dry_run: plan.dry_run,
        status: PolicyLedgerStatus::ApplyBlocked,
        applied: false,
        plan: plan.clone(),
        adapter_report: None,
        routing_report: None,
        blockers,
        structured_blockers,
        receipt,
        recommendations,
    }
}

fn blocked_rollback_report(
    plan: &PolicyApplyPlan,
    structured_blockers: Vec<PolicyBlocker>,
    recommendation: &str,
) -> PolicyRollbackReport {
    let report_id = format!("policy-rollback-{}", now_millis());
    let blockers = blocker_messages(&structured_blockers);
    let recommendations = vec![recommendation.to_string()];
    let receipt = policy_action_receipt(
        report_id.clone(),
        plan,
        PolicyLedgerStatus::RollbackBlocked,
        false,
        None,
        None,
        None,
        None,
        structured_blockers.clone(),
        recommendations.clone(),
    );
    PolicyRollbackReport {
        version: POLICY_GOVERNANCE_VERSION,
        id: report_id,
        executed_at: now_secs(),
        status: PolicyLedgerStatus::RollbackBlocked,
        rolled_back: false,
        plan: plan.clone(),
        adapter_report: None,
        routing_report: None,
        blockers,
        structured_blockers,
        receipt,
        recommendations,
    }
}

fn policy_action_receipt(
    id: String,
    plan: &PolicyApplyPlan,
    status: PolicyLedgerStatus,
    executed: bool,
    before_status: Option<PolicyLedgerStatus>,
    after_status: Option<PolicyLedgerStatus>,
    adapter: Option<String>,
    adapter_report_id: Option<String>,
    blockers: Vec<PolicyBlocker>,
    recommendations: Vec<String>,
) -> PolicyActionReceipt {
    PolicyActionReceipt {
        version: POLICY_GOVERNANCE_VERSION,
        id: format!("{id}-receipt"),
        plan_id: plan.id.clone(),
        plan_fingerprint: plan.fingerprint.clone(),
        operation: plan.operation,
        dry_run: plan.dry_run,
        domain: plan.domain_filter,
        proposal_id: plan.proposal_id.clone(),
        status,
        executed,
        before_status,
        after_status,
        adapter,
        adapter_report_id,
        ledger_entry_id: Some(id),
        blockers,
        recommendations,
    }
}

pub(super) fn policy_blocker(
    kind: PolicyBlockerKind,
    domain: Option<PolicyDomain>,
    proposal_id: Option<String>,
    severity: PolicyRiskLevel,
    reason: impl Into<String>,
    source: impl Into<String>,
) -> PolicyBlocker {
    PolicyBlocker {
        kind,
        domain,
        proposal_id,
        severity,
        reason: reason.into(),
        source: source.into(),
    }
}

pub(super) fn blocker_messages(blockers: &[PolicyBlocker]) -> Vec<String> {
    blockers
        .iter()
        .map(|blocker| blocker.reason.clone())
        .collect()
}

fn policy_plan_fingerprint(plan: &PolicyApplyPlan) -> String {
    stable_hash_json(&json!({
        "version": plan.version,
        "operation": plan.operation,
        "dry_run": plan.dry_run,
        "review_id": plan.review.ledger_entry.id,
        "review_status": plan.review_status,
        "domain_filter": plan.domain_filter,
        "proposal_id": plan.proposal_id,
        "actions": plan.actions.iter().map(|action| {
            json!({
                "domain": action.domain,
                "operation": action.operation,
                "proposal_id": action.proposal_id,
                "status": action.status,
                "source_fingerprint": action.source_fingerprint,
            })
        }).collect::<Vec<_>>(),
        "adapters": plan.adapters,
        "blockers": plan.structured_blockers,
    }))
}

fn policy_proposal_fingerprint(proposal: &PolicyProposal) -> String {
    stable_hash_json(&json!({
        "id": proposal.id,
        "domain": proposal.domain,
        "action": proposal.action,
        "status": proposal.status,
        "risk": proposal.risk,
        "summary": proposal.summary,
        "gates": proposal.gates,
        "references": proposal.references,
    }))
}

pub(super) fn routing_action_snapshot(
    proposal: &RoutingPolicyProposal,
    operation: PolicyApplyOperation,
) -> Option<PolicyApplyAction> {
    let policy_proposal = routing_policy_proposal(proposal);
    Some(policy_apply_action_from_proposal(
        &policy_proposal,
        operation,
    ))
}

fn policy_apply_ledger_entry(
    id: String,
    timestamp: u64,
    status: PolicyLedgerStatus,
    plan: &PolicyApplyPlan,
    outcome_blockers: Vec<String>,
    recommendations: Vec<String>,
) -> PolicyLedgerEntry {
    let proposals = plan
        .actions
        .iter()
        .enumerate()
        .map(|(index, action)| PolicyProposal {
            id: action
                .proposal_id
                .clone()
                .unwrap_or_else(|| format!("{}-action-{}", plan.id, index.saturating_add(1))),
            domain: action.domain,
            action: action.action.clone(),
            status: action.status,
            risk: action.risk,
            summary: action.reason.clone(),
            source_fingerprint: action.source_fingerprint.clone(),
            references: action.references.clone(),
            gates: Vec::new(),
        })
        .collect::<Vec<_>>();
    let decisions = vec![PolicyDecision {
        domain: plan.domain_filter.unwrap_or(PolicyDomain::Routing),
        action: match plan.operation {
            PolicyApplyOperation::Apply if plan.dry_run => "governed_policy_apply_dry_run",
            PolicyApplyOperation::Apply => "governed_policy_apply",
            PolicyApplyOperation::Rollback => "governed_policy_rollback",
        }
        .to_string(),
        status,
        risk: plan
            .actions
            .iter()
            .map(|action| action.risk)
            .max()
            .unwrap_or(PolicyRiskLevel::Low),
        reason: if outcome_blockers.is_empty() {
            "governed policy coordinator recorded the action outcome".to_string()
        } else {
            outcome_blockers.join("; ")
        },
    }];
    let mut domains = plan
        .actions
        .iter()
        .map(|action| action.domain)
        .collect::<Vec<_>>();
    if let Some(domain) = plan.domain_filter {
        domains.push(domain);
    }
    domains.extend(
        plan.review
            .ledger_entry
            .conflicts
            .iter()
            .flat_map(|conflict| conflict.domains.clone()),
    );
    domains.sort();
    domains.dedup();
    let failed_gate_count = plan
        .review
        .ledger_entry
        .gates
        .iter()
        .filter(|gate| !gate.passed)
        .count();
    let blocking_conflict_count = plan
        .review
        .ledger_entry
        .conflicts
        .iter()
        .filter(|conflict| conflict.blocks_apply)
        .count();
    PolicyLedgerEntry {
        version: POLICY_GOVERNANCE_VERSION,
        id,
        timestamp,
        status,
        domains,
        proposals,
        decisions,
        gates: plan.review.ledger_entry.gates.clone(),
        conflicts: plan.review.ledger_entry.conflicts.clone(),
        recommendations,
        summary: PolicyReviewSummary {
            status,
            proposal_count: plan.actions.len(),
            decision_count: 1,
            gate_count: plan.review.ledger_entry.gates.len(),
            failed_gate_count,
            conflict_count: plan.review.ledger_entry.conflicts.len(),
            blocking_conflict_count,
            active_routing_policy: plan.review.ledger_entry.summary.active_routing_policy,
            routing_proposal_count: plan.review.ledger_entry.summary.routing_proposal_count,
            scheduler_status: plan.review.ledger_entry.summary.scheduler_status,
            autonomous_action: plan.review.ledger_entry.summary.autonomous_action,
            autonomous_review_required: plan.review.ledger_entry.summary.autonomous_review_required,
            memory_reuse_score: plan.review.ledger_entry.summary.memory_reuse_score,
        },
    }
}
