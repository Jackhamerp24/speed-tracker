use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// An instant in UTC, counted in 100 ns ticks since the Unix epoch. The history file was first
/// written by .NET, whose timestamps have this resolution, so nothing is lost reading it back.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Time(pub i64);

pub const TICKS_PER_SECOND: i64 = 10_000_000;
pub const TICKS_PER_MILLISECOND: i64 = 10_000;
/// The last millisecond of year 9999: epoch values at or past it are not dates.
pub const MAX_UNIX_MS: f64 = 253_402_300_799_999.0;

impl Time {
    pub const MIN: Time = Time(i64::MIN / 4);

    pub fn now() -> Time {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => Time((elapsed.as_nanos() / 100) as i64),
            Err(_) => Time(0),
        }
    }
    pub fn from_unix_ms(milliseconds: i64) -> Time {
        Time(milliseconds.saturating_mul(TICKS_PER_MILLISECOND))
    }
    pub fn from_unix_seconds(seconds: i64) -> Time {
        Time(seconds.saturating_mul(TICKS_PER_SECOND))
    }
    pub fn from_system(time: SystemTime) -> Time {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Time((after.as_nanos() / 100) as i64),
            Err(before) => Time(-((before.duration().as_nanos() / 100) as i64)),
        }
    }
    pub fn unix_ms(self) -> i64 {
        self.0.div_euclid(TICKS_PER_MILLISECOND)
    }
    pub fn unix_seconds(self) -> i64 {
        self.0.div_euclid(TICKS_PER_SECOND)
    }
    /// Seconds from `earlier` to `self`; negative when `self` is the earlier one.
    pub fn since(self, earlier: Time) -> f64 {
        (self.0 - earlier.0) as f64 / TICKS_PER_SECOND as f64
    }
    // Whole units and the fraction are converted apart, so large values keep tick precision.
    fn add_units(self, value: f64, ticks_per_unit: i64) -> Time {
        if !value.is_finite() {
            return self;
        }
        let whole = value.trunc();
        let ticks = (whole as i64)
            .saturating_mul(ticks_per_unit)
            .saturating_add(((value - whole) * ticks_per_unit as f64) as i64);
        Time(self.0.saturating_add(ticks))
    }
    pub fn add_seconds(self, seconds: f64) -> Time {
        self.add_units(seconds, TICKS_PER_SECOND)
    }
    pub fn add_ms(self, milliseconds: f64) -> Time {
        self.add_units(milliseconds, TICKS_PER_MILLISECOND)
    }
    pub fn add_minutes(self, minutes: f64) -> Time {
        self.add_units(minutes, 60 * TICKS_PER_SECOND)
    }
    pub fn add_days(self, days: f64) -> Time {
        self.add_units(days, 86_400 * TICKS_PER_SECOND)
    }

    /// Reads an ISO 8601 date and time. A value with no offset is taken as UTC.
    pub fn parse(text: &str) -> Option<Time> {
        let bytes = text.trim().as_bytes();
        let mut index = 0;
        let number = |index: &mut usize, digits: usize| -> Option<i64> {
            let end = *index + digits;
            let part = bytes.get(*index..end)?;
            if !part.iter().all(u8::is_ascii_digit) {
                return None;
            }
            *index = end;
            std::str::from_utf8(part).ok()?.parse().ok()
        };
        let expect = |index: &mut usize, value: u8| -> Option<()> {
            (bytes.get(*index) == Some(&value)).then(|| *index += 1)
        };
        let year = number(&mut index, 4)?;
        expect(&mut index, b'-')?;
        let month = number(&mut index, 2)?;
        expect(&mut index, b'-')?;
        let day = number(&mut index, 2)?;
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
            return None;
        }
        let (mut hour, mut minute, mut second, mut fraction) = (0, 0, 0, 0i64);
        if index < bytes.len() {
            if !matches!(bytes[index], b'T' | b't' | b' ') {
                return None;
            }
            index += 1;
            hour = number(&mut index, 2)?;
            expect(&mut index, b':')?;
            minute = number(&mut index, 2)?;
            if bytes.get(index) == Some(&b':') {
                index += 1;
                second = number(&mut index, 2)?;
                if matches!(bytes.get(index), Some(b'.' | b',')) {
                    index += 1;
                    let start = index;
                    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                        index += 1;
                    }
                    if index == start {
                        return None;
                    }
                    // Seven digits fill a tick; anything finer is dropped.
                    let mut scale = TICKS_PER_SECOND / 10;
                    for digit in &bytes[start..index.min(start + 7)] {
                        fraction += (digit - b'0') as i64 * scale;
                        scale /= 10;
                    }
                }
            }
            if hour > 23 || minute > 59 || second > 59 {
                return None;
            }
        }
        let mut offset = 0;
        match bytes.get(index) {
            None => {}
            Some(b'Z' | b'z') => index += 1,
            Some(sign @ (b'+' | b'-')) => {
                let sign = if *sign == b'-' { -1 } else { 1 };
                index += 1;
                let hours = number(&mut index, 2)?;
                if bytes.get(index) == Some(&b':') {
                    index += 1;
                }
                let minutes = if index < bytes.len() {
                    number(&mut index, 2)?
                } else {
                    0
                };
                if hours > 14 || minutes > 59 {
                    return None;
                }
                offset = sign * (hours * 3600 + minutes * 60);
            }
            Some(_) => return None,
        }
        if index != bytes.len() {
            return None;
        }
        let seconds =
            days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
                - offset;
        Some(Time(seconds * TICKS_PER_SECOND + fraction))
    }

    /// Year, month, day, hour, minute, second and leftover ticks, in UTC.
    pub fn civil(self) -> (i64, i64, i64, i64, i64, i64, i64) {
        let seconds = self.0.div_euclid(TICKS_PER_SECOND);
        let ticks = self.0.rem_euclid(TICKS_PER_SECOND);
        let days = seconds.div_euclid(86_400);
        let rest = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        (
            year,
            month,
            day,
            rest / 3600,
            rest % 3600 / 60,
            rest % 60,
            ticks,
        )
    }
}

/// `2026-10-08T12:34:56.1234567+00:00`, with the fraction trimmed of trailing zeros.
impl fmt::Display for Time {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day, hour, minute, second, ticks) = self.civil();
        write!(
            formatter,
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
        )?;
        if ticks != 0 {
            let digits = format!("{ticks:07}");
            write!(formatter, ".{}", digits.trim_end_matches('0'))?;
        }
        formatter.write_str("+00:00")
    }
}

impl Serialize for Time {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Time {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Time, D::Error> {
        let text = String::deserialize(deserializer)?;
        Time::parse(&text).ok_or_else(|| serde::de::Error::custom("not an ISO 8601 date"))
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Howard Hinnant's civil-date algorithms: days since 1970-01-01 in the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// The earlier of two optional instants; a missing one never wins.
pub fn earlier(first: Option<Time>, second: Option<Time>) -> Option<Time> {
    match (first, second) {
        (None, other) | (other, None) => other,
        (Some(a), Some(b)) => Some(a.min(b)),
    }
}
