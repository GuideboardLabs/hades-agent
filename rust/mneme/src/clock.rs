//! The real clock. The only place the vault reads the world's time.

use crate::date::Date;
use crate::seams::Clock;

/// Today, from the system clock, counted in UTC days.
///
/// A UTC day can differ from the local day by one at the boundary, and the vault does not care: it
/// compares days and never hours. A surface that wants the user's local date passes its own [`Clock`]
/// rather than reading the world here.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn today(&self) -> Date {
        let since_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the system clock is set after 1970");
        Date::from_days((since_epoch.as_secs() / 86_400) as i64)
    }
}
