//! Fallibility is a type, and every variant names its class.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// Policy. A note may not leave the Inbox without a topic, and the note stays where it was.
    OrphanNote { path: String },
    /// Policy. The note breaks a structural rule for its area (a topic without a category, a
    /// project without research). The sentence says which rule, so the writer can fix it.
    InvalidNote { path: String, detail: String },
    /// Infrastructure. The note named is not on disk.
    MissingNote { path: String },
    /// Policy. A thousand notes share this name and the vault will not guess further.
    NoFreeName { path: String },
    /// Infrastructure. The disk refused, and the detail is whatever it said.
    Io { path: String, detail: String },
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::OrphanNote { path } => {
                write!(
                    f,
                    "{path}: a note needs a topic before it can leave the Inbox"
                )
            }
            Fault::InvalidNote { path, detail } => write!(f, "{path}: {detail}"),
            Fault::MissingNote { path } => write!(f, "{path}: no such note"),
            Fault::NoFreeName { path } => {
                write!(f, "{path}: cannot find a free name next to it")
            }
            Fault::Io { path, detail } => write!(f, "{path}: {detail}"),
        }
    }
}

impl std::error::Error for Fault {}
