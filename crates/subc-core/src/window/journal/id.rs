use std::{
    collections::hash_map::RandomState,
    hash::BuildHasher,
    time::{SystemTime, UNIX_EPOCH},
};

use ck_ui::time::{Civil, Timestamp, UtcOffset};

pub fn valid(id: &str) -> bool {
    if !crate::setup::install_journal::valid_id(id) || id.len() != 20 {
        return false;
    }
    let (date, time, suffix) = (&id[..8], &id[9..15], &id[16..]);
    if id.as_bytes()[8] != b'-'
        || id.as_bytes()[15] != b'-'
        || !date
            .bytes()
            .chain(time.bytes())
            .all(|byte| byte.is_ascii_digit())
        || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return false;
    }
    let (year, month, day) = (&date[..4], &date[4..6], &date[6..]);
    let (h, m, s) = (&time[..2], &time[2..4], &time[4..]);
    let input = format!("{year}-{month}-{day}T{h}:{m}:{s}Z");
    Timestamp::parse_utc(&input).is_some_and(|time| input == rfc3339(time.civil(UtcOffset::UTC)))
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn civil(ms: u64) -> Civil {
    Timestamp::from_unix_millis(i64::try_from(ms).unwrap_or(i64::MAX)).civil(UtcOffset::UTC)
}

fn rfc3339(time: Civil) -> String {
    format!(
        "{:04}-{:02}-{:02}T{}Z",
        time.year,
        time.month,
        time.day,
        time.hms()
    )
}

pub(crate) fn log_time(ms: u64) -> String {
    rfc3339(civil(ms))
}

fn prefix(ms: u64) -> String {
    let Civil {
        year,
        month,
        day,
        hour,
        minute,
        second,
        ..
    } = civil(ms);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-")
}

pub(crate) fn candidate(ms: u64, suffix: u16) -> String {
    format!("{}{suffix:04x}", prefix(ms))
}

pub(super) fn matches_begin(id: &str, ms: u64) -> bool {
    id.starts_with(&prefix(ms))
}

pub(crate) fn suffix() -> u16 {
    // The suffix only spreads collisions. Exclusive directory creation, not
    // the random value, reserves it.
    RandomState::new().hash_one(0_u8) as u16
}

pub(crate) fn validate_note(note: Option<&str>) -> Result<(), String> {
    if note.is_some_and(|note| note.chars().count() > 200 || note.chars().any(char::is_control)) {
        Err("window note must be at most 200 characters with no control characters".into())
    } else {
        Ok(())
    }
}
