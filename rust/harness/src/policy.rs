//! Permissions, as objects rather than as branches.
//!
//! The rules that make this worth a type:
//!
//! - **Deny beats ask beats allow.** Strength is a total order over three values, so a later rule can
//!   never reopen a door: adding a rule can move a decision towards safer or leave it alone, and nothing
//!   else. Reordering the list cannot turn a deny into an allow.
//! - **A blocking hook outranks an allow.** A hook exists to stop something the rules were going to let
//!   through, so it is a deny whatever else matched.
//! - **Nothing matched means ask.** A harness whose default is allow has already decided, and what it
//!   decided is that the model gets the machine.
//!
//! Every decision comes back with the reason that produced it, because a refusal an operator cannot
//! explain is a refusal they will route around.

use std::fmt;

/// What a rule can say about an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

impl Decision {
    pub fn name(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Ask => "ask",
            Decision::Deny => "deny",
        }
    }

    /// Higher is safer. The order is the rule.
    fn strength(self) -> u8 {
        match self {
            Decision::Allow => 0,
            Decision::Ask => 1,
            Decision::Deny => 2,
        }
    }
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A decision and the reason for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruling {
    decision: Decision,
    reason: String,
}

impl Ruling {
    pub fn new(decision: Decision, reason: impl Into<String>) -> Self {
        Ruling {
            decision,
            reason: reason.into(),
        }
    }

    pub fn decision(&self) -> Decision {
        self.decision
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// One rule: an action pattern, what to do about it, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pattern: String,
    decision: Decision,
    reason: String,
    blocking: bool,
}

impl Rule {
    pub fn new(pattern: impl Into<String>, decision: Decision, reason: impl Into<String>) -> Self {
        Rule {
            pattern: pattern.into(),
            decision,
            reason: reason.into(),
            blocking: false,
        }
    }

    /// A hook that exists to stop something: a deny whichever way the other rules went.
    pub fn blocking(pattern: impl Into<String>, reason: impl Into<String>) -> Self {
        Rule {
            pattern: pattern.into(),
            decision: Decision::Deny,
            reason: reason.into(),
            blocking: true,
        }
    }

    /// An exact match, or a trailing `*` for a family: `shell*` matches `shell` and `shell-run`.
    fn matches(&self, action: &str) -> bool {
        match self.pattern.strip_suffix('*') {
            Some(prefix) => action.starts_with(prefix),
            None => action == self.pattern,
        }
    }

    fn says(&self) -> Decision {
        if self.blocking {
            Decision::Deny
        } else {
            self.decision
        }
    }
}

/// The rules, and only the rules: no classifier, no model in the middle band. That is a seam this type
/// will grow, and it is not here because a half-built classifier that silently allows is worse than none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    rules: Vec<Rule>,
    /// Guarded plus the deny list in [`crate::floor::strict`]. A field rather than a rule because a rule
    /// matches an action name and this reads the command text.
    strict: bool,
}

impl Policy {
    pub fn new(rules: Vec<Rule>) -> Self {
        Policy {
            rules,
            strict: false,
        }
    }

    /// The same rules, with the strict deny list on.
    pub fn strict(mut self) -> Self {
        self.strict = true;
        self
    }

    pub fn is_strict(&self) -> bool {
        self.strict
    }

    /// The rules this policy holds, in order.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The strongest thing any matching rule says, with the reason that said it.
    ///
    /// The default is only the default: when nothing matches, the answer is ask. It is deliberately *not*
    /// in the strength comparison, because an implicit ask outranking an explicit allow is a policy where
    /// writing a rule down can never loosen anything, which is not what a rule is for.
    pub fn decide(&self, action: &str) -> Ruling {
        let mut matched: Option<Ruling> = None;

        for rule in &self.rules {
            if !rule.matches(action) {
                continue;
            }
            let says = Ruling::new(rule.says(), rule.reason.clone());
            matched = match matched {
                None => Some(says),
                Some(current) if says.decision.strength() > current.decision.strength() => {
                    Some(says)
                }
                Some(current) => Some(current),
            };
        }

        matched.unwrap_or_else(|| {
            Ruling::new(
                Decision::Ask,
                format!("nothing in the policy mentions {action}, so it waits for a person"),
            )
        })
    }
}

/// The policy that lets the shell run. For a caller who has decided, and for the work where the program is
/// not in question.
pub const OPEN: &str = "open";

/// The policy that asks before the shell runs. The safe default, because a harness that defaults to allow
/// has already decided that the model gets the machine.
pub const GUARDED: &str = "guarded";

/// Guarded, plus a coarse deny list over command text. It refuses the destructive primitives in
/// [`crate::floor::STRICT_DENY_LIST`] before the shell runs; it is a deny list, not a sandbox, and a
/// receipt that says otherwise would be claiming a guarantee this build does not make.
pub const STRICT: &str = "strict";

/// The policies this build ships, by name.
///
/// The difference between the two is the whole reason the broker exists: one lets the shell run, the other
/// asks a person and remembers the answer for the session, so the question is asked once rather than every
/// turn.
pub fn named(name: &str) -> Option<Policy> {
    Some(match name {
        OPEN => Policy::new(vec![Rule::new(
            crate::SHELL,
            Decision::Allow,
            "this policy lets the shell run",
        )]),
        GUARDED => Policy::new(vec![Rule::new(
            crate::SHELL,
            Decision::Ask,
            "this policy asks a person before the shell runs",
        )]),
        STRICT => Policy::new(vec![Rule::new(
            crate::SHELL,
            Decision::Ask,
            "this policy asks a person before the shell runs, and refuses a destructive primitive in the \
             command text",
        )])
        .strict(),
        _ => return None,
    })
}

/// The names, for a usage message that cannot drift from the table.
pub fn shipped() -> [&'static str; 3] {
    [OPEN, GUARDED, STRICT]
}

/// The actions a guard samples: what matters is the decision and the reason a person would read for it,
/// not the private rules that produced them. One list, so a guard digest and a candidate refusal cannot
/// disagree about which actions the policy is asked about.
pub const fn probes() -> [&'static str; 4] {
    ["shell", "shell-run", "write_file", "vault"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_matched_asks() {
        let policy = Policy::new(vec![Rule::new(
            "shell",
            Decision::Allow,
            "the shell is allowed",
        )]);
        let ruling = policy.decide("vault");
        assert_eq!(ruling.decision(), Decision::Ask);
        assert!(ruling.reason().contains("nothing in the policy"));
    }

    #[test]
    fn a_deny_beats_a_later_allow() {
        let policy = Policy::new(vec![
            Rule::new("shell", Decision::Deny, "the shell is closed here"),
            Rule::new("shell", Decision::Allow, "and open here"),
        ]);
        let ruling = policy.decide("shell");
        assert_eq!(ruling.decision(), Decision::Deny);
        assert_eq!(ruling.reason(), "the shell is closed here");
    }

    #[test]
    fn order_does_not_weaken_a_decision() {
        // The same two rules the other way round: strength is a total order, not "last one wins".
        let policy = Policy::new(vec![
            Rule::new("shell", Decision::Allow, "and open here"),
            Rule::new("shell", Decision::Deny, "the shell is closed here"),
        ]);
        assert_eq!(policy.decide("shell").decision(), Decision::Deny);
    }

    #[test]
    fn an_ask_beats_an_allow() {
        let policy = Policy::new(vec![
            Rule::new("shell", Decision::Allow, "open"),
            Rule::new("shell", Decision::Ask, "but ask a person"),
        ]);
        assert_eq!(policy.decide("shell").decision(), Decision::Ask);
    }

    #[test]
    fn a_blocking_hook_outranks_an_allow() {
        let policy = Policy::new(vec![
            Rule::new("shell", Decision::Allow, "the shell is allowed"),
            Rule::blocking("shell", "this hook stops it"),
        ]);
        let ruling = policy.decide("shell");
        assert_eq!(ruling.decision(), Decision::Deny);
        assert_eq!(ruling.reason(), "this hook stops it");
    }

    #[test]
    fn a_wildcard_matches_a_family_and_not_a_neighbour() {
        let policy = Policy::new(vec![Rule::new(
            "shell*",
            Decision::Allow,
            "the shell family",
        )]);
        assert_eq!(policy.decide("shell").decision(), Decision::Allow);
        assert_eq!(policy.decide("shell-run").decision(), Decision::Allow);
        assert_eq!(policy.decide("vault").decision(), Decision::Ask);
    }
}
