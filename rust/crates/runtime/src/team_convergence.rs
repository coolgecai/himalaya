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

/// Drive a node to convergence through the Architect -> Executor -> Reviewer
/// loop, bounded by `max_rounds` review cycles.
///
/// The three callbacks isolate the model-facing work so this orchestration is
/// pure and unit-testable (the runtime supplies real model sub-turns in later
/// stages):
/// - `architect()` runs once and returns the approach brief. `None` aborts with
///   an immediate `Exhausted` (no brief, nothing to execute).
/// - `executor(brief, prior_reasons)` produces the result for a round.
///   `prior_reasons` is `None` on the first round and `Some(reasons)` when the
///   reviewer previously requested changes. `None` output counts as a failed
///   round whose reasons are "executor produced no output".
/// - `reviewer(brief, output)` returns the verdict for a round.
///
/// Converges (returns `Converged`) as soon as the reviewer approves; returns
/// `Exhausted` if `max_rounds` review cycles pass without approval. `max_rounds`
/// is clamped to at least 1.
pub fn drive_convergence<A, E, R>(
    max_rounds: usize,
    architect: A,
    mut executor: E,
    mut reviewer: R,
) -> ConvergenceOutcome
where
    A: FnOnce() -> Option<String>,
    E: FnMut(&str, Option<&str>) -> Option<String>,
    R: FnMut(&str, &str) -> ReviewVerdict,
{
    let rounds_budget = max_rounds.max(1);
    let mut rounds = Vec::new();

    let Some(brief) = architect() else {
        return ConvergenceOutcome::Exhausted {
            last_output: String::new(),
            last_reasons: "architect produced no brief".to_string(),
            rounds,
        };
    };

    let mut prior_reasons: Option<String> = None;
    for index in 0..rounds_budget {
        let output = executor(&brief, prior_reasons.as_deref());
        let Some(output) = output else {
            let reasons = "executor produced no output".to_string();
            rounds.push(ConvergenceRound {
                index,
                architect_brief: brief.clone(),
                executor_output: String::new(),
                verdict: ReviewVerdict::RequestChanges {
                    reasons: reasons.clone(),
                },
            });
            prior_reasons = Some(reasons);
            continue;
        };

        let verdict = reviewer(&brief, &output);
        rounds.push(ConvergenceRound {
            index,
            architect_brief: brief.clone(),
            executor_output: output.clone(),
            verdict: verdict.clone(),
        });

        match verdict {
            ReviewVerdict::Approve => {
                return ConvergenceOutcome::Converged {
                    approved_output: output,
                    rounds,
                };
            }
            ReviewVerdict::RequestChanges { reasons } => {
                prior_reasons = Some(reasons);
            }
        }
    }

    // Budget spent without approval.
    let last_output = rounds
        .last()
        .map(|round| round.executor_output.clone())
        .unwrap_or_default();
    let last_reasons =
        prior_reasons.unwrap_or_else(|| "no approval within round budget".to_string());
    ConvergenceOutcome::Exhausted {
        last_output,
        last_reasons,
        rounds,
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

    #[test]
    fn converges_on_first_round_when_reviewer_approves() {
        let outcome = drive_convergence(
            3,
            || Some("brief".to_string()),
            |_brief, _prior| Some("impl v1".to_string()),
            |_brief, _output| ReviewVerdict::Approve,
        );
        assert!(outcome.converged());
        assert_eq!(outcome.best_output(), "impl v1");
        assert_eq!(outcome.rounds().len(), 1);
    }

    #[test]
    fn converges_after_reviewer_requests_changes_then_approves() {
        use std::cell::Cell;
        let exec_calls = Cell::new(0usize);
        let review_calls = Cell::new(0usize);
        let outcome = drive_convergence(
            5,
            || Some("brief".to_string()),
            |_brief, prior| {
                exec_calls.set(exec_calls.get() + 1);
                // The executor sees the reviewer's prior reasons on retries.
                if exec_calls.get() == 1 {
                    assert!(prior.is_none());
                } else {
                    assert_eq!(prior, Some("fix the edge case"));
                }
                Some(format!("impl v{}", exec_calls.get()))
            },
            |_brief, _output| {
                review_calls.set(review_calls.get() + 1);
                if review_calls.get() < 3 {
                    ReviewVerdict::RequestChanges {
                        reasons: "fix the edge case".to_string(),
                    }
                } else {
                    ReviewVerdict::Approve
                }
            },
        );
        assert!(outcome.converged());
        assert_eq!(exec_calls.get(), 3);
        assert_eq!(outcome.rounds().len(), 3);
        assert_eq!(outcome.best_output(), "impl v3");
    }

    #[test]
    fn exhausts_when_reviewer_never_approves() {
        let outcome = drive_convergence(
            2,
            || Some("brief".to_string()),
            |_brief, _prior| Some("attempt".to_string()),
            |_brief, _output| ReviewVerdict::RequestChanges {
                reasons: "still wrong".to_string(),
            },
        );
        assert!(!outcome.converged());
        assert_eq!(outcome.rounds().len(), 2);
        if let ConvergenceOutcome::Exhausted {
            last_reasons,
            last_output,
            ..
        } = outcome
        {
            assert_eq!(last_reasons, "still wrong");
            assert_eq!(last_output, "attempt");
        } else {
            panic!("expected Exhausted");
        }
    }

    #[test]
    fn aborts_when_architect_produces_no_brief() {
        let outcome = drive_convergence(
            3,
            || None,
            |_brief, _prior| Some("never runs".to_string()),
            |_brief, _output| ReviewVerdict::Approve,
        );
        assert!(!outcome.converged());
        assert!(outcome.rounds().is_empty());
    }

    #[test]
    fn executor_no_output_counts_as_failed_round() {
        let outcome = drive_convergence(
            2,
            || Some("brief".to_string()),
            |_brief, _prior| None,
            |_brief, _output| ReviewVerdict::Approve,
        );
        assert!(!outcome.converged());
        // Both rounds recorded as failed (reviewer never consulted).
        assert_eq!(outcome.rounds().len(), 2);
        assert!(outcome
            .rounds()
            .iter()
            .all(|round| !round.verdict.is_approve()));
    }

    #[test]
    fn max_rounds_zero_is_clamped_to_one() {
        let outcome = drive_convergence(
            0,
            || Some("brief".to_string()),
            |_brief, _prior| Some("once".to_string()),
            |_brief, _output| ReviewVerdict::Approve,
        );
        assert!(outcome.converged());
        assert_eq!(outcome.rounds().len(), 1);
    }
}
