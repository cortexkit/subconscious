//! ck-bus's broker inputs, read from its supervised environment (the `env` block of the
//! `ckbus` module declaration), never from a hard-coded path:
//!
//! - `CKBUS_NATS_URL`: the local server's client URL, `nats://<loopback host>:<port>`.
//!   The listener has no TLS and every client authenticates by JWT nonce challenge, so a
//!   host that is not a loopback address is refused.
//! - `CKBUS_OPERATOR_JWT`: the path of the root-signed operator JWT that `ck setup`
//!   wrote. ck-bus reads its `signing_keys` (the operator signer must be one of them) and
//!   its `system_account`.
//! - `CKBUS_SYSTEM_ACCOUNT`: the system account id (`A...`), which must equal the
//!   operator JWT's `system_account`.

use std::{env, fs, net::IpAddr, path::PathBuf};

use serde_json::Value;

use super::account_jwt::verify_self_named_issuer;

pub const NATS_URL_ENV: &str = "CKBUS_NATS_URL";
pub const OPERATOR_JWT_ENV: &str = "CKBUS_OPERATOR_JWT";
pub const SYSTEM_ACCOUNT_ENV: &str = "CKBUS_SYSTEM_ACCOUNT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerConfig {
    pub url: String,
    pub operator_jwt_path: PathBuf,
    pub system_account: String,
}

/// The facts ck-bus takes from the operator JWT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorFacts {
    /// The operator identity (`O...`), the root. ck-bus never signs with it.
    pub operator_public: String,
    /// The operator signing keys (`O...`); the operator signer must be listed.
    pub signing_keys: Vec<String>,
}

impl BrokerConfig {
    /// Reads the three variables. The error names every missing or invalid one.
    pub fn from_env() -> Result<Self, String> {
        let read = |name: &str| env::var(name).ok().filter(|value| !value.trim().is_empty());
        let (url, operator, system) = (
            read(NATS_URL_ENV),
            read(OPERATOR_JWT_ENV),
            read(SYSTEM_ACCOUNT_ENV),
        );
        let missing: Vec<&str> = [
            (NATS_URL_ENV, url.is_none()),
            (OPERATOR_JWT_ENV, operator.is_none()),
            (SYSTEM_ACCOUNT_ENV, system.is_none()),
        ]
        .into_iter()
        .filter_map(|(name, absent)| absent.then_some(name))
        .collect();
        let (Some(url), Some(operator), Some(system_account)) = (url, operator, system) else {
            return Err(format!("unset: {}", missing.join(", ")));
        };
        check_loopback_url(&url)?;
        if !system_account.starts_with('A')
            || nkeys::KeyPair::from_public_key(&system_account).is_err()
        {
            return Err(format!(
                "{SYSTEM_ACCOUNT_ENV} {system_account} is not an account public key"
            ));
        }
        Ok(Self {
            url,
            operator_jwt_path: PathBuf::from(operator),
            system_account,
        })
    }

    /// Reads the operator JWT, checks its self-signature and that it names the same
    /// system account as the environment.
    pub fn operator_facts(&self) -> Result<OperatorFacts, String> {
        let path = &self.operator_jwt_path;
        let text = fs::read_to_string(path)
            .map_err(|error| format!("operator JWT {}: {error}", path.display()))?;
        let claims = verify_self_named_issuer(&text)
            .map_err(|error| format!("operator JWT {}: {error}", path.display()))?;
        let operator_public = claims["sub"].as_str().unwrap_or_default().to_string();
        if claims["iss"].as_str() != Some(operator_public.as_str())
            || !operator_public.starts_with('O')
        {
            return Err(format!(
                "operator JWT {} is not self-signed by an operator key",
                path.display()
            ));
        }
        let named = claims["nats"]["system_account"]
            .as_str()
            .unwrap_or_default();
        if named != self.system_account {
            return Err(format!(
                "operator JWT {} names system account {named:?}, {SYSTEM_ACCOUNT_ENV} is {}",
                path.display(),
                self.system_account
            ));
        }
        let signing_keys = claims["nats"]["signing_keys"]
            .as_array()
            .map(|keys| {
                keys.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Ok(OperatorFacts {
            operator_public,
            signing_keys,
        })
    }
}

/// Refuses a URL whose host is not a loopback address.
pub fn check_loopback_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("nats://")
        .ok_or_else(|| format!("{NATS_URL_ENV} {url} is not a nats:// URL"))?;
    let authority = rest.split('/').next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host),
    };
    let loopback = host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if loopback {
        Ok(())
    } else {
        Err(format!(
            "{NATS_URL_ENV} host {host:?} is not a loopback address; the local listener has \
             no TLS"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::check_loopback_url;

    #[test]
    fn only_loopback_hosts_are_accepted() {
        for url in [
            "nats://127.0.0.1:4222",
            "nats://[::1]:4222",
            "nats://localhost:4222",
        ] {
            check_loopback_url(url).unwrap_or_else(|error| panic!("{url}: {error}"));
        }
        for url in [
            "nats://0.0.0.0:4222",
            "nats://10.0.0.5:4222",
            "nats://example.com:4222",
            "tls://127.0.0.1:4222",
        ] {
            assert!(check_loopback_url(url).is_err(), "{url} must be refused");
        }
    }
}
