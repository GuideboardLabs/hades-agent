//! Fallibility is a type, and it is classified.
//!
//! The class is the point. A runtime that scores an infrastructure failure as a bad answer is a runtime
//! whose scoreboard lies, and every one of Anthropic's own postmortems on agent loops comes back to the
//! same thing: the harness fault was read as a model fault. Here the class is on the value, so the caller
//! cannot lose it by accident, and [`Class::counts_against_the_model`] is the one place that answers the
//! question a scoreboard asks.

use std::error::Error;
use std::fmt;

use mneme::Fault as VaultFault;
use session_tree::Fault as SessionFault;

/// Who failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// The harness: a tool that would not spawn, a store that refused, a check we could not run.
    /// Ours, and never charged to the model.
    Harness,
    /// The model: a program that will not parse, a refusal, an empty turn. The only class that is an
    /// answer, because it is the only one the model could have got right.
    Model,
    /// The check could not run or could not be read, so nothing was learned either way. Not a pass, and
    /// not a failure of the answer.
    Verification,
    /// Policy refused or a gate stopped the work before it started.
    Policy,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::Harness => "harness",
            Class::Model => "model",
            Class::Verification => "verification",
            Class::Policy => "policy",
        }
    }

    /// The one question a scoreboard asks, answered in one place.
    pub fn counts_against_the_model(self) -> bool {
        matches!(self, Class::Model)
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A failure, with its class and enough detail to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    class: Class,
    detail: String,
}

impl Fault {
    pub fn new(class: Class, detail: impl Into<String>) -> Self {
        Fault {
            class,
            detail: detail.into(),
        }
    }

    pub fn harness(detail: impl Into<String>) -> Self {
        Fault::new(Class::Harness, detail)
    }

    pub fn model(detail: impl Into<String>) -> Self {
        Fault::new(Class::Model, detail)
    }

    pub fn verification(detail: impl Into<String>) -> Self {
        Fault::new(Class::Verification, detail)
    }

    pub fn policy(detail: impl Into<String>) -> Self {
        Fault::new(Class::Policy, detail)
    }

    pub fn class(&self) -> Class {
        self.class
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} fault: {}", self.class, self.detail)
    }
}

impl Error for Fault {}

/// The vault's refusals keep their meaning across the boundary: a note that was correctly refused is a
/// policy outcome, and a note we could not read is our own failure. Collapsing the two is how a governance
/// rule gets mistaken for a bug.
impl From<VaultFault> for Fault {
    fn from(fault: VaultFault) -> Self {
        let class = match &fault {
            VaultFault::OrphanNote { .. }
            | VaultFault::InvalidNote { .. }
            | VaultFault::NoFreeName { .. } => Class::Policy,
            VaultFault::MissingNote { .. } | VaultFault::Io { .. } => Class::Harness,
        };
        Fault::new(class, fault.to_string())
    }
}

/// A log we cannot read is ours, every time. Nothing about a broken store is the model's fault.
impl From<SessionFault> for Fault {
    fn from(fault: SessionFault) -> Self {
        Fault::harness(fault.to_string())
    }
}
