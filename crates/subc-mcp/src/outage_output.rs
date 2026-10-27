//! The text block every CortexKit runner shows the model when a tool call is
//! refused because its provider module is restarting or down.
//!
//! This is a copy of `cortexkit-role-llm-runner` 0.7.9's
//! `serializer_profile::error_output::render` and its member rules
//! (CONTRACT.md §4.4 and §11.4 of that crate). The gateway copies them
//! instead of depending on the crate because the crate pulls in packages the
//! gateway does not otherwise build (`cortexkit-rich-ui-syntax`,
//! `pulldown-cmark`, `serde_jcs`, `ryu-js`). The crate is a dev-dependency,
//! and `copy_renders_like_the_crate` below checks this copy against it for
//! every sample in the crate's `valid-error-outputs.json`, so a change to the
//! shared block fails the gateway's tests instead of drifting silently.
//!
//! The block is the message, then one line per present member in a fixed
//! order: `module: <module>`, `code: <code>`, `detail: <detail>`,
//! `nothing was sent`, `retry: <retry>`, joined by `\n` with no trailing
//! newline. Each member is one non-empty line, so no member can forge
//! another member's line.

/// The longest `module`, in bytes.
pub(crate) const MODULE_MAX: usize = 255;

/// The longest `code`, in bytes.
pub(crate) const CODE_MAX: usize = 64;

/// The longest `detail`, in bytes of UTF-8: 1 KiB.
pub(crate) const DETAIL_MAX: usize = 1024;

/// The longest `retry`, in bytes of UTF-8.
pub(crate) const RETRY_MAX: usize = 256;

/// The rendered line standing for `nothing_sent: true`.
pub(crate) const NOTHING_SENT_LINE: &str = "nothing was sent";

/// A tool error reporting a module outage. Build it with the `with_*`
/// methods, which keep a member only when it passes that member's rule, so
/// every value of this type renders a well-formed block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutageOutput {
    message: String,
    module: Option<String>,
    code: Option<String>,
    detail: Option<String>,
    nothing_sent: bool,
    retry: Option<String>,
}

impl OutageOutput {
    /// An error carrying only its message.
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            module: None,
            code: None,
            detail: None,
            nothing_sent: false,
            retry: None,
        }
    }

    /// Sets `module`, or leaves it out when `module` breaks its rule.
    pub(crate) fn with_module(mut self, module: &str) -> Self {
        self.module = is_valid_module(module).then(|| module.to_owned());
        self
    }

    /// Sets `code`, or leaves it out when `code` breaks its rule.
    pub(crate) fn with_code(mut self, code: &str) -> Self {
        self.code = is_valid_code(code).then(|| code.to_owned());
        self
    }

    /// Sets `detail`, or leaves it out when `detail` breaks its rule. Never
    /// truncated: a cut could split a multi-byte character or leave half a
    /// JSON object, so an over-long detail is dropped whole.
    pub(crate) fn with_detail(mut self, detail: &str) -> Self {
        self.detail = is_valid_line(detail, DETAIL_MAX).then(|| detail.to_owned());
        self
    }

    /// Marks the call as never having reached the provider. Only for a
    /// refusal at route open, or `module_reloading` answered before the send;
    /// never for a timeout or a lost reply, where the call may have run.
    pub(crate) fn with_nothing_sent(mut self) -> Self {
        self.nothing_sent = true;
        self
    }

    /// Sets `retry`, or leaves it out when `retry` breaks its rule.
    pub(crate) fn with_retry(mut self, retry: &str) -> Self {
        self.retry = is_valid_line(retry, RETRY_MAX).then(|| retry.to_owned());
        self
    }

    /// The text block the model sees.
    pub(crate) fn render(&self) -> String {
        render(
            &self.message,
            self.module.as_deref(),
            self.code.as_deref(),
            self.detail.as_deref(),
            self.nothing_sent,
            self.retry.as_deref(),
        )
    }
}

/// The shared rendering, member by member. Callers pass only members that
/// pass their rules; [`OutageOutput`] guarantees that.
pub(crate) fn render(
    message: &str,
    module: Option<&str>,
    code: Option<&str>,
    detail: Option<&str>,
    nothing_sent: bool,
    retry: Option<&str>,
) -> String {
    let mut lines = vec![message.to_owned()];
    if let Some(module) = module {
        lines.push(format!("module: {module}"));
    }
    if let Some(code) = code {
        lines.push(format!("code: {code}"));
    }
    if let Some(detail) = detail {
        lines.push(format!("detail: {detail}"));
    }
    if nothing_sent {
        lines.push(NOTHING_SENT_LINE.to_owned());
    }
    if let Some(retry) = retry {
        lines.push(format!("retry: {retry}"));
    }
    lines.join("\n")
}

/// Whether `module` is a valid `module` member: 1 to [`MODULE_MAX`] bytes of
/// UTF-8 with no control character.
pub(crate) fn is_valid_module(module: &str) -> bool {
    (1..=MODULE_MAX).contains(&module.len()) && !module.chars().any(char::is_control)
}

/// Whether `code` is a valid `code` member: it matches
/// `^[a-z][a-z0-9_]{0,63}$`.
pub(crate) fn is_valid_code(code: &str) -> bool {
    let mut bytes = code.bytes();
    bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && code.len() <= CODE_MAX
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Whether `value` is a valid `detail` or `retry` member: non-empty, without
/// `\n` or `\r`, and at most `max` bytes.
fn is_valid_line(value: &str, max: usize) -> bool {
    !value.is_empty() && !value.contains(['\n', '\r']) && value.len() <= max
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_role_llm_runner::serializer_profile::error_output::{
        self as shared, ErrorOutput,
    };
    use serde_json::Value;

    /// The crate's own samples, read from the package cargo unpacked. The
    /// crate does not export the file, so its location comes from
    /// `cargo metadata`, which names the manifest of every package in the
    /// build, wherever cargo keeps it (registry cache or vendor folder).
    fn valid_samples() -> Vec<Value> {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let output = std::process::Command::new(cargo)
            .args([
                "metadata",
                "--format-version",
                "1",
                "--offline",
                "--manifest-path",
            ])
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .output()
            .expect("cargo metadata runs");
        assert!(
            output.status.success(),
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
        let manifests: Vec<&str> = metadata["packages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|package| package["name"] == "cortexkit-role-llm-runner")
            .map(|package| package["manifest_path"].as_str().unwrap())
            .collect();
        assert_eq!(manifests.len(), 1, "one llm-runner crate: {manifests:?}");
        let path = std::path::Path::new(manifests[0])
            .parent()
            .unwrap()
            .join("test-vectors/owned-broca-tool-outage/valid-error-outputs.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let file: Value = serde_json::from_str(&text).unwrap();
        file["valid"].as_array().unwrap().clone()
    }

    fn copy_of(output: &ErrorOutput) -> OutageOutput {
        let mut copy = OutageOutput::new(output.message.clone());
        if let Some(module) = &output.module {
            copy = copy.with_module(module);
        }
        if let Some(code) = &output.code {
            copy = copy.with_code(code);
        }
        if let Some(detail) = &output.detail {
            copy = copy.with_detail(detail);
        }
        if output.nothing_sent {
            copy = copy.with_nothing_sent();
        }
        if let Some(retry) = &output.retry {
            copy = copy.with_retry(retry);
        }
        copy
    }

    #[test]
    fn copy_renders_like_the_crate() {
        let samples = valid_samples();
        assert!(samples.len() >= 7, "expected the crate's 7 or more samples");
        for sample in samples {
            let name = sample["name"].as_str().unwrap();
            let decoded = ErrorOutput::decode(&sample["output"])
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let copy = copy_of(&decoded);
            // Every member of a valid sample passes the copied rules, so the
            // copy kept them all, and renders the same bytes as the crate.
            assert_eq!(copy.render(), shared::render(&decoded), "{name}");
            assert_eq!(Value::from(copy.render()), sample["rendered"], "{name}");
        }
    }

    #[test]
    fn copied_limits_match_the_crate() {
        assert_eq!(MODULE_MAX, shared::MODULE_MAX);
        assert_eq!(CODE_MAX, shared::CODE_MAX);
        assert_eq!(DETAIL_MAX, shared::DETAIL_MAX);
        assert_eq!(RETRY_MAX, shared::RETRY_MAX);
        assert_eq!(NOTHING_SENT_LINE, shared::NOTHING_SENT_LINE);
        for module in [
            "aft",
            "fed:peer-1:aft",
            "a b",
            "a\tb",
            "a\nb",
            "",
            "aft\u{85}",
        ] {
            assert_eq!(
                is_valid_module(module),
                shared::is_valid_module(module),
                "{module:?}"
            );
        }
        for code in ["a", "module_reloading", "Upper", "_a", "0a", "a-b", ""] {
            assert_eq!(is_valid_code(code), shared::is_valid_code(code), "{code:?}");
        }
    }

    /// A member breaking its rule is left out rather than truncated, and what
    /// remains still passes the crate's own producer check.
    #[test]
    fn members_breaking_a_rule_are_left_out() {
        let output = OutageOutput::new("the module is unavailable")
            .with_module("a\nmodule")
            .with_code("Not_Snake")
            .with_detail(&"é".repeat(DETAIL_MAX))
            .with_retry("line one\nnothing was sent");
        assert_eq!(output.render(), "the module is unavailable");

        let output = OutageOutput::new("the module is unavailable")
            .with_module("aft")
            .with_code("module_reloading")
            .with_detail(&"x".repeat(DETAIL_MAX))
            .with_nothing_sent()
            .with_retry(&"r".repeat(RETRY_MAX));
        let shared_output = ErrorOutput::new("the module is unavailable")
            .with_module("aft")
            .with_code("module_reloading")
            .with_detail("x".repeat(DETAIL_MAX))
            .with_nothing_sent()
            .with_retry("r".repeat(RETRY_MAX));
        assert_eq!(shared_output.check(), Ok(()));
        assert_eq!(output.render(), shared_output.render());
    }
}
