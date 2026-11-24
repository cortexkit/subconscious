use serde::Deserialize;

use super::{
    host::{Child, Host, Io},
    Context,
};

#[derive(Clone, Copy, Debug)]
pub(super) enum Login {
    Claude,
    AnthropicKey,
    ChatGpt,
    OpenAiKey,
    Copilot,
}

impl Login {
    pub fn menu() -> &'static str {
        "  1. Claude subscription\n  2. Anthropic API key\n  3. ChatGPT sign-in\n  4. OpenAI API key\n  5. Copilot"
    }

    pub fn choose(number: usize) -> Option<Self> {
        Some(match number {
            1 => Self::Claude,
            2 => Self::AnthropicKey,
            3 => Self::ChatGpt,
            4 => Self::OpenAiKey,
            5 => Self::Copilot,
            _ => return None,
        })
    }

    pub fn provider(self) -> &'static str {
        match self {
            Self::Claude | Self::AnthropicKey => "anthropic",
            Self::ChatGpt => "openai-codex",
            Self::OpenAiKey => "openai",
            Self::Copilot => "github-copilot",
        }
    }

    fn argv(self) -> Vec<&'static str> {
        let mut args = vec!["auth", "login", self.provider()];
        if matches!(self, Self::AnthropicKey | Self::OpenAiKey) {
            args.push("--api-key");
        }
        args
    }
}

const PROVIDERS: [Login; 4] = [
    Login::Claude,
    Login::ChatGpt,
    Login::OpenAiKey,
    Login::Copilot,
];

#[derive(Deserialize)]
struct CheckOutput<'a> {
    status: &'a str,
    provider: &'a str,
}

fn check(
    host: &impl Host,
    child: impl FnOnce(&[&str]) -> Result<Child, String>,
    provider: &str,
) -> Result<bool, String> {
    let output = host
        .run(child(&["auth", "check", "--provider", provider, "--json"])?)
        .map_err(|error| format!("Pi auth check for {provider}: {error}"))?;
    let status = match output.code {
        Some(0) => "ready",
        Some(1) => "not_ready",
        Some(2) => "invalid",
        _ => {
            return Err(format!(
                "Pi auth check for {provider}: {}",
                output.description()
            ))
        }
    };
    if !serde_json::from_slice::<CheckOutput<'_>>(&output.stdout)
        .is_ok_and(|result| result.status == status && result.provider == provider)
    {
        return Err(format!(
            "Pi auth check for {provider} returned an invalid or mismatched result"
        ));
    }
    match status {
        "ready" => Ok(true),
        "not_ready" => Ok(false),
        _ => Err(format!(
            "Pi credentials for {provider} are invalid (exit code 2)"
        )),
    }
}

pub(super) fn sign_in(
    context: &Context<'_, impl Host>,
    child: impl Fn(&[&str]) -> Result<Child, String>,
) -> Result<&'static str, String> {
    context.check_auth()?;
    for login in PROVIDERS {
        let provider = login.provider();
        if check(context.host, &child, provider)? {
            context.check_auth()?;
            return Ok(provider);
        }
    }

    let login = context.host.login()?.ok_or("Pi sign-in cancelled")?;
    let provider = login.provider();
    let mut command = child(&login.argv())?;
    command.io = Io::Terminal;
    let output = context
        .host
        .run(command)
        .map_err(|error| format!("Pi login for {provider}: {error}"))?;
    context.check_auth()?;
    if output.code != Some(0) {
        return Err(format!("Pi login for {provider}: {}", output.description()));
    }
    if !check(context.host, &child, provider)? {
        return Err(format!(
            "Pi credentials for {provider} are not ready after sign-in"
        ));
    }
    context.check_auth()?;
    Ok(provider)
}
