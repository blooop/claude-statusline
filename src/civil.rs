//! The calendar arithmetic `parse_reset` needs, and nothing more.
//!
//! The status line never prints a date — it prints *differences* of epoch
//! seconds (`fmt_eta`) and a fraction of a window (`even`). So the only thing a
//! date library would be asked for here is "turn this ISO-8601 string into an
//! epoch second", which is Howard Hinnant's `days_from_civil` plus field
//! validation. That is cheap enough to own, and owning it keeps the binary's
//! dependency list at one crate.

/// Epoch seconds of `0001-01-01T00:00:00Z` — Python's `datetime.min` in UTC.
pub const DT_MIN: f64 = -62_135_596_800.0;
/// Epoch seconds of `9999-12-31T23:59:59.999999Z` — Python's `datetime.max`.
pub const DT_MAX: f64 = 253_402_300_799.999_999;

/// Days since the Unix epoch for a proleptic-Gregorian civil date.
///
/// Howard Hinnant's algorithm (`chrono`-compatible, exact for the whole range
/// Python's `datetime` covers).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// A parsed civil timestamp, already reduced to an offset from UTC.
pub struct Civil {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    pub micro: i64,
    /// UTC offset in seconds; `None` for a naive timestamp, which
    /// `parse_reset` reads as UTC.
    pub offset: Option<i64>,
}

impl Civil {
    /// Validate the fields the way `datetime(...)` would, then reduce to epoch
    /// seconds. `None` for any field Python would have raised `ValueError` on.
    pub fn to_epoch(&self) -> Option<f64> {
        if !(1..=9999).contains(&self.year)
            || !(1..=12).contains(&self.month)
            || self.day < 1
            || self.day > days_in_month(self.year, self.month)
            || !(0..=23).contains(&self.hour)
            || !(0..=59).contains(&self.minute)
            || !(0..=59).contains(&self.second)
            || !(0..1_000_000).contains(&self.micro)
        {
            return None;
        }
        let days = days_from_civil(self.year, self.month, self.day);
        let secs = days * 86_400 + self.hour * 3600 + self.minute * 60 + self.second;
        let secs = secs - self.offset.unwrap_or(0);
        Some(secs as f64 + self.micro as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_day_zero() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn known_dates_round_trip() {
        assert_eq!(days_from_civil(2024, 3, 1) * 86_400, 1_709_251_200);
        assert_eq!(days_from_civil(1, 1, 1) * 86_400, DT_MIN as i64);
    }

    #[test]
    fn february_knows_about_leap_years() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
    }

    #[test]
    fn an_out_of_range_field_is_rejected() {
        let base = Civil {
            year: 2024,
            month: 2,
            day: 30,
            hour: 0,
            minute: 0,
            second: 0,
            micro: 0,
            offset: None,
        };
        assert!(base.to_epoch().is_none());
    }
}
