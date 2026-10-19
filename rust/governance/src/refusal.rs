//! The refusals, and the vault faults that pass through them.
//!
//! Every variant names its class, because a caller that cannot tell a policy refusal from a disk error
//! will eventually score the wrong thing: a note that was correctly refused is not a broken run, and a
//! missing file is not a bad answer.

use std::fmt;

use mneme::Fault;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Policy. The destination is not one a note may be filed into and stay memory.
    Unfiled { detail: String },
    /// Policy. A durable write must say where it came from.
    MissingProvenance { detail: String },
    /// Policy. The destination is fenced: never read, never indexed, never written.
    Fenced { path: String },
    /// Infrastructure, straight through from the vault: a missing note, a disk that said no.
    Vault(Fault),
}

impl From<Fault> for Refusal {
    fn from(fault: Fault) -> Self {
        Refusal::Vault(fault)
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Unfiled { detail } => write!(f, "unfiled: {detail}"),
            Refusal::MissingProvenance { detail } => write!(f, "no provenance: {detail}"),
            Refusal::Fenced { path } => {
                write!(
                    f,
                    "{path} is fenced: never read, never indexed, never written"
                )
            }
            Refusal::Vault(fault) => fault.fmt(f),
        }
    }
}

impl std::error::Error for Refusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Refusal::Vault(fault) => Some(fault),
            _ => None,
        }
    }
}
