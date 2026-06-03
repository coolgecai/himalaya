//! Multi-role convergence: run a structured-execution node through an
//! Architect -> Executor -> Reviewer loop that converges on an approved result
//! or exhausts a bounded number of rounds. This is the mechanism that turns the
//! "humans set direction; agents coordinate, argue, and converge" philosophy
//! into a concrete loop on top of the structured execution pipeline.
//!
//! Built in stages and entirely opt-in (gated by
//! `DecisioningConfig::team_convergence_threshold`). Stage 0 defines the role
//! model and outcome types; Stage 1 adds the pure state machine; Stage 2 wires
//! the roles to real model sub-turns; Stage 3 fuses with recovery and the team
//! ledger; Stage 4 adds role-level adaptive routing.
//!
//! The roles map onto the existing [`crate::TeamRole`] so events flow through
//! the established `TeamExecutionLedger`:
//! - Architect  = `TeamRole::Planner`   (Planning route): sets the approach.
//! - Executor   = `TeamRole::Implementer` (Coding route): produces the result.
//! - Reviewer   = `TeamRole::Reviewer`  (Verification route): approves or asks
//!   for changes.

use crate::TeamRole;

/// The three convergence roles. A thin, intention-revealing alias over the
/// broader [`TeamRole`] set so call sites read as Architect/Executor/Reviewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvergenceRole {
    Architect,
    Executor,
    Reviewer,
}

impl ConvergenceRole {
    /// Map to the shared [`TeamRole`] used by the execution ledger and routing.
    #[must_use]
    pub fn team_role(self) -> TeamRole {
        match self {
            ConvergenceRole::Architect => TeamRole::Planner,
            ConvergenceRole::Executor => TeamRole::Implementer,
            ConvergenceRole::Reviewer => TeamRole::Reviewer,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ConvergenceRole::Architect => "architect",
            ConvergenceRole::Executor => "executor",
            ConvergenceRole::Reviewer => "reviewer",
        }
    }
}

/// The reviewer's verdict on an executor result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewVerdict {
    /// The result is acceptable; the node converges.
    Approve,
    /// The result needs changes; the reasons are fed back to the executor.
    RequestChanges { reasons: String },
}

impl ReviewVerdict {
    #[must_use]
    pub fn is_approve(&self) -> bool {
        matches!(self, ReviewVerdict::Approve)
    }
}

/// A single Architect->Executor->Reviewer round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergenceRound {
    pub index: usize,
    pub architect_brief: String,
    pub executor_output: String,
    pub verdict: ReviewVerdict,
}

/// Terminal outcome of driving a node to convergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvergenceOutcome {
    /// The reviewer approved a result. Carries the approved executor output and
    /// all rounds for the ledger.
    Converged {
        approved_output: String,
        rounds: Vec<ConvergenceRound>,
    },
    /// The round budget was spent without approval. Carries the last executor
    /// output (best effort) and all rounds so callers can fall back / record.
    Exhausted {
        last_output: String,
        last_reasons: String,
        rounds: Vec<ConvergenceRound>,
    },
}

impl ConvergenceOutcome {
    #[must_use]
    pub fn converged(&self) -> bool {
        matches!(self, ConvergenceOutcome::Converged { .. })
    }

    #[must_use]
    pub fn rounds(&self) -> &[ConvergenceRound] {
        match self {
            ConvergenceOutcome::Converged { rounds, .. }
            | ConvergenceOutcome::Exhausted { rounds, .. } => rounds,
        }
    }

    /// The output to carry forward: the approved output on success, or the last
    /// attempt on exhaustion.
    #[must_use]
    pub fn best_output(&self) -> &str {
        match self {
            ConvergenceOutcome::Converged {
                approved_output, ..
            } => approved_output,
            ConvergenceOutcome::Exhausted { last_output, .. } => last_output,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_map_to_team_roles() {
        assert_eq!(ConvergenceRole::Architect.team_role(), TeamRole::Planner);
        assert_eq!(ConvergenceRole::Executor.team_role(), TeamRole::Implementer);
        assert_eq!(ConvergenceRole::Reviewer.team_role(), TeamRole::Reviewer);
    }

    #[test]
    fn verdict_and_outcome_helpers() {
        assert!(ReviewVerdict::Approve.is_approve());
        assert!(!ReviewVerdict::RequestChanges {
            reasons: "x".to_string()
        }
        .is_approve());

        let converged = ConvergenceOutcome::Converged {
            approved_output: "result".to_string(),
            rounds: vec![],
        };
        assert!(converged.converged());
        assert_eq!(converged.best_output(), "result");

        let exhausted = ConvergenceOutcome::Exhausted {
            last_output: "partial".to_string(),
            last_reasons: "still wrong".to_string(),
            rounds: vec![],
        };
        assert!(!exhausted.converged());
        assert_eq!(exhausted.best_output(), "partial");
    }
}
