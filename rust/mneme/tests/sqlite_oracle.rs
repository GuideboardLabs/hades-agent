//! The same porting oracle, against the SQLite store.
//!
//! This is the gate in `STORAGE.md`: if an assertion here needs a different call shape, the store has
//! leaked its implementation. The body is shared verbatim with `port_oracle.rs`.

use mneme::SqliteStore as Store;

include!("common/port_body.rs");
