//! The two seams. A capability with one implementation does not earn a trait, so there are two.

use crate::types::{Entry, Fault, SessionId};

/// Time is injected, never read from the ambient world, so tests run offline and deterministic.
pub trait Clock {
    fn now(&self) -> String;
}

/// The one seam that persists the log. Append-only by contract: no method removes or rewrites an
/// entry, and a fork writes a new file rather than touching the source.
pub trait SessionStore {
    /// Write a new session made of exactly these entries, in order.
    fn create(&self, session: &SessionId, entries: &[Entry]) -> Result<(), Fault>;
    /// Read a session back, failing loudly on the first malformed line.
    fn load(&self, session: &SessionId) -> Result<Vec<Entry>, Fault>;
    /// Add one entry to the end of an existing session.
    fn append(&self, session: &SessionId, entry: &Entry) -> Result<(), Fault>;
    /// Whether a session is already stored. A fork uses this to pick an id it will not clobber.
    fn exists(&self, session: &SessionId) -> bool;
}
