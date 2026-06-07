mod adapter;
mod coordinator;
mod ledger;
mod replay;
mod review;
mod types;

#[cfg(test)]
mod tests;

use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

pub use adapter::policy_adapter_descriptors;
pub use coordinator::{
    policy_apply_plan_ledger_entry, policy_apply_report_ledger_entry,
    policy_rollback_report_ledger_entry, PolicyApplyCoordinator,
};
pub use ledger::{
    load_policy_governance_ledger, policy_governance_ledger_path, PolicyGovernanceLedger,
};
pub use replay::{
    filter_policy_lifecycle_replay, replay_policy_lifecycle, replay_policy_lifecycle_entries,
};
pub use review::review_policy_governance;
pub use types::*;

pub const POLICY_GOVERNANCE_VERSION: u32 = 1;
pub const POLICY_GOVERNANCE_LEDGER_FILE: &str = "ledger.jsonl";

pub(super) fn stable_hash_json(value: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

pub(super) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
