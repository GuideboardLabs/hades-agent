//! The router: which profile a piece of work runs on, and why.
//!
//! The design's rule is *rules first, then our own scoreboard*. This is the rules half and only the rules
//! half. The reason is a value that travels with the turn into the log, because a router that cannot say
//! why it chose is a router that cannot be improved: the choices would have no record to score.

use crate::profile::{Profile, ProfileId};
use crate::task::TaskSpec;

/// Why a profile was picked. Recorded with every turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingReason(String);

impl RoutingReason {
    pub fn new(reason: impl Into<String>) -> Self {
        RoutingReason(reason.into())
    }

    /// No rule matched, so the fallback ran. Kept as a distinct reason rather than folded into a match,
    /// because "we had no rule for this" is the most useful line in the log.
    pub fn no_rule_matched() -> Self {
        RoutingReason::new("no rule matched, so the fallback profile ran")
    }

    /// The caller chose, and this is what to write down.
    pub fn chosen(by: &str) -> Self {
        RoutingReason::new(format!("chosen by {by}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One rule: if the goal contains any of these words, run this profile, for this reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingRule {
    pub needles: Vec<String>,
    pub profile: ProfileId,
    pub reason: String,
}

impl RoutingRule {
    pub fn new(needles: &[&str], profile: &str, reason: &str) -> Self {
        RoutingRule {
            needles: needles.iter().map(|n| n.to_lowercase()).collect(),
            profile: ProfileId::new(profile),
            reason: reason.to_string(),
        }
    }

    fn matches(&self, goal: &str) -> bool {
        let goal = goal.to_lowercase();
        self.needles
            .iter()
            .any(|needle| goal.contains(needle.as_str()))
    }
}

/// Picks a profile from rules.
pub trait Router {
    fn pick(&self, task: &TaskSpec) -> (ProfileId, RoutingReason);
}

/// The rules, in order, first match wins, and a fallback for when nothing matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    rules: Vec<RoutingRule>,
    fallback: ProfileId,
}

impl Rules {
    pub fn new(rules: Vec<RoutingRule>, fallback: impl Into<String>) -> Self {
        Rules {
            rules,
            fallback: ProfileId::new(fallback),
        }
    }

    /// The starting rules. Three, and every one of them names a profile that exists: a rule pointing at a
    /// profile nothing ships is a router that fails at the worst possible moment.
    pub fn shipped() -> Self {
        Rules::new(
            vec![
                RoutingRule::new(
                    &[
                        "verify",
                        "review the change",
                        "second opinion",
                        "audit",
                        "check the diff",
                    ],
                    "verify",
                    "the goal asks for an independent check, so it runs on the read-only verifier",
                ),
                RoutingRule::new(
                    &[
                        "remember",
                        "record this",
                        "write this down",
                        "note that",
                        "make a note",
                    ],
                    "record",
                    "the goal is a memory write, so it runs where only memory tools exist",
                ),
                RoutingRule::new(
                    &["what", "where", "which", "survey", "read", "list"],
                    "survey",
                    "the goal asks about the repo rather than for a change",
                ),
                RoutingRule::new(
                    &["sweep", "refactor", "every", "all ", "across"],
                    "sweep",
                    "the goal spans more than one file, so it runs on the wide profile",
                ),
                RoutingRule::new(
                    &["fix", "bug", "broken", "repair", "fails", "panic"],
                    "fix",
                    "the goal is a correction, so it runs on the narrow profile",
                ),
            ],
            "fix",
        )
    }
}

impl Router for Rules {
    fn pick(&self, task: &TaskSpec) -> (ProfileId, RoutingReason) {
        for rule in &self.rules {
            if rule.matches(task.goal()) {
                return (rule.profile.clone(), RoutingReason::new(&rule.reason));
            }
        }
        (self.fallback.clone(), RoutingReason::no_rule_matched())
    }
}

/// A convenience for callers that hold profiles as a list: the profile a rule names, as data.
pub fn profile_for<'a>(profiles: &'a [Profile], id: &ProfileId) -> Option<&'a Profile> {
    profiles.iter().find(|p| &p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn task(goal: &str) -> TaskSpec {
        TaskSpec::new(goal, Path::new("/tmp"))
    }

    #[test]
    fn an_independent_check_routes_to_the_verifier() {
        let (id, _) = Rules::shipped().pick(&task("verify the change in lib.rs"));
        assert_eq!(id.as_str(), "verify");
        let (id, _) = Rules::shipped().pick(&task("review the change for a second opinion"));
        assert_eq!(id.as_str(), "verify");
    }

    #[test]
    fn a_memory_write_routes_to_the_record_profile() {
        let (id, _) = Rules::shipped().pick(&task("remember that the build needs --release"));
        assert_eq!(id.as_str(), "record");
    }

    #[test]
    fn every_rule_and_the_fallback_name_a_shipped_profile() {
        let rules = Rules::shipped();
        let shipped = crate::profile::shipped();
        for rule in &rules.rules {
            assert!(
                shipped.iter().any(|profile| profile.id == rule.profile),
                "rule names {}, which this build does not ship",
                rule.profile
            );
        }
        assert!(
            shipped.iter().any(|profile| profile.id == rules.fallback),
            "fallback names {}, which this build does not ship",
            rules.fallback
        );
    }
}
