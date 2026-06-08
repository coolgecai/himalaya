use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use super::{
    load_policy_governance_ledger, PolicyApplyOperation, PolicyDomain, PolicyLedgerEntry,
    PolicyLedgerLoad, PolicyLedgerStatus, PolicyLifecycle, PolicyLifecycleEvent,
    PolicyLifecycleReplay, PolicyLifecycleReplaySummary, PolicyReplayAnomaly,
    POLICY_GOVERNANCE_VERSION,
};

pub fn replay_policy_lifecycle(dir: &Path, limit: usize) -> io::Result<PolicyLifecycleReplay> {
    let load = load_policy_governance_ledger(dir, limit)?;
    Ok(replay_policy_lifecycle_entries(load))
}

pub fn replay_policy_lifecycle_entries(load: PolicyLedgerLoad) -> PolicyLifecycleReplay {
    let mut lifecycles = BTreeMap::<(PolicyDomain, String), PolicyLifecycle>::new();
    let mut anomalies = Vec::new();
    let mut event_count = 0_usize;

    for entry in &load.entries {
        let operation = policy_entry_operation(entry);
        for proposal in &entry.proposals {
            let key = (proposal.domain, proposal.id.clone());
            let lifecycle = lifecycles
                .entry(key.clone())
                .or_insert_with(|| PolicyLifecycle {
                    domain: proposal.domain,
                    proposal_id: proposal.id.clone(),
                    current_status: entry.status,
                    first_seen_at: entry.timestamp,
                    last_seen_at: entry.timestamp,
                    events: Vec::new(),
                });
            if entry.timestamp < lifecycle.last_seen_at {
                anomalies.push(PolicyReplayAnomaly {
                    entry_id: entry.id.clone(),
                    proposal_id: Some(proposal.id.clone()),
                    kind: "out_of_order".to_string(),
                    reason: "ledger entry timestamp is older than the previous lifecycle event"
                        .to_string(),
                });
            }
            if !lifecycle.events.is_empty() && lifecycle.current_status == entry.status {
                anomalies.push(PolicyReplayAnomaly {
                    entry_id: entry.id.clone(),
                    proposal_id: Some(proposal.id.clone()),
                    kind: "duplicate_status".to_string(),
                    reason: format!("status {:?} repeated without transition", entry.status),
                });
            }
            let seen = lifecycle
                .events
                .iter()
                .map(|event| event.status)
                .collect::<Vec<_>>();
            if entry.status == PolicyLedgerStatus::Applied
                && !seen.iter().any(|status| {
                    matches!(
                        status,
                        PolicyLedgerStatus::Planned
                            | PolicyLedgerStatus::Approved
                            | PolicyLedgerStatus::Proposed
                    )
                })
            {
                anomalies.push(PolicyReplayAnomaly {
                    entry_id: entry.id.clone(),
                    proposal_id: Some(proposal.id.clone()),
                    kind: "apply_without_plan".to_string(),
                    reason: "persistent apply event appeared before an observed plan/proposal"
                        .to_string(),
                });
            }
            if entry.status == PolicyLedgerStatus::RolledBack
                && !seen.contains(&PolicyLedgerStatus::Applied)
            {
                anomalies.push(PolicyReplayAnomaly {
                    entry_id: entry.id.clone(),
                    proposal_id: Some(proposal.id.clone()),
                    kind: "rollback_without_applied".to_string(),
                    reason: "rollback appeared before an applied lifecycle state".to_string(),
                });
            }
            if lifecycle.current_status == PolicyLedgerStatus::RolledBack
                && matches!(
                    entry.status,
                    PolicyLedgerStatus::Applied
                        | PolicyLedgerStatus::Planned
                        | PolicyLedgerStatus::DryRunPassed
                )
            {
                anomalies.push(PolicyReplayAnomaly {
                    entry_id: entry.id.clone(),
                    proposal_id: Some(proposal.id.clone()),
                    kind: "reopened_after_rollback".to_string(),
                    reason: "proposal lifecycle continued after rollback".to_string(),
                });
            }
            lifecycle.current_status = entry.status;
            lifecycle.last_seen_at = lifecycle.last_seen_at.max(entry.timestamp);
            lifecycle.events.push(PolicyLifecycleEvent {
                entry_id: entry.id.clone(),
                timestamp: entry.timestamp,
                status: entry.status,
                action: proposal.action.clone(),
                operation,
            });
            event_count += 1;
        }
    }

    let mut lifecycles = lifecycles.into_values().collect::<Vec<_>>();
    lifecycles.sort_by(|left, right| {
        left.domain
            .cmp(&right.domain)
            .then_with(|| left.proposal_id.cmp(&right.proposal_id))
    });
    let summary = summarize_policy_lifecycle_replay(
        lifecycles.as_slice(),
        anomalies.as_slice(),
        event_count,
        load.malformed_lines,
    );
    PolicyLifecycleReplay {
        version: POLICY_GOVERNANCE_VERSION,
        ledger_path: load.ledger_path,
        entries_considered: load.entries.len(),
        malformed_lines: load.malformed_lines,
        summary,
        lifecycles,
        anomalies,
    }
}

#[must_use]
pub fn filter_policy_lifecycle_replay(
    replay: &PolicyLifecycleReplay,
    domain: Option<PolicyDomain>,
    action_contains: Option<&str>,
) -> PolicyLifecycleReplay {
    let lifecycles = replay
        .lifecycles
        .iter()
        .filter(|lifecycle| {
            domain.is_none_or(|wanted| lifecycle.domain == wanted)
                && action_contains.is_none_or(|wanted| {
                    lifecycle
                        .events
                        .iter()
                        .any(|event| event.action.contains(wanted))
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    let proposal_ids = lifecycles
        .iter()
        .map(|lifecycle| lifecycle.proposal_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let anomalies = replay
        .anomalies
        .iter()
        .filter(|anomaly| {
            anomaly
                .proposal_id
                .as_deref()
                .is_none_or(|proposal_id| proposal_ids.contains(proposal_id))
        })
        .cloned()
        .collect::<Vec<_>>();
    let event_count = lifecycles
        .iter()
        .map(|lifecycle| lifecycle.events.len())
        .sum::<usize>();
    let summary = summarize_policy_lifecycle_replay(
        lifecycles.as_slice(),
        anomalies.as_slice(),
        event_count,
        replay.malformed_lines,
    );
    PolicyLifecycleReplay {
        version: replay.version,
        ledger_path: replay.ledger_path.clone(),
        entries_considered: replay.entries_considered,
        malformed_lines: replay.malformed_lines,
        lifecycles,
        anomalies,
        summary,
    }
}

fn summarize_policy_lifecycle_replay(
    lifecycles: &[PolicyLifecycle],
    anomalies: &[PolicyReplayAnomaly],
    event_count: usize,
    malformed_lines: usize,
) -> PolicyLifecycleReplaySummary {
    let mut domain_counts = BTreeMap::new();
    let mut action_counts = BTreeMap::new();
    for lifecycle in lifecycles {
        *domain_counts.entry(lifecycle.domain).or_insert(0) += 1;
        for event in &lifecycle.events {
            *action_counts.entry(event.action.clone()).or_insert(0) += 1;
        }
    }
    let mut anomaly_kind_counts = BTreeMap::new();
    for anomaly in anomalies {
        *anomaly_kind_counts.entry(anomaly.kind.clone()).or_insert(0) += 1;
    }
    PolicyLifecycleReplaySummary {
        lifecycle_count: lifecycles.len(),
        event_count,
        anomaly_count: anomalies.len(),
        malformed_lines,
        domain_counts,
        action_counts,
        anomaly_kind_counts,
    }
}

fn policy_entry_operation(entry: &PolicyLedgerEntry) -> Option<PolicyApplyOperation> {
    entry.decisions.iter().find_map(|decision| {
        if decision.action.contains("rollback") {
            Some(PolicyApplyOperation::Rollback)
        } else if decision.action.contains("apply") {
            Some(PolicyApplyOperation::Apply)
        } else {
            None
        }
    })
}
