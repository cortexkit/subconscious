use std::fmt;
use std::time::SystemTime;

use chrono::{DateTime, SecondsFormat, Utc};
use tracing::Level;

pub(crate) fn render_line(
    at: SystemTime,
    level: &Level,
    module_id: &str,
    session: Option<&str>,
    tag: Option<&str>,
    message: &str,
    fields: &[(String, String)],
) -> String {
    let timestamp = DateTime::<Utc>::from(at).to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut line = format!("{timestamp} {:<5} {module_id}", level.as_str());

    if let Some(session) = session {
        line.push_str(" session=");
        line.push_str(session);
    }
    if let Some(tag) = tag {
        line.push_str(" tag=");
        line.push_str(tag);
    }
    if !message.is_empty() {
        line.push(' ');
        line.push_str(&escape_message(message));
    }
    for (key, value) in fields {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(&format_value(value));
    }

    line
}

// Backslash is escaped first so a literal `\n` in the input survives as
// `\\n` and cannot be read back as a newline. Same order as @cortexkit/log.
fn escape_message(message: &str) -> String {
    message
        .replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

fn format_value(value: &str) -> String {
    if value.is_empty()
        || value
            .chars()
            .any(|character| matches!(character, ' ' | '"' | '\n' | '\r'))
    {
        let escaped = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\r', "\\r")
            .replace('\n', "\\n");
        format!("\"{escaped}\"")
    } else {
        // An unquoted value is verbatim, backslashes included: only the quoted
        // form has an escape grammar, and a reader decodes only quoted values.
        value.to_owned()
    }
}

pub(crate) fn strip_ansi(input: &str) -> String {
    let without_c1 = strip_c1_sequences(input);
    let input = without_c1.as_str();
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    let mut plain_start = 0;

    while index < bytes.len() {
        if bytes[index] != 0x1b {
            index += 1;
            continue;
        }

        output.push_str(&input[plain_start..index]);
        index += 1;
        if index >= bytes.len() {
            plain_start = index;
            break;
        }

        match bytes[index] {
            b'[' => {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            b']' => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if bytes[index] == 0x1b
                        && bytes.get(index + 1).is_some_and(|next| *next == b'\\')
                    {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            _ => index += 1,
        }
        plain_start = index;
    }

    if plain_start == 0 {
        return without_c1;
    }
    output.push_str(&input[plain_start..]);
    output
}

fn strip_c1_sequences(input: &str) -> String {
    let mut characters = input.chars();
    let mut output = String::with_capacity(input.len());
    while let Some(character) = characters.next() {
        match character {
            '\u{009b}' => {
                for parameter in characters.by_ref() {
                    if ('@'..='~').contains(&parameter) {
                        break;
                    }
                }
            }
            '\u{009d}' => {
                for payload in characters.by_ref() {
                    if matches!(payload, '\u{0007}' | '\u{009c}') {
                        break;
                    }
                }
            }
            '\u{0080}'..='\u{009f}' => {}
            _ => output.push(character),
        }
    }
    output
}

/// A level parsed from a fleet log line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParsedLevel {
    /// A trace event.
    Trace,
    /// A debug event.
    Debug,
    /// An informational event.
    Info,
    /// A warning event.
    Warn,
    /// An error event.
    Error,
}

/// The fields needed to merge and filter a fleet log line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedLine<'a> {
    /// Event time decoded from the UTC timestamp.
    pub timestamp: SystemTime,
    /// Event severity.
    pub level: ParsedLevel,
    /// Module that owns the log file.
    pub module_id: &'a str,
    /// Session lineage, including its issuer, when one was present.
    pub session: Option<&'a str>,
    /// Declared target tag, when one was present.
    pub tag: Option<&'a str>,
    /// The message and ordered fields after the fixed columns.
    pub body: &'a str,
}

/// A stable parse failure returned by [`parse_line`](crate::parse_line).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseError {
    reason: &'static str,
}

impl ParseError {
    pub(crate) const fn new(reason: &'static str) -> Self {
        Self { reason }
    }

    /// Returns the stable reason used by conformance fixtures and CLI diagnostics.
    pub const fn reason(self) -> &'static str {
        self.reason
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.reason)
    }
}

impl std::error::Error for ParseError {}

pub(crate) fn parse(line: &str) -> Result<ParsedLine<'_>, ParseError> {
    if line.contains('\u{1b}') || line.contains('\u{9b}') {
        return Err(ParseError::new("ansi_forbidden"));
    }
    if line.contains(['\n', '\r']) {
        return Err(ParseError::new("line_break"));
    }

    let (timestamp_text, after_timestamp) = line
        .split_once(' ')
        .ok_or_else(|| ParseError::new("timestamp_missing"))?;
    if !timestamp_text.ends_with('Z') {
        return Err(ParseError::new("timestamp_not_utc_z"));
    }
    if timestamp_text.len() != 24 {
        return Err(ParseError::new("timestamp_precision"));
    }
    let timestamp = DateTime::parse_from_rfc3339(timestamp_text)
        .map_err(|_| ParseError::new("timestamp_invalid"))?;

    let (level, after_level) = parse_level(after_timestamp)?;
    let (module_id, mut body) = after_level
        .split_once(' ')
        .ok_or_else(|| ParseError::new("message_missing"))?;
    if module_id.is_empty() {
        return Err(ParseError::new("module_missing"));
    }

    let mut session = None;
    if let Some(rest) = body.strip_prefix("session=") {
        let (value, remainder) = rest
            .split_once(' ')
            .ok_or_else(|| ParseError::new("message_missing"))?;
        let valid = value
            .rsplit_once(':')
            .is_some_and(|(issuer, id)| !issuer.is_empty() && !id.is_empty());
        // `session=global` and other issuer-less placeholders fail here on
        // their missing `issuer:` half; the renderer never has to know any
        // particular sentinel.
        if !valid {
            return Err(ParseError::new("session_missing_issuer"));
        }
        session = Some(value);
        body = remainder;
    }

    let mut tag = None;
    if let Some(rest) = body.strip_prefix("tag=") {
        let (value, remainder) = rest
            .split_once(' ')
            .ok_or_else(|| ParseError::new("message_missing"))?;
        if value.is_empty() {
            return Err(ParseError::new("tag_missing"));
        }
        tag = Some(value);
        body = remainder;
    }
    if body.is_empty() {
        return Err(ParseError::new("message_missing"));
    }

    Ok(ParsedLine {
        timestamp: SystemTime::from(timestamp),
        level,
        module_id,
        session,
        tag,
        body,
    })
}

fn parse_level(input: &str) -> Result<(ParsedLevel, &str), ParseError> {
    for (prefix, level) in [
        ("TRACE ", ParsedLevel::Trace),
        ("DEBUG ", ParsedLevel::Debug),
        ("INFO  ", ParsedLevel::Info),
        ("WARN  ", ParsedLevel::Warn),
        ("ERROR ", ParsedLevel::Error),
    ] {
        if let Some(rest) = input.strip_prefix(prefix) {
            return Ok((level, rest));
        }
    }

    if ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"]
        .iter()
        .any(|level| input.starts_with(level))
    {
        Err(ParseError::new("level_column_width"))
    } else {
        Err(ParseError::new("level_invalid"))
    }
}
