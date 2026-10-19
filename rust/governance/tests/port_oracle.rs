//! The porting oracle against the file store.
//!
//! The assertions live in `common/port_body.rs` so the same suite runs against `SqliteStore` in
//! `sqlite_oracle.rs`. Nothing in the body knows which store it is driving.

use mneme::FileVault as Store;

include!("common/port_body.rs");
