//! Reading the quota module's `usage.get` rows: which provider and account a
//! row is, every shape a usage window comes in (the `primary`, `secondary`
//! and `tertiary` slots and the `extraRateWindows` list, whose numbers sit
//! under `window`), spend pools, errors and how disconnected rows are
//! classed. `ck quota` prints from these and the app's Usage panel reads
//! them, so the two never disagree about what a row says. The names accounts
//! are shown under, with and without `ck quota --redact`, are here for the
//! same reason.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::metrics::format_minor_amount;

/// The quota module, as the daemon knows it.
pub const USAGE_MODULE: &str = "insula";

/// The quota module's operation that returns one row per provider account.
/// It takes no parameters.
pub const USAGE_METHOD: &str = "usage.get";

/// The rows of a `usage.get` reply: the array under `result`. Any other
/// shape, an error reply included, is `None`.
pub fn usage_rows(reply: &Value) -> Option<&Vec<Value>> {
    reply.get("result").and_then(Value::as_array)
}

pub fn provider_id(provider: &Value) -> String {
    provider
        .get("provider")
        .or_else(|| provider.get("provider_id"))
        .or_else(|| provider.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_string()
}

pub fn account_label(entry: &Value) -> String {
    entry
        .get("account")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_default()
}

pub fn table_account_label(entry: &Value) -> String {
    shorten_uuid_label(&account_label(entry))
}

/// The name an account row is shown under.
///
/// Without `redact` it is the account's email when the row carries one, else
/// its account id (a UUID cut to its first eight characters), else
/// "<credential source> account", else "account", so a row is never
/// nameless.
///
/// With `redact` it is [`account_pseudonym`] instead, for every row. The
/// account id is replaced too: a UUID names no person, but it is the same in
/// every reading, so it identifies the account to anyone who sees two
/// screenshots.
pub fn account_name(entry: &Value, redact: bool) -> String {
    if redact {
        return account_pseudonym(entry);
    }
    let name = entry
        .get("accountInfo")
        .and_then(|info| info.get("email"))
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| table_account_label(entry));
    if !name.is_empty() {
        return name;
    }
    entry
        .get("source")
        .and_then(Value::as_str)
        .map(|source| format!("{source} account"))
        .unwrap_or_else(|| "account".to_string())
}

/// The account's organization name, or `None` when the row has none or
/// `redact` is set.
///
/// Redaction must hide this as well as the email: a personal organization is
/// usually named after its owner ("Alex's Organization"), so hiding only the
/// email would still show the owner's name next to the account.
pub fn account_org(entry: &Value, redact: bool) -> Option<String> {
    if redact {
        return None;
    }
    entry
        .get("accountInfo")
        .and_then(|info| info.get("orgName"))
        .and_then(Value::as_str)
        .filter(|org| !org.is_empty())
        .map(str::to_string)
}

/// The name an account is shown under while account identifiers are hidden
/// (`ck quota --redact`): "account " and four hex digits taken from a
/// SHA-256 of the account id, or of the email when the row has no id.
/// "account" alone when the row has neither.
///
/// It is not blank, because one provider often has several accounts, and
/// without a name per account a reader can't tell which windows belong
/// together or how many accounts there are. Four hex digits keep the name
/// short; two accounts can still share one, rarely.
///
/// It is derived from the account rather than numbered by position, because
/// a position changes when rows are reordered, and the same account must
/// read the same in two screenshots taken minutes apart. The account id comes
/// first because it changes less often than the email.
pub fn account_pseudonym(entry: &Value) -> String {
    let seed = entry
        .get("account")
        .and_then(Value::as_str)
        .filter(|account| !account.is_empty())
        .or_else(|| {
            entry
                .get("accountInfo")
                .and_then(|info| info.get("email"))
                .and_then(Value::as_str)
                .filter(|email| !email.is_empty())
        })
        .unwrap_or("");
    if seed.is_empty() {
        return "account".to_string();
    }
    let digest = Sha256::digest(seed.as_bytes());
    format!("account {:02x}{:02x}", digest[0], digest[1])
}

/// The values `--redact` hides for one row: its email, its account id
/// (whole, and shortened as [`account_name`] shows it) and its organization's
/// name. Empty values are left out.
pub fn account_identifiers(entry: &Value) -> Vec<String> {
    let email = entry
        .get("accountInfo")
        .and_then(|info| info.get("email"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let id = account_label(entry);
    let short = shorten_uuid_label(&id);
    let org = account_org(entry, false).unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for value in [email, id, short, org] {
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

/// `text` with every occurrence of each of `identifiers` replaced by
/// `pseudonym`. A provider's error sentence can quote the account it is
/// about, so hiding account names must reach that text too. The longest
/// identifier goes first, so a shortened id never leaves the rest of its
/// whole id behind. Only the identifiers given are replaced; nothing else in
/// the text is guessed at.
pub fn mask_identifiers(text: &str, identifiers: &[String], pseudonym: &str) -> String {
    let mut ordered: Vec<&String> = identifiers.iter().filter(|i| !i.is_empty()).collect();
    ordered.sort_by_key(|i| std::cmp::Reverse(i.len()));
    let mut out = text.to_string();
    for identifier in ordered {
        out = out.replace(identifier.as_str(), pseudonym);
    }
    out
}

pub fn shorten_uuid_label(label: &str) -> String {
    if is_uuid_shaped(label) {
        label[..8].to_string()
    } else {
        label.to_string()
    }
}

pub fn is_uuid_shaped(label: &str) -> bool {
    let bytes = label.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

pub fn entry_error_detail(entry: &Value) -> Option<String> {
    entry
        .get("error")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn quota_entry_is_connected(entry: &Value) -> bool {
    // Connected is signalled by the presence of a usage object on the wire; a
    // disconnected provider carries an error string and no usage object. The
    // wire carries no explicit "ok" flag, so usage presence is the signal.
    entry.get("usage").is_some_and(Value::is_object)
}

/// A connected entry with money and no rate window: the balance cohort.
///
/// `usage.primary` absent + `spend` present identifies these structurally.
/// Disconnected entries (no usage object) are NOT balance-only — they keep
/// their own classification and ordering.
pub fn quota_entry_is_balance_only(entry: &Value) -> bool {
    let windowless = entry
        .get("usage")
        .and_then(Value::as_object)
        .is_some_and(|usage| !usage.get("primary").is_some_and(Value::is_object));
    let has_spend = entry
        .get("spend")
        .and_then(Value::as_array)
        .is_some_and(|pools| !pools.is_empty());
    windowless && has_spend
}

/// Classify a disconnected entry from the producer's `errorClass` (see the
/// field's docs in `cortexkit-provider-usage`).
///
/// An UNRECOGNISED class is [`UserFixable`](QuotaDisconnectKind::UserFixable) on
/// purpose. The class list is open and grows on the producer's side, so the
/// choice is between surfacing something we have not heard of and silently
/// filing it under "nothing to do". On an observability surface the first is a
/// line someone reads once; the second is the exact blindness this split exists
/// to remove.
///
/// An entry with NO class — any producer predating the field — is `Inert`, so an
/// older producer renders exactly as it did before rather than turning every
/// disconnected provider into an alarm.
pub fn quota_disconnect_kind(entry: &Value) -> QuotaDisconnectKind {
    match entry.get("errorClass").and_then(Value::as_str) {
        None => QuotaDisconnectKind::Inert,
        Some("credential_absent" | "no_quota_reported") => QuotaDisconnectKind::Inert,
        Some("internal_error") => QuotaDisconnectKind::ModuleDefect,
        Some(_) => QuotaDisconnectKind::UserFixable,
    }
}

/// What, if anything, a reader should do about a disconnected provider.
///
/// Not-connected used to be one number covering unrelated situations. A provider
/// nobody ever configured is a permanent, correct state; a provider whose
/// credential broke this morning is a login away from working. Counted together,
/// a provider that STOPPED working moves the total by one and produces no other
/// signal — which is how a quota exhaustion went unnoticed here before.
///
/// The split is three ways rather than two because the middle bucket carries an
/// implied instruction. "Configured but failing" means go fix your credential,
/// and that is the wrong thing to tell someone when the fault is in the quota
/// module itself: nothing they can log into or reconfigure changes the outcome.
/// A bucket whose implied action cannot work is a worse place to be than an
/// unlabelled one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDisconnectKind {
    /// Permanent and correct. Never configured, or the account genuinely has no
    /// quota to report. Nothing to do, and it must never inflate a count that is
    /// supposed to mean "something needs attention".
    Inert,
    /// A person can fix this — usually by logging in again.
    UserFixable,
    /// The quota module itself failed. Surface it as a module problem; there
    /// is nothing the user can do about it, such as logging in again.
    ModuleDefect,
}

pub fn quota_window_used_percent(window: &Value) -> Option<f64> {
    // rawUsedPercent is the provider's real utilization when a banked-reset
    // relaxed window is in effect; prefer it so 0% effective pacing never
    // reads as an idle account.
    window
        .get("rawUsedPercent")
        .and_then(Value::as_f64)
        .or_else(|| window.get("usedPercent").and_then(Value::as_f64))
}

/// Render an entry's `spend` pools as display lines, one per pool.
///
/// `spend` is a pool balance or allowance, not a rate window: render each
/// pool as a labeled amount (remaining of total when stated, or just its
/// limit), followed by any stated reset, stopped state and funding. Three wire
/// states are deliberately distinct and must stay so:
/// - `spend` ABSENT: the producer has nothing to say -> no lines.
/// - `spend: []`: the producer asked and the provider has no credit
///   product on this account -> no lines (NOT "0 credit").
/// - `spend: [...]`: one line per pool with an amount.
///
/// Pools are account-scoped; callers must never sum them across sibling
/// accounts of a provider. `unit` is a free string, not necessarily a
/// currency code (MiniMax reports "credit"), so render it verbatim.
pub fn quota_spend_lines_for_entry(entry: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(pools) = entry.get("spend").and_then(Value::as_array) else {
        return lines;
    };
    for pool in pools {
        let amount = |key| {
            let (minor, exponent, unit) = pool_amount(pool, key)?;
            Some((format_minor_amount(minor, exponent), unit))
        };
        let remaining = amount("remaining");
        let total = amount("total");
        let detail = match (remaining, total) {
            (Some((left, unit)), Some((cap, cap_unit))) if unit == cap_unit => {
                format!("{left} of {cap} {unit} left")
            }
            (Some((left, unit)), Some((cap, cap_unit))) => {
                format!("{left} {unit} of {cap} {cap_unit} left")
            }
            (Some((left, unit)), None) => format!("{left} {unit} left"),
            (None, Some((cap, unit))) => format!("limit {cap} {unit}"),
            (None, None) => continue,
        };
        let label = pool
            .get("label")
            .and_then(Value::as_str)
            .filter(|label| !label.is_empty())
            .unwrap_or("credit");
        // Each stated fact after the amount is its own clause, so none of them
        // reads as a qualifier on the number (the defect behind issue #132).
        let mut clauses = vec![format!("{label}  {detail}")];
        // A reset refills the pool; an expiry loses whatever is left. Each is
        // shown only when the provider stated it, never inferred.
        for (field, verb) in [("resetsAt", "resets"), ("expiresAt", "expires")] {
            if let Some(raw) = pool.get(field).and_then(Value::as_str) {
                let date = if parse_rfc3339_to_utc_secs(raw).is_some() {
                    raw.get(..10).unwrap_or(raw)
                } else {
                    raw
                };
                clauses.push(format!("{verb} {date}"));
            }
        }
        if pool.get("spendable").and_then(Value::as_bool) == Some(false) {
            clauses.push("stopped".to_string());
        }
        match pool.get("funding").and_then(Value::as_str) {
            Some("granted") => clauses.push("provider grant".to_string()),
            Some("purchased") => clauses.push("bought credit".to_string()),
            Some("subscription") => clauses.push("included in subscription".to_string()),
            _ => {} // Unknown funding cannot justify a claim about who pays.
        }
        lines.push(clauses.join(" · "));
    }
    lines
}

pub fn quota_window_rows_for_entry(entry: &Value) -> Vec<(String, Value)> {
    let mut rows = Vec::new();
    let usage = entry.get("usage").and_then(Value::as_object);
    let Some(usage) = usage else {
        return rows;
    };

    // THE THREE SLOTS ARE POSITIONS, NOT A RANKING, AND THEY CAN HAVE HOLES: each
    // is filled from its own optional upstream field, so `secondary` may be absent
    // while `tertiary` is present. Walk all three unconditionally and never stop at
    // the first gap -- another consumer of this wire shipped a status bar reading
    // 25% for an account whose binding constraint was a weekly at 36%, by treating
    // the first slot as the answer.
    //
    // This loop tolerates holes because it is a filter rather than a search, which
    // was luck rather than intent when it was written. The note exists so that
    // stays a decision: an "optimisation" that breaks on the first absent slot
    // compiles, passes these tests (the fixtures are dense), and reproduces that
    // bug silently.
    for slot in ["primary", "secondary", "tertiary"] {
        if let Some(window) = usage.get(slot).filter(|w| !w.is_null()) {
            rows.push((rate_window_label(window, slot), window.clone()));
        }
    }

    if let Some(extras) = usage.get("extraRateWindows").and_then(Value::as_array) {
        for extra in extras {
            let label = extra_window_label(extra);
            if let Some(window) = extra.get("window").filter(|w| !w.is_null()) {
                rows.push((label, window.clone()));
            } else {
                rows.push((label, Value::Null));
            }
        }
    }

    rows.into_iter()
        .map(|(label, window)| {
            if window.is_null() {
                (label, json!({}))
            } else {
                (label, window)
            }
        })
        .collect()
}

pub fn extra_window_label(extra: &Value) -> String {
    extra
        .get("title")
        .or_else(|| extra.get("id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "extra".to_string())
}

pub fn rate_window_label(window: &Value, slot: &str) -> String {
    if let Some(minutes) = window.get("windowMinutes").and_then(Value::as_i64) {
        return label_from_window_minutes(minutes);
    }
    slot.to_string()
}

pub fn label_from_window_minutes(minutes: i64) -> String {
    match minutes {
        m if m >= 1440 && m % 1440 == 0 => {
            let days = m / 1440;
            if days == 7 {
                "week".to_string()
            } else if days == 1 {
                "day".to_string()
            } else {
                format!("{days}d")
            }
        }
        m if m >= 60 && m % 60 == 0 => format!("{}h", m / 60),
        _ => format!("{minutes}m"),
    }
}

pub fn parse_rfc3339_to_utc_secs(raw: &str) -> Option<u64> {
    // RFC 3339 uses ASCII. Establish this before fixed byte slices, including
    // the offset, so malformed provider text cannot split a UTF-8 character.
    if raw.len() < 19 || !raw.is_ascii() {
        return None;
    }
    let bytes = raw.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: i32 = raw[0..4].parse().ok()?;
    let month: u32 = raw[5..7].parse().ok()?;
    let day: u32 = raw[8..10].parse().ok()?;
    let hour: u32 = raw[11..13].parse().ok()?;
    let minute: u32 = raw[14..16].parse().ok()?;
    let second: u32 = raw[17..19].parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    // Skip fractional seconds (".466665") — some providers emit them.
    let mut rest = &raw[19..];
    if rest.starts_with('.') {
        let end = rest[1..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    let rest = rest.trim_start();
    let offset_secs = if rest.is_empty() || rest.starts_with('Z') || rest.starts_with('z') {
        0
    } else {
        let sign = rest.chars().next().filter(|c| *c == '+' || *c == '-')?;
        let tail = &rest[1..];
        let (oh, om) = parse_hh_mm_offset(tail)?;
        let mag = (oh as i64) * 3600 + (om as i64) * 60;
        if sign == '+' {
            -mag
        } else {
            mag
        }
    };

    let days = civil_to_days(year, month, day)?;
    let secs_of_day = (hour as u64) * 3600 + (minute as u64) * 60 + (second as u64);
    let utc = (days as i64) * 86_400 + secs_of_day as i64 + offset_secs;
    if utc < 0 {
        return None;
    }
    Some(utc as u64)
}

pub fn parse_hh_mm_offset(tail: &str) -> Option<(u32, u32)> {
    let (h, m) = if let Some((h, m)) = tail.split_once(':') {
        (h, m)
    } else if tail.len() >= 4 {
        (&tail[..2], &tail[2..])
    } else {
        return None;
    };
    let oh: u32 = h.parse().ok()?;
    let om: u32 = m.parse().ok()?;
    if oh > 23 || om > 59 {
        return None;
    }
    Some((oh, om))
}

pub fn civil_to_days(year: i32, month: u32, day: u32) -> Option<i32> {
    let (y, m) = if month <= 2 {
        (year - 1, month + 12)
    } else {
        (year, month)
    };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m as i32 - 3) + 2) / 5 + day as i32 - 1 + yoe * 365 + yoe / 4 - yoe / 100;
    Some(era * 146097 + doy - 719468)
}

/// One amount of a spend pool (`remaining` or `total`): minor units, the
/// decimal exponent, and the unit, which is free text (often a currency
/// code, sometimes "credit").
pub fn pool_amount<'a>(pool: &'a Value, key: &str) -> Option<(i64, i64, &'a str)> {
    let value = pool.get(key)?;
    let minor = value.get("minor")?.as_i64()?;
    let exponent = value.get("exponent")?.as_i64()?;
    let unit = value.get("unit").and_then(Value::as_str).unwrap_or("");
    Some((minor, exponent, unit))
}

/// A spend pool's amounts and dates, for adding pools up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpendPool {
    pub remaining: Option<(i64, i64, String)>,
    pub total: Option<(i64, i64, String)>,
    /// The pool's `expiresAt`, as sent.
    pub expires_at: Option<String>,
}

/// The pools in an entry's `spend` array that state a remaining or total
/// amount, with their amounts parsed the same way
/// [`quota_spend_lines_for_entry`] parses them. Pools that state neither are
/// left out.
pub fn spend_pools(entry: &Value) -> Vec<SpendPool> {
    let Some(pools) = entry.get("spend").and_then(Value::as_array) else {
        return Vec::new();
    };
    pools
        .iter()
        .filter_map(|pool| {
            let owned = |key| pool_amount(pool, key).map(|(m, e, u)| (m, e, u.to_string()));
            let remaining = owned("remaining");
            let total = owned("total");
            if remaining.is_none() && total.is_none() {
                return None;
            }
            Some(SpendPool {
                remaining,
                total,
                expires_at: pool
                    .get("expiresAt")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

/// A provider id as a display name: "claude-console" is "Claude Console".
pub fn format_provider_display_name(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Redaction replaces the email and the account id with a pseudonym taken
    /// from the account id, the same on every call, and different for the two
    /// sample ids here; it also drops the organization name, which often
    /// names the owner.
    #[test]
    fn redaction_names_each_account_by_a_stable_pseudonym_and_drops_its_org() {
        let entry = json!({
            "account": "11111111-2222-3333-4444-555555555555",
            "accountInfo": {
                "email": "alex@example.com",
                "orgName": "Alex's Organization",
                "planType": "max",
            },
        });

        let name = account_name(&entry, true);
        assert_eq!(name, account_name(&entry, true));
        assert!(
            name.starts_with("account ")
                && name.len() == "account ".len() + 4
                && name["account ".len()..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "expected `account <4 hex digits>`, got {name}"
        );
        assert!(!name.contains("alex") && !name.contains("11111111"));

        let other = json!({ "account": "99999999-8888-7777-6666-555555555555" });
        assert_ne!(name, account_name(&other, true));

        assert_eq!(account_org(&entry, true), None);
        assert_eq!(
            account_org(&entry, false).as_deref(),
            Some("Alex's Organization")
        );
    }

    /// The pseudonym comes from the account id when there is one, so a row
    /// whose email changes keeps its name; from the email otherwise.
    #[test]
    fn the_pseudonym_is_seeded_by_the_account_id_before_the_email() {
        let with_id =
            |email: &str| json!({ "account": "acct-1", "accountInfo": { "email": email } });
        assert_eq!(
            account_name(&with_id("one@example.com"), true),
            account_name(&with_id("two@example.com"), true)
        );
        let email_only = |email: &str| json!({ "accountInfo": { "email": email } });
        assert_ne!(
            account_name(&email_only("one@example.com"), true),
            account_name(&email_only("two@example.com"), true)
        );
        assert_eq!(account_name(&json!({ "source": "oauth" }), true), "account");
    }

    /// Unredacted, a row is named by its email, its shortened account id,
    /// its credential source, or "account", in that order.
    #[test]
    fn an_account_is_named_by_email_then_id_then_source() {
        let both = json!({
            "account": "0f1e2d3c-aaaa-bbbb-cccc-000000000001",
            "accountInfo": { "email": "alex@example.com" },
        });
        assert_eq!(account_name(&both, false), "alex@example.com");
        let id_only = json!({ "account": "0f1e2d3c-aaaa-bbbb-cccc-000000000001" });
        assert_eq!(account_name(&id_only, false), "0f1e2d3c");
        assert_eq!(
            account_name(&json!({ "source": "oauth" }), false),
            "oauth account"
        );
        assert_eq!(account_name(&json!({}), false), "account");
    }

    /// The identifiers an account row carries are exactly what `--redact`
    /// hides, and masking a sentence replaces each of them, the whole id
    /// before its shortened form.
    #[test]
    fn identifiers_in_a_sentence_are_replaced_by_the_pseudonym() {
        let entry = json!({
            "account": "0f1e2d3c-aaaa-bbbb-cccc-000000000001",
            "accountInfo": { "email": "alex@example.com", "orgName": "Alex's Organization" },
        });
        let ids = account_identifiers(&entry);
        assert_eq!(
            ids,
            [
                "alex@example.com",
                "0f1e2d3c-aaaa-bbbb-cccc-000000000001",
                "0f1e2d3c",
                "Alex's Organization"
            ]
        );
        let pseudonym = account_name(&entry, true);
        let masked = mask_identifiers(
            "token for alex@example.com (0f1e2d3c-aaaa-bbbb-cccc-000000000001, 0f1e2d3c) in Alex's Organization expired",
            &ids,
            &pseudonym,
        );
        assert_eq!(
            masked,
            format!("token for {pseudonym} ({pseudonym}, {pseudonym}) in {pseudonym} expired")
        );
        assert_eq!(
            mask_identifiers("upstream returned 500", &ids, &pseudonym),
            "upstream returned 500"
        );
        assert!(account_identifiers(&json!({ "source": "oauth" })).is_empty());
    }

    #[test]
    fn usage_rows_are_the_array_under_result() {
        let reply = json!({ "result": [{ "provider": "claude" }] });
        assert_eq!(usage_rows(&reply).map(Vec::len), Some(1));
        assert_eq!(usage_rows(&json!({ "result": {} })), None);
        assert_eq!(usage_rows(&json!([{ "provider": "claude" }])), None);
        assert_eq!(usage_rows(&json!({ "error": { "code": "x" } })), None);
    }
}
