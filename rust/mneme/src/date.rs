//! A day, and only a day.
//!
//! The vault asks three things of a date: stamp a capture, work out the stale cutoff, and print the
//! index header. All three are day-granular, so this is a calendar date and nothing more. The
//! arithmetic is Howard Hinnant's days-from-civil, twenty lines that turn a date into a day number
//! and back, which is what keeps the crate's dependency list empty.

use std::fmt;

/// A proleptic Gregorian date. Ordering is chronological, because the fields are in that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    pub year: i64,
    pub month: u32,
    pub day: u32,
}

impl Date {
    pub const EPOCH: Date = Date {
        year: 1970,
        month: 1,
        day: 1,
    };

    /// A day that exists, or nothing. The 30th of February is not a date.
    pub fn new(year: i64, month: u32, day: u32) -> Option<Date> {
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return None;
        }
        Some(Date { year, month, day })
    }

    /// `YYYY-MM-DD`, exactly, the way `date.fromisoformat` reads it. Anything else is not a day,
    /// and a note carrying a stamp no one can read is skipped rather than guessed at.
    pub fn parse_iso(text: &str) -> Option<Date> {
        let bytes = text.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return None;
        }
        for (index, byte) in bytes.iter().enumerate() {
            if index != 4 && index != 7 && !byte.is_ascii_digit() {
                return None;
            }
        }
        let year = text[0..4].parse().ok()?;
        let month = text[5..7].parse().ok()?;
        let day = text[8..10].parse().ok()?;
        Date::new(year, month, day)
    }

    pub fn to_iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// The day this many days earlier, across months and years.
    pub fn minus_days(self, days: i64) -> Date {
        Date::from_days(self.to_days() - days)
    }

    /// Days since 1970-01-01.
    pub fn to_days(self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }

    pub fn from_days(days: i64) -> Date {
        let (year, month, day) = civil_from_days(days);
        Date { year, month, day }
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_iso())
    }
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month as i64 + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_day_zero() {
        assert_eq!(Date::EPOCH.to_days(), 0);
        assert_eq!(Date::from_days(0), Date::EPOCH);
    }

    #[test]
    fn a_day_survives_a_round_trip_through_day_numbers() {
        for iso in [
            "1970-01-01",
            "1969-12-31",
            "2000-02-29",
            "1900-03-01",
            "2024-12-31",
            "2026-10-17",
        ] {
            let date = Date::parse_iso(iso).expect(iso);
            assert_eq!(date.to_iso(), iso);
            assert_eq!(Date::from_days(date.to_days()), date, "{iso}");
            assert_eq!(date.minus_days(0), date, "{iso}");
        }
    }

    #[test]
    fn days_that_do_not_exist_are_refused() {
        for text in [
            "2026-02-30",
            "2026-13-01",
            "2026-00-10",
            "2026-01-00",
            "2026-1-1",
            "20261017",
            "2026-10-17T00:00:00",
            "",
            "not a day",
        ] {
            assert!(Date::parse_iso(text).is_none(), "{text:?}");
        }
        assert!(
            Date::parse_iso("2024-02-29").is_some(),
            "a leap day is a day"
        );
    }

    #[test]
    fn minus_days_crosses_a_month_and_a_year() {
        let new_year = Date::parse_iso("2026-01-01").unwrap();
        assert_eq!(new_year.minus_days(1).to_iso(), "2025-12-31");
        assert_eq!(new_year.minus_days(14).to_iso(), "2025-12-18");
        let march = Date::parse_iso("2024-03-01").unwrap();
        assert_eq!(march.minus_days(1).to_iso(), "2024-02-29");
    }

    #[test]
    fn ordering_is_chronological() {
        assert!(Date::parse_iso("2025-12-31").unwrap() < Date::parse_iso("2026-01-01").unwrap());
        assert!(Date::parse_iso("2026-01-02").unwrap() < Date::parse_iso("2026-02-01").unwrap());
    }
}
