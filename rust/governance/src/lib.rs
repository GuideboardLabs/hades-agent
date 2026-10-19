//! Governance: who may write memory, what counts as durable, and what the audit has to catch.
//!
//! A port of the Python spec at `hades/curator.py`. The tests in `tests/port_oracle.rs` mirror the
//! Python suite, and the port counts as done when those behaviours hold here unchanged.
//!
//! Carried over from our own OverCR work, and the reason Mneme is a substrate rather than a folder:
//! provenance on every write, exactly one writer, fences that are never read and never indexed, and an
//! audit that reports what is wrong instead of reassuring.
//!
//! **The one-writer rule is a type here, not a promise.** In Python it is a class docstring backed by a
//! test that scans the package for the write calls, because nothing in the language can say "only this
//! component writes". Here the move into an area takes a [`mneme::WritePermit`], whose only constructor
//! is `Vault::permit`, and the layer that writes the audit trail reads the holder back out of it. The
//! Python's structural test stays on as the regression guard on the Python side; on this side there is
//! nothing left to scan for.
//!
//! One seam, and it is the timestamp: the store and the day are already injected one layer down, inside
//! the vault this holds, so a second seam here would be ceremony. Everything else is a type.

pub mod curator;
pub mod problem;
pub mod refusal;
pub mod report;
pub mod timestamp;

pub use curator::{Curation, Curator};
pub use problem::{Problem, ProblemKind};
pub use refusal::Refusal;
pub use report::AuditReport;
pub use timestamp::{SystemTimestamp, Timestamp};

use mneme::Area;

/// The fences a vault runs with unless it is told otherwise: the personal areas, fenced out of every
/// read and every index.
pub const FENCE_DEFAULT: [&str; 3] = ["2-Areas", "3-Resources/People", "People"];

/// A note may be filed into these and stay memory. Every other area is a queue or a hub.
pub const DURABLE_AREAS: [Area; 2] = [Area::Research, Area::Projects];

/// The Inbox is a human review queue, and past this many candidates the queue is the problem.
pub const INBOX_CAP: usize = 30;

/// How long a capture may sit before the audit reports it rather than quietly keeping it.
pub const STALE_DAYS: i64 = mneme::INBOX_EXPIRY_DAYS;
