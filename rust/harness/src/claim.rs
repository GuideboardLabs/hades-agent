//! What a run may say it did, as a ladder rather than a score.
//!
//! A check passing and a judge saying `not_yet` are two different facts, and a run that records both but
//! reports only exit 0 leaves the difference for a reader to infer. The ladder names it: every rung is a
//! fact already in the record, a rung is reachable only when every rung below it holds, and the first
//! unmet rung carries a sentence saying why.
//!
//! The ladder does not change acceptance. It reports what acceptance already means, so an overclaim is
//! visible instead of inferred.

use crate::judge::Verdict;
use crate::verify::State;

/// One rung. Ordered: a claim at rung N requires every rung below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    /// No turn ran: nothing was attempted, so nothing can be claimed.
    Nothing,
    /// A model call and a program that ran. This is all a run with no result may say.
    Attempted,
    /// The check reached a verdict: passed or failed.
    Checked,
    /// The check passed.
    Passed,
    /// The judge said `met`.
    Judged,
    /// The walk ended accepted, which is what `hades run` and `turn` report as exit 0.
    Accepted,
}

impl Rung {
    pub fn name(self) -> &'static str {
        match self {
            Rung::Nothing => "nothing",
            Rung::Attempted => "attempted",
            Rung::Checked => "checked",
            Rung::Passed => "passed",
            Rung::Judged => "judged",
            Rung::Accepted => "accepted",
        }
    }
}

/// The facts the ladder reads. Each one is already in the session log or the walk's own record; the ladder
/// is a view, not a second source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Facts {
    pub attempted: bool,
    pub check: Option<State>,
    pub verdict: Option<Verdict>,
    pub accepted: bool,
    /// The check or a file it depends on changed during the run. Read before `check`, because a green
    /// check from a moved verifier is not evidence of a pass.
    pub ruler_changed: bool,
}

/// The highest rung the facts support, and the first rung they refuse, with a reason a person can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub rung: Rung,
    pub refused: Option<(Rung, String)>,
}

impl Claim {
    /// One line for a surface: what may be claimed, and what may not, in the walk's own words.
    pub fn line(&self) -> String {
        match &self.refused {
            None => format!("may claim: {}", self.rung.name()),
            Some((rung, why)) => format!(
                "may claim: {}; cannot claim: {} ({why})",
                self.rung.name(),
                rung.name()
            ),
        }
    }

    /// Whether the run may say it was accepted. The one question a caller usually has.
    pub fn accepted(&self) -> bool {
        self.rung == Rung::Accepted
    }
}

/// Walk the ladder from the bottom and stop at the first fact that does not hold.
pub fn ladder(facts: &Facts) -> Claim {
    if !facts.attempted {
        return refused(Rung::Nothing, Rung::Attempted, "no turn ran");
    }
    // Before the check is read: a tampered verifier can produce a green result, and the ladder must never
    // climb on one.
    if facts.ruler_changed {
        return refused(
            Rung::Attempted,
            Rung::Checked,
            "the check changed during the run",
        );
    }
    let Some(check) = facts.check else {
        return refused(
            Rung::Attempted,
            Rung::Checked,
            "the check never ran, so nothing was checked",
        );
    };
    if !check.is_a_result() {
        let why = match check {
            State::TimedOut => "the check timed out without a verdict",
            _ => "the check never ran, so nothing was checked",
        };
        return refused(Rung::Attempted, Rung::Checked, why);
    }
    if check != State::Passed {
        return refused(Rung::Checked, Rung::Passed, "the check failed");
    }
    match facts.verdict {
        Some(Verdict::Met) => {}
        Some(Verdict::NotYet) => {
            return refused(Rung::Passed, Rung::Judged, "the judge said not_yet");
        }
        Some(Verdict::Impossible) => {
            return refused(Rung::Passed, Rung::Judged, "the judge said impossible");
        }
        Some(Verdict::Unjudged) | None => {
            return refused(Rung::Passed, Rung::Judged, "no judge ran");
        }
    }
    if !facts.accepted {
        return refused(
            Rung::Judged,
            Rung::Accepted,
            "the walk did not end accepted",
        );
    }
    Claim {
        rung: Rung::Accepted,
        refused: None,
    }
}

fn refused(rung: Rung, next: Rung, why: &str) -> Claim {
    Claim {
        rung,
        refused: Some((next, why.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_moved_verifier_cannot_climb_on_a_green_check() {
        let facts = Facts {
            attempted: true,
            check: Some(State::Passed),
            verdict: Some(Verdict::Met),
            accepted: true,
            ruler_changed: true,
        };
        let claim = ladder(&facts);
        assert_eq!(claim.rung, Rung::Attempted);
        let (next, why) = claim.refused.expect("a refusal");
        assert_eq!(next, Rung::Checked);
        assert!(why.contains("changed during the run"), "{why}");
    }

    #[test]
    fn no_turn_is_nothing() {
        let claim = ladder(&Facts::default());
        assert_eq!(claim.rung, Rung::Nothing);
        assert_eq!(
            claim.refused,
            Some((Rung::Attempted, "no turn ran".to_string()))
        );
    }

    #[test]
    fn a_check_that_could_not_run_is_attempted() {
        let facts = Facts {
            attempted: true,
            check: Some(State::CouldNotRun),
            ..Facts::default()
        };
        let claim = ladder(&facts);
        assert_eq!(claim.rung, Rung::Attempted);
        let (next, why) = claim.refused.expect("a refusal");
        assert_eq!(next, Rung::Checked);
        assert!(why.contains("never ran"), "{why}");
    }

    #[test]
    fn a_timed_out_check_is_attempted_and_names_the_timeout() {
        let facts = Facts {
            attempted: true,
            check: Some(State::TimedOut),
            ..Facts::default()
        };
        let claim = ladder(&facts);
        assert_eq!(claim.rung, Rung::Attempted);
        let (next, why) = claim.refused.expect("a refusal");
        assert_eq!(next, Rung::Checked);
        assert!(why.contains("timed out"), "{why}");
    }

    #[test]
    fn a_failed_check_is_checked() {
        let facts = Facts {
            attempted: true,
            check: Some(State::Failed),
            ..Facts::default()
        };
        let claim = ladder(&facts);
        assert_eq!(claim.rung, Rung::Checked);
        assert_eq!(
            claim.refused,
            Some((Rung::Passed, "the check failed".to_string()))
        );
    }

    #[test]
    fn a_pass_with_no_judge_is_passed() {
        let facts = Facts {
            attempted: true,
            check: Some(State::Passed),
            verdict: Some(Verdict::Unjudged),
            accepted: true,
            ruler_changed: false,
        };
        let claim = ladder(&facts);
        assert_eq!(claim.rung, Rung::Passed);
        assert_eq!(
            claim.refused,
            Some((Rung::Judged, "no judge ran".to_string()))
        );
        assert_eq!(
            claim.line(),
            "may claim: passed; cannot claim: judged (no judge ran)"
        );
    }

    #[test]
    fn a_pass_the_judge_did_not_accept_is_not_judged() {
        let facts = Facts {
            attempted: true,
            check: Some(State::Passed),
            verdict: Some(Verdict::NotYet),
            accepted: true,
            ruler_changed: false,
        };
        let claim = ladder(&facts);
        assert_eq!(claim.rung, Rung::Passed);
        assert_eq!(
            claim.refused,
            Some((Rung::Judged, "the judge said not_yet".to_string()))
        );
    }

    #[test]
    fn a_judged_run_that_was_not_accepted_is_judged() {
        let facts = Facts {
            attempted: true,
            check: Some(State::Passed),
            verdict: Some(Verdict::Met),
            accepted: false,
            ruler_changed: false,
        };
        let claim = ladder(&facts);
        assert_eq!(claim.rung, Rung::Judged);
        assert_eq!(
            claim.refused,
            Some((Rung::Accepted, "the walk did not end accepted".to_string()))
        );
    }

    #[test]
    fn a_judged_accepted_run_claims_everything() {
        let facts = Facts {
            attempted: true,
            check: Some(State::Passed),
            verdict: Some(Verdict::Met),
            accepted: true,
            ruler_changed: false,
        };
        let claim = ladder(&facts);
        assert_eq!(claim.rung, Rung::Accepted);
        assert!(claim.refused.is_none());
        assert!(claim.accepted());
        assert_eq!(claim.line(), "may claim: accepted");
    }
}
