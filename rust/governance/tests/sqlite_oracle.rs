//! The same governance oracle, against the SQLite store.
//!
//! This is the gate in `STORAGE.md`: the audit, the stamp and the refusals must hold with the record
//! in the database. The body is shared verbatim with `port_oracle.rs`.

use mneme::SqliteStore as Store;

include!("common/port_body.rs");
