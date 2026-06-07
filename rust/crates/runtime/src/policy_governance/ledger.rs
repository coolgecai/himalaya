use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{
    policy_apply_plan_ledger_entry, policy_apply_report_ledger_entry,
    policy_rollback_report_ledger_entry, PolicyApplyPlan, PolicyApplyReport,
    PolicyGovernanceReview, PolicyLedgerEntry, PolicyLedgerLoad, PolicyLedgerWarning,
    PolicyRollbackReport, POLICY_GOVERNANCE_LEDGER_FILE,
};

#[derive(Debug, Clone)]
pub struct PolicyGovernanceLedger {
    dir: PathBuf,
}

impl PolicyGovernanceLedger {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    #[must_use]
    pub fn ledger_path(&self) -> PathBuf {
        policy_governance_ledger_path(&self.dir)
    }

    pub fn record_review(
        &self,
        mut review: PolicyGovernanceReview,
    ) -> io::Result<PolicyGovernanceReview> {
        review.ledger_path = self.ledger_path();
        self.record_entry(review.ledger_entry.clone())?;
        Ok(review)
    }

    pub fn record_entry(&self, entry: PolicyLedgerEntry) -> io::Result<PolicyLedgerEntry> {
        fs::create_dir_all(&self.dir)?;
        let line = serde_json::to_string(&entry)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.ledger_path())?;
        writeln!(file, "{line}")?;
        Ok(entry)
    }

    pub fn record_apply_plan(&self, plan: &PolicyApplyPlan) -> io::Result<PolicyLedgerEntry> {
        self.record_entry(policy_apply_plan_ledger_entry(plan))
    }

    pub fn record_apply_report(&self, report: &PolicyApplyReport) -> io::Result<PolicyLedgerEntry> {
        self.record_entry(policy_apply_report_ledger_entry(report))
    }

    pub fn record_rollback_report(
        &self,
        report: &PolicyRollbackReport,
    ) -> io::Result<PolicyLedgerEntry> {
        self.record_entry(policy_rollback_report_ledger_entry(report))
    }

    pub fn load(&self, limit: usize) -> io::Result<PolicyLedgerLoad> {
        load_policy_governance_ledger(&self.dir, limit)
    }
}

pub fn policy_governance_ledger_path(dir: &Path) -> PathBuf {
    dir.join(POLICY_GOVERNANCE_LEDGER_FILE)
}

pub fn load_policy_governance_ledger(dir: &Path, limit: usize) -> io::Result<PolicyLedgerLoad> {
    let ledger_path = policy_governance_ledger_path(dir);
    if !ledger_path.exists() {
        return Ok(PolicyLedgerLoad {
            ledger_path,
            entries: Vec::new(),
            malformed_lines: 0,
            warnings: Vec::new(),
        });
    }
    let contents = fs::read_to_string(&ledger_path)?;
    let mut entries = Vec::new();
    let mut malformed_lines = 0_usize;
    let mut warnings = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<PolicyLedgerEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(error) => {
                malformed_lines += 1;
                warnings.push(PolicyLedgerWarning {
                    line: index.saturating_add(1),
                    message: error.to_string(),
                });
            }
        }
    }
    if limit > 0 && entries.len() > limit {
        let start = entries.len().saturating_sub(limit);
        entries = entries.into_iter().skip(start).collect();
    }
    Ok(PolicyLedgerLoad {
        ledger_path,
        entries,
        malformed_lines,
        warnings,
    })
}
