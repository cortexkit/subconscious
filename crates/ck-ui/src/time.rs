//! Instants, UTC offsets and the number and duration formats the screens use.
//!
//! The screens never read the process time zone: every renderer is handed a
//! `now` and the machine's UTC offset, so a frame drawn for a given snapshot is
//! the same on every computer. Number formatting follows JavaScript's rules
//! (`Math.round`, `toFixed`, `toLocaleString("en-US")`) because the approved
//! design frames were produced with them, and a half-way value must round the
//! same way here or a cell differs.

use std::time::{SystemTime, UNIX_EPOCH};

/// A point in time, in milliseconds since the Unix epoch (UTC).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    pub const fn from_unix_millis(ms: i64) -> Self {
        Self(ms)
    }

    pub const fn unix_millis(self) -> i64 {
        self.0
    }

    /// The system clock. Only the app's run loop calls this; renderers take a
    /// `now` argument instead.
    pub fn system_now() -> Self {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self(ms)
    }

    pub fn add_millis(self, ms: i64) -> Self {
        Self(self.0.saturating_add(ms))
    }

    pub fn add_secs(self, secs: f64) -> Self {
        self.add_millis(js_round(secs * 1000.0) as i64)
    }

    /// Seconds from `earlier` to `self`; negative when `earlier` is later.
    pub fn secs_since(self, earlier: Timestamp) -> f64 {
        (self.0 - earlier.0) as f64 / 1000.0
    }

    /// Parses `YYYY-MM-DDTHH:MM:SS[.fff]Z`, the only form the fixture uses.
    pub fn parse_utc(s: &str) -> Option<Self> {
        let s = s.strip_suffix('Z')?;
        let (date, time) = s.split_once('T')?;
        let mut d = date.split('-');
        let year: i64 = d.next()?.parse().ok()?;
        let month: u32 = d.next()?.parse().ok()?;
        let day: u32 = d.next()?.parse().ok()?;
        if d.next().is_some() {
            return None;
        }
        let (hms, frac) = match time.split_once('.') {
            Some((hms, frac)) => (hms, Some(frac)),
            None => (time, None),
        };
        let mut t = hms.split(':');
        let hour: i64 = t.next()?.parse().ok()?;
        let minute: i64 = t.next()?.parse().ok()?;
        let second: i64 = t.next()?.parse().ok()?;
        if t.next().is_some() || month == 0 || month > 12 || day == 0 || day > 31 {
            return None;
        }
        let millis: i64 = match frac {
            Some(f) if f.len() == 3 => f.parse().ok()?,
            Some(_) => return None,
            None => 0,
        };
        let days = days_from_civil(year, month, day);
        Some(Self(
            ((days * 86_400 + hour * 3600 + minute * 60 + second) * 1000) + millis,
        ))
    }

    /// The wall-clock reading of this instant at the given UTC offset.
    pub fn civil(self, offset: UtcOffset) -> Civil {
        let local = self.0 + i64::from(offset.0) * 1000;
        let days = local.div_euclid(86_400_000);
        let ms_of_day = local.rem_euclid(86_400_000);
        let (year, month, day) = civil_from_days(days);
        Civil {
            days,
            year,
            month,
            day,
            hour: (ms_of_day / 3_600_000) as u32,
            minute: ((ms_of_day / 60_000) % 60) as u32,
            second: ((ms_of_day / 1000) % 60) as u32,
        }
    }
}

/// A fixed offset from UTC, in seconds east of Greenwich.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UtcOffset(pub i32);

impl UtcOffset {
    pub const UTC: UtcOffset = UtcOffset(0);
}

/// A wall-clock reading: calendar date and time of day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    /// Days since 1970-01-01 in the same offset, for "today" and "yesterday".
    pub days: i64,
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Civil {
    /// `HH:MM`.
    pub fn hm(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }

    /// `HH:MM:SS`.
    pub fn hms(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }
}

// Howard Hinnant's days-from-civil algorithm (proleptic Gregorian calendar).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// JavaScript's `Math.round`: the nearest integer, and halves go up (toward
/// positive infinity), unlike `f64::round`, which takes halves away from zero.
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// JavaScript's `Number.prototype.toFixed`: fixed decimals, where an exact
/// half goes away from zero. Rust's formatter takes exact halves to even, so
/// those are handled here and everything else is left to the formatter.
pub fn to_fixed(x: f64, digits: usize) -> String {
    let p = 10f64.powi(digits as i32);
    let scaled = x.abs() * p;
    if scaled.fract() == 0.5 && scaled / p == x.abs() && scaled < 1e15 {
        let n = scaled.floor() as u64 + 1;
        let sign = if x < 0.0 { "-" } else { "" };
        if digits == 0 {
            return format!("{sign}{n}");
        }
        let unit = 10u64.pow(digits as u32);
        return format!("{sign}{}.{:0width$}", n / unit, n % unit, width = digits);
    }
    format!("{:.*}", digits, x)
}

/// A number the way `toLocaleString("en-US")` writes it: thousands
/// separators and at most three decimals, without trailing zeros.
pub fn locale_number(x: f64) -> String {
    let fixed = to_fixed(x, 3);
    let (sign, fixed) = match fixed.strip_prefix('-') {
        Some(rest) => ("-", rest.to_string()),
        None => ("", fixed),
    };
    let (int, frac) = fixed.split_once('.').unwrap_or((fixed.as_str(), ""));
    let frac = frac.trim_end_matches('0');
    let mut grouped = String::new();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    if frac.is_empty() {
        format!("{sign}{grouped}")
    } else {
        format!("{sign}{grouped}.{frac}")
    }
}

/// An integer with thousands separators: `10,258`.
pub fn thousands(n: u64) -> String {
    locale_number(n as f64)
}

/// A duration in at most its two largest units: `42s`, `7m`, `2h 14m`,
/// `3d 4h`. Below one second it still says `1s`.
pub fn ago(sec: f64) -> String {
    if sec < 60.0 {
        return format!("{}s", js_round(sec).max(1.0) as i64);
    }
    if sec < 3600.0 {
        return format!("{}m", (sec / 60.0).floor() as i64);
    }
    if sec < 86_400.0 {
        return format!(
            "{}h {}m",
            (sec / 3600.0).floor() as i64,
            ((sec % 3600.0) / 60.0).floor() as i64
        );
    }
    format!(
        "{}d {}h",
        (sec / 86_400.0).floor() as i64,
        ((sec % 86_400.0) / 3600.0).floor() as i64
    )
}

/// A duration in its largest unit only: `2d`, `5h`, `12m`.
pub fn ago_short(sec: f64) -> String {
    let full = ago(sec);
    full.split(' ').next().unwrap_or("").to_string()
}

/// Parses a duration written the way `ago` writes one (`9h 25m`, `6d 11h`,
/// `5m`, `42s`) back into seconds.
pub fn parse_duration(s: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut any = false;
    for part in s.split_whitespace() {
        let unit_at = part.char_indices().last()?.0;
        let (n, unit) = part.split_at(unit_at);
        let n: f64 = n.parse().ok()?;
        total += n * match unit {
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            "d" => 86_400.0,
            _ => return None,
        };
        any = true;
    }
    any.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_utc() {
        let t = Timestamp::parse_utc("2026-10-20T20:48:00Z").unwrap();
        let c = t.civil(UtcOffset::UTC);
        assert_eq!((c.year, c.month, c.day), (2026, 10, 20));
        assert_eq!(c.hm(), "20:48");
        let east = t.civil(UtcOffset(2 * 3600));
        assert_eq!((east.day, east.hm()), (20, "22:48".to_string()));
        let west = Timestamp::parse_utc("2026-10-20T00:10:05.250Z")
            .unwrap()
            .civil(UtcOffset(-3600));
        assert_eq!((west.day, west.hms()), (19, "23:10:05".to_string()));
    }

    #[test]
    fn js_rounding_rules() {
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(to_fixed(0.25, 1), "0.3");
        assert_eq!(to_fixed(12.5, 0), "13");
        assert_eq!(to_fixed(14.2, 0), "14");
        assert_eq!(to_fixed(4.26, 1), "4.3");
        assert_eq!(to_fixed(5.0, 1), "5.0");
        assert_eq!(locale_number(41.2), "41.2");
        assert_eq!(locale_number(1000.0), "1,000");
        assert_eq!(thousands(10_258), "10,258");
    }

    #[test]
    fn durations() {
        assert_eq!(ago(0.2), "1s");
        assert_eq!(ago(180.0), "3m");
        assert_eq!(ago(33_300.0), "9h 15m");
        assert_eq!(ago(2.0 * 86_400.0), "2d 0h");
        assert_eq!(ago_short(2.0 * 86_400.0), "2d");
        assert_eq!(parse_duration("9h 25m"), Some(33_900.0));
        assert_eq!(parse_duration("—"), None);
    }
}
