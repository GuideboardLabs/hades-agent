//! The timestamp the audit trail is written against.
//!
//! Governance's one seam. The store and the day arrive already injected inside the vault this layer
//! holds, so what a curator adds of its own is the moment a note was curated, and that is injected here.

use std::time::{SystemTime, UNIX_EPOCH};

use mneme::Date;

/// Time is injected, never read from the ambient world, so the audit trail is the same in a test as it
/// is in production and a report can be diffed.
pub trait Timestamp {
    fn now(&self) -> String;
}

/// UTC at second precision, in the shape the Python writes: `2026-10-17T00:00:00+00:00`.
///
/// Seconds rather than nanoseconds on purpose: two notes curated in the same second are the same event
/// as far as anything downstream is concerned, and a stable width keeps the notes diffable.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemTimestamp;

impl Timestamp for SystemTimestamp {
    fn now(&self) -> String {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the system clock is set after 1970");
        let days = (since_epoch.as_secs() / 86_400) as i64;
        let seconds = since_epoch.as_secs() % 86_400;
        format!(
            "{}T{:02}:{:02}:{:02}+00:00",
            Date::from_days(days).to_iso(),
            seconds / 3_600,
            (seconds % 3_600) / 60,
            seconds % 60
        )
    }
}
