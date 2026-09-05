use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

const REDACTED: &str = "[REDACTED]";

static BEARER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(Bearer)[ \t]+[A-Za-z0-9._~+/=-]+").expect("valid bearer regex")
});
static JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\beyJ[A-Za-z0-9_-]*\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b").expect("valid JWT regex")
});
static CORTEXKIT_HANDLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bckh_[A-Za-z0-9_-]+\b").expect("valid CortexKit handle regex"));
static OPENAI_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bsk-[A-Za-z0-9_-]+\b").expect("valid OpenAI key regex"));
static GITHUB_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bgh[po]_[A-Za-z0-9_-]+\b").expect("valid GitHub token regex"));

pub(crate) fn fleet_redact(line: &str) -> Cow<'_, str> {
    let authorization_redacted = redact_authorization(line);
    let bearer_redacted = BEARER.replace_all(&authorization_redacted, |captures: &Captures<'_>| {
        format!("{} {REDACTED}", &captures[1])
    });
    let jwt_redacted = JWT.replace_all(&bearer_redacted, REDACTED);
    let handle_redacted = CORTEXKIT_HANDLE.replace_all(&jwt_redacted, REDACTED);
    let openai_redacted = OPENAI_KEY.replace_all(&handle_redacted, REDACTED);
    let github_redacted = GITHUB_TOKEN.replace_all(&openai_redacted, REDACTED);

    if github_redacted == line {
        Cow::Borrowed(line)
    } else {
        Cow::Owned(github_redacted.into_owned())
    }
}

fn redact_authorization(input: &str) -> Cow<'_, str> {
    let lowercase = input.to_ascii_lowercase();
    let mut search_from = 0;
    let mut output = String::new();
    let mut copied_through = 0;

    while let Some(relative) = lowercase[search_from..].find("authorization:") {
        let marker = search_from + relative;
        let after_marker = marker + "authorization:".len();
        let mut value_start = after_marker;
        while input
            .as_bytes()
            .get(value_start)
            .is_some_and(u8::is_ascii_whitespace)
        {
            value_start += 1;
        }
        if value_start >= input.len() {
            break;
        }

        let value_end = authorization_value_end(input, value_start);
        let redaction_start = if input.as_bytes()[value_start] == b'"' {
            value_start + 1
        } else if input.as_bytes()[value_start] == b'\\'
            && input.as_bytes().get(value_start + 1) == Some(&b'"')
        {
            value_start + 2
        } else {
            value_start
        };
        output.push_str(&input[copied_through..redaction_start]);
        output.push_str(REDACTED);
        copied_through = value_end;
        search_from = value_end;
    }

    if copied_through == 0 {
        Cow::Borrowed(input)
    } else {
        output.push_str(&input[copied_through..]);
        Cow::Owned(output)
    }
}

fn authorization_value_end(input: &str, start: usize) -> usize {
    let bytes = input.as_bytes();
    if bytes[start] == b'"' {
        return quoted_end(bytes, start + 1, false);
    }
    if bytes[start] == b'\\' && bytes.get(start + 1) == Some(&b'"') {
        return quoted_end(bytes, start + 2, true);
    }

    let first_end = token_end(bytes, start);
    let scheme = &input[start..first_end];
    if !["basic", "bearer", "digest"].contains(&scheme.to_ascii_lowercase().as_str()) {
        return preserve_escaped_quote(bytes, start, first_end);
    }

    let mut credential_start = first_end;
    while bytes
        .get(credential_start)
        .is_some_and(u8::is_ascii_whitespace)
    {
        credential_start += 1;
    }
    let credential_end = token_end(bytes, credential_start);
    preserve_escaped_quote(bytes, credential_start, credential_end)
}

fn quoted_end(bytes: &[u8], mut index: usize, escaped_delimiter: bool) -> usize {
    while index < bytes.len() {
        if escaped_delimiter && bytes[index] == b'\\' && bytes.get(index + 1) == Some(&b'"') {
            return index;
        }
        if !escaped_delimiter && bytes[index] == b'"' {
            return index;
        }
        index += 1;
    }
    bytes.len()
}

fn token_end(bytes: &[u8], mut index: usize) -> usize {
    while bytes
        .get(index)
        .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        index += 1;
    }
    index
}

fn preserve_escaped_quote(bytes: &[u8], start: usize, end: usize) -> usize {
    if end >= start + 2 && bytes.get(end - 2..end) == Some(br#"\""#) {
        end - 2
    } else if end > start && bytes[end - 1] == b'"' {
        end - 1
    } else {
        end
    }
}
