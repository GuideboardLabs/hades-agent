//! A profile is a named set of parameters for one kind of work, and it is data rather than code.
//!
//! The rule it exists for: **every model, effort, context budget and check is a parameter that a run
//! records, never a branch in the harness.** A harness with variants of itself compiled in cannot be
//! compared across them, and the comparison is the point.

use std::fmt;

use crate::task::Budget;

/// The name of a profile. A newtype rather than a string, so a profile id cannot be confused with a
/// model name or a path by a caller who is moving fast.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProfileId(String);

impl ProfileId {
    pub fn new(id: impl Into<String>) -> Self {
        ProfileId(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How hard the model is asked to think. Recorded on every turn, because a change in answer quality is
/// not a finding unless the effort behind it is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn name(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

/// What kind of work a profile is allowed to do. The name is plain on purpose: a role is a permission
/// shape, not a persona, and the code enforces it rather than asking the model to respect it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Look, do not touch. Wide context, no edits, no memory writes.
    Survey,
    /// One narrow change, with edits and a check.
    Patch,
    /// A planned change across files, with edits and a check.
    Sweep,
    /// Check someone else's work. Read-only, and never the profile that produced it.
    Verify,
    /// Write memory only. No code, no shell.
    Record,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Survey => "survey",
            Role::Patch => "patch",
            Role::Sweep => "sweep",
            Role::Verify => "verify",
            Role::Record => "record",
        }
    }

    /// The tools this role may hold. A missing tool is a permission, not a prompt sentence: the schema does
    /// not offer it and `execute` refuses it even if a model names it.
    pub fn allows(self, tool: &str) -> bool {
        const READ: [&str; 9] = [
            "read_file",
            "list_dir",
            "search",
            "recall",
            "session_search",
            "skills",
            "web_search",
            "fetch",
            "image",
        ];
        const MEMORY: [&str; 4] = ["remember", "recall", "session_search", "skills"];
        match self {
            Role::Survey | Role::Verify => READ.contains(&tool),
            Role::Patch | Role::Sweep => true,
            Role::Record => MEMORY.contains(&tool),
        }
    }

    pub fn edits(self) -> bool {
        matches!(self, Role::Patch | Role::Sweep)
    }

    /// Whether a shell under this role may reach the network. A patch needs a clone or a package index; a
    /// read-only or memory role does not, so it is closed by default.
    pub fn network(self) -> bool {
        matches!(self, Role::Patch | Role::Sweep)
    }
}

/// The parameters of one kind of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: ProfileId,
    /// What the profile may do. Enforced in the tool schema and in `execute`.
    pub role: Role,
    /// The model this kind of work asks for. Nothing calls it yet; it is recorded now so that the choice
    /// is in the log from the first turn instead of being bolted on after the first bad comparison.
    pub model: String,
    pub effort: Effort,
    pub context_tokens: usize,
    /// The command that decides whether the work is done. A command, not a language server: a language
    /// server is opt-in and off unless the repo configured one, which is the honest default because most
    /// repos have not.
    pub verify: String,
    /// Whether a separate, small judge reads the turn afterwards.
    pub judge: bool,
    /// Whether the model must write a plan, and the turn must record it, before its first program. One plan
    /// per task: a retry after a failed check already has it, and a session reused for a different goal is
    /// asked again. There is no review step; the plan is recorded evidence, and the check still decides.
    pub plan_first: bool,
    /// Whether an edit answer is accepted. A profile that must see a whole program sets this false, and the
    /// turn refuses an edit as unwritten rather than quietly applying it.
    pub edits: bool,
    /// Whether a shadow snapshot is taken before each program, when the caller did not say otherwise.
    pub shadow: bool,
    /// The default attempt and time bounds for a run on this profile. A flag overrides them; the defaults
    /// live here so the autonomy gate is data about the work, not a constant in `run_graph`.
    pub attempts: u32,
    pub minutes: u64,
    /// Whether a governed program under this profile may reach the network. Read-only profiles close it by
    /// default; a patch profile needs it for a package index or a clone.
    pub network: bool,
}

impl Profile {
    pub fn budget(&self) -> Budget {
        Budget::new(self.context_tokens)
    }
}

/// The profiles this build ships. Rules name them, and nothing here is tuned: these are the starting
/// parameters, listed in one place so that changing one is a visible act rather than a scattered edit.
pub fn shipped() -> Vec<Profile> {
    vec![
        Profile {
            id: ProfileId::new("fix"),
            role: Role::Patch,
            model: "deepseek-v4.1-flash".to_string(),
            effort: Effort::Medium,
            context_tokens: 32_000,
            verify: "cargo test --quiet".to_string(),
            judge: true,
            plan_first: false,
            edits: true,
            shadow: true,
            attempts: 3,
            minutes: 10,
            network: true,
        },
        Profile {
            id: ProfileId::new("sweep"),
            role: Role::Sweep,
            model: "deepseek-v4.1-flash".to_string(),
            effort: Effort::High,
            context_tokens: 96_000,
            verify: "cargo test --quiet".to_string(),
            judge: true,
            plan_first: true,
            edits: true,
            shadow: true,
            attempts: 4,
            minutes: 30,
            network: true,
        },
        Profile {
            id: ProfileId::new("survey"),
            role: Role::Survey,
            model: "deepseek-v4.1-flash".to_string(),
            effort: Effort::Low,
            context_tokens: 16_000,
            // A read-only profile's check is that nothing changed, so the check is the diff.
            verify: "git diff --exit-code".to_string(),
            judge: false,
            plan_first: false,
            edits: false,
            shadow: false,
            attempts: 1,
            minutes: 5,
            network: false,
        },
        Profile {
            id: ProfileId::new("verify"),
            role: Role::Verify,
            // A different model from the one patch and sweep produce with: an independent check is not a
            // second opinion from the same voice. This is the "sentinel never verifies itself" rule as data.
            model: "glm-5.3-flash".to_string(),
            effort: Effort::Medium,
            context_tokens: 32_000,
            verify: "git diff --exit-code".to_string(),
            judge: true,
            plan_first: false,
            edits: false,
            shadow: false,
            attempts: 1,
            minutes: 5,
            network: false,
        },
        Profile {
            id: ProfileId::new("record"),
            role: Role::Record,
            model: "deepseek-v4.1-flash".to_string(),
            effort: Effort::Low,
            context_tokens: 16_000,
            verify: "git diff --exit-code".to_string(),
            judge: false,
            plan_first: false,
            edits: false,
            shadow: false,
            attempts: 1,
            minutes: 5,
            network: false,
        },
    ]
}

/// The shipped profile a caller names, or `None`.
pub fn named(id: &str) -> Option<Profile> {
    shipped().into_iter().find(|p| p.id.as_str() == id)
}

/// Every shipped parameter, one line per profile: what a harness edit may not change.
///
/// A digest of this is recorded with each harness generation, so a rollback made across a change to a
/// default is refused instead of being applied to a harness that was measured under another one. Every
/// field is in the line, including the ones nobody would miss, because those are the ones that change
/// silently.
pub fn parameter_text() -> String {
    let mut text = String::new();
    for profile in shipped() {
        text.push_str(&format!(
            "profile {} {} {} {} {} {} {} {} {} {} {} {} {}\n",
            profile.id.as_str(),
            profile.role.name(),
            profile.model,
            profile.effort.name(),
            profile.context_tokens,
            profile.verify,
            profile.judge,
            profile.plan_first,
            profile.edits,
            profile.shadow,
            profile.attempts,
            profile.minutes,
            profile.network,
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_profiles_and_every_one_has_a_distinct_role() {
        let profiles = shipped();
        assert_eq!(profiles.len(), 5);
        let mut roles: Vec<&str> = profiles.iter().map(|p| p.role.name()).collect();
        roles.sort_unstable();
        roles.dedup();
        assert_eq!(roles.len(), 5, "each role appears once: {roles:?}");
    }

    #[test]
    fn the_edits_flag_agrees_with_the_role() {
        for profile in shipped() {
            assert_eq!(
                profile.edits,
                profile.role.edits(),
                "{} says edits={} but its role says {}",
                profile.id,
                profile.edits,
                profile.role.edits()
            );
        }
    }

    #[test]
    fn the_verifier_does_not_run_on_the_producers_model() {
        let patch = named("fix").expect("fix");
        let sweep = named("sweep").expect("sweep");
        let verify = named("verify").expect("verify");
        assert_ne!(verify.model, patch.model);
        assert_ne!(verify.model, sweep.model);
        assert!(!verify.edits);
    }

    #[test]
    fn a_record_profile_holds_memory_tools_only() {
        let record = named("record").expect("record");
        assert!(record.role.allows("remember"));
        assert!(!record.role.allows("shell"));
        assert!(!record.role.allows("write_file"));
        assert!(!record.role.allows("patch"));
        assert!(!record.role.allows("delegate"));
    }

    #[test]
    fn a_verifier_is_read_only_even_in_act_mode() {
        let verify = named("verify").expect("verify");
        assert!(verify.role.allows("read_file"));
        assert!(verify.role.allows("search"));
        assert!(!verify.role.allows("shell"));
        assert!(!verify.role.allows("write_file"));
        assert!(!verify.role.allows("remember"));
    }
}
