//! The session tree: append-only, forkable, and never lossy.
//!
//! A port of the Python spec at `hades/session/tree.py`. The tests in `tests/port_oracle.rs` mirror
//! the 24 Python tests, and the port counts as done when those behaviours hold here unchanged.
//!
//! Two seams and nothing else, because varying parameters are values rather than seams:
//! [`seams::SessionStore`] persists the log and [`seams::Clock`] supplies time. Everything else is a
//! type.

pub mod clock;
pub mod seams;
pub mod session;
pub mod store;
pub mod types;

pub use clock::SystemClock;
pub use seams::{Clock, SessionStore};
pub use session::Session;
pub use store::FileStore;
pub use types::{Entry, EntryId, Fault, Kind, SessionId};

/// The schema version written on every entry. A newer version is a load error, never a guess.
pub const SCHEMA_VERSION: u64 = 1;
