use super::super::{
    model::Component,
    runtime::{CommandResult, CommandRunner},
};
use serde::Deserialize;
use std::{path::Path, time::Duration};

const LEASE_RETRIES: usize = 10;
const LEASE_DELAY: Duration = Duration::from_secs(2);

type Grant = (&'static str, &'static str, &'static str);

#[derive(Deserialize)]
pub(in crate::setup) struct Credential {
    pub id: String,
    pub state: String,
}

pub(in crate::setup) enum ListError {
    Runner(String),
    Exit(CommandResult),
    InvalidJson(String),
    UnsupportedVersion(u64, String),
}

impl ListError {
    pub(in crate::setup) fn reason(self) -> String {
        match self {
            Self::Runner(error) => error,
            Self::Exit(result) => result.explanation(),
            Self::InvalidJson(stderr) if stderr.trim().is_empty() => {
                "invalid JSON or record fields".into()
            }
            Self::UnsupportedVersion(v, stderr) if stderr.trim().is_empty() => {
                format!("unsupported v={v}")
            }
            Self::InvalidJson(stderr) | Self::UnsupportedVersion(_, stderr) => stderr.trim().into(),
        }
    }

    fn original_message(self) -> String {
        match self {
            Self::Runner(error) => error,
            Self::Exit(result) => format!(
                "ck auth list --json failed{}: {}",
                exit_description(&result),
                result.explanation()
            ),
            Self::InvalidJson(_) => {
                "refusal: ck auth list --json returned invalid JSON or record fields".into()
            }
            Self::UnsupportedVersion(v, _) => {
                format!("refusal: ck auth list --json returned unsupported v={v}")
            }
        }
    }
}

pub(super) fn component_grants(component: Component) -> &'static [Grant] {
    match component {
        Component::Astrocyte => &[("reserved:astrocyte", "llm-provider", "list")],
        Component::Striatum => &[
            ("reserved:striatum", "llm-provider", "list"),
            ("reserved:striatum", "llm-gateway", "list"),
            ("reserved:striatum", "media-provider", "list"),
        ],
        Component::Broca => &[
            ("reserved:broca", "llm-provider", "read"),
            ("reserved:broca", "llm-gateway", "read"),
            ("reserved:broca", "llm-gateway", "list"),
        ],
        Component::Cerebellum => &[("reserved:cerebellum", "browser-session", "deposit")],
        Component::Plexus => &[
            ("reserved:plexus", "github-app-native", "read"),
            ("reserved:plexus", "web-search", "read"),
        ],
        _ => &[],
    }
}

pub(in crate::setup) struct Auth<'a> {
    pub program: &'a Path,
    pub runner: &'a mut dyn CommandRunner,
    pub wait: fn(Duration),
}

impl Auth<'_> {
    pub fn list(&mut self) -> Result<Vec<Credential>, String> {
        self.read_list().map_err(ListError::original_message)
    }

    pub(in crate::setup) fn read_list(&mut self) -> Result<Vec<Credential>, ListError> {
        #[derive(Deserialize)]
        struct Listing {
            v: u64,
            credentials: Vec<Credential>,
        }
        let result = self
            .retry_lease(&["list", "--json"])
            .map_err(ListError::Runner)?;
        if result.exit_code != Some(0) {
            return Err(ListError::Exit(result));
        }
        let listing: Listing = match serde_json::from_str(&result.stdout) {
            Ok(listing) => listing,
            Err(_) => return Err(ListError::InvalidJson(result.stderr)),
        };
        if listing.v != 1 {
            return Err(ListError::UnsupportedVersion(listing.v, result.stderr));
        }
        Ok(listing.credentials)
    }

    fn run(&mut self, args: &[String]) -> Result<CommandResult, String> {
        self.runner.run(&self.program.to_string_lossy(), args)
    }

    pub fn retry_lease(&mut self, args: &[&str]) -> Result<CommandResult, String> {
        self.run_with_retries(args).map(|(result, _)| result)
    }

    fn run_with_retries(&mut self, args: &[&str]) -> Result<(CommandResult, bool), String> {
        let args = args.iter().map(|&arg| arg.to_owned()).collect::<Vec<_>>();
        for retries in 0..LEASE_RETRIES {
            let result = self.run(&args)?;
            if result.exit_code != Some(3) {
                return Ok((result, retries > 0));
            }
            (self.wait)(LEASE_DELAY);
        }
        Ok((self.run(&args)?, true))
    }

    pub fn grant(
        &mut self,
        principal: &str,
        kind: &str,
        selector: &str,
        op: &str,
    ) -> Result<bool, String> {
        let args = grant_args(principal, kind, selector, op);
        let description = || format!("ck auth grant ({principal}, {kind}, {selector}, {op})");
        let (first, retried) = self
            .run_with_retries(&args)
            .map_err(|error| format!("{}: {error}", description()))?;
        let uncertain = first.exit_code == Some(5);
        let result = if uncertain {
            self.run(&args.map(str::to_owned))
                .map_err(|error| format!("{} after exit 5: {error}", description()))?
        } else {
            first
        };
        if result.exit_code != Some(0) {
            return Err(format!(
                "{} failed{}",
                description(),
                exit_description(&result)
            ));
        }
        // A lost reply could have added the grant even when the second call
        // reports that it was already present.
        Ok(retried || uncertain || result.stdout.trim() != "already granted (no change)")
    }

    pub fn ensure_component(&mut self, component: Component) -> Result<bool, String> {
        let mut changed = false;
        for &(principal, selector, op) in component_grants(component) {
            changed |= self.grant(principal, "category", selector, op)?;
        }
        Ok(changed)
    }
}

pub(super) fn grant_args<'a>(
    principal: &'a str,
    kind: &'a str,
    selector: &'a str,
    op: &'a str,
) -> [&'a str; 9] {
    [
        "grant",
        "--principal",
        principal,
        "--selector-kind",
        kind,
        "--selector",
        selector,
        "--operation",
        op,
    ]
}

pub(super) fn exit_description(result: &CommandResult) -> String {
    match result.exit_code {
        Some(code) => format!(" with exit {code}"),
        None => " without an exit code".into(),
    }
}

pub(super) fn report(changed: bool) -> &'static str {
    if changed {
        "ensured"
    } else {
        "no change"
    }
}
