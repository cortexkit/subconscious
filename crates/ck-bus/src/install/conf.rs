//! `server.conf` for the local `nats-server`, as `install-apply` writes it
//! (`docs/designs/nats-install-trust-chain.md`, section 4, with SUBC's decision 6.10).
//!
//! - `listen` binds the IPv4 loopback address. Stock nats-server has a single client
//!   listener, so `::1` cannot be bound alongside it, and a wildcard address is never
//!   written: the listener has no TLS and relies on loopback plus the JWT nonce challenge.
//! - `max_control_line` is 64 KiB: a CONNECT carrying ck-bus's box grant is longer than
//!   the server's 4 KiB default, which the server refuses as "maximum control line
//!   exceeded".
//! - The full (directory) resolver runs with deletion disabled, and only the system
//!   account is preloaded; ck-bus creates the box account at its first boot.

use std::path::Path;

/// The only host `listen` is ever written with.
pub const LISTEN_HOST: &str = "127.0.0.1";

pub struct ServerConf<'a> {
    pub port: u16,
    pub js_dir: &'a Path,
    pub operator_jwt: &'a Path,
    pub jwt_dir: &'a Path,
    pub system_account: &'a str,
    pub system_account_jwt: &'a str,
}

/// A double-quoted string in the nats-server config grammar, where `\` starts an escape
/// (so a Windows path needs its backslashes doubled).
fn quoted(value: &str) -> Result<String, String> {
    if value.chars().any(char::is_control) {
        return Err(format!("{value:?} contains a control character"));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn path_text(path: &Path) -> Result<String, String> {
    quoted(
        path.to_str()
            .ok_or_else(|| format!("{} is not valid UTF-8", path.display()))?,
    )
}

impl ServerConf<'_> {
    pub fn listen(&self) -> String {
        format!("{LISTEN_HOST}:{}", self.port)
    }

    /// The client URL ck-bus connects to.
    pub fn url(&self) -> String {
        format!("nats://{}", self.listen())
    }

    pub fn render(&self) -> Result<String, String> {
        Ok(format!(
            "# Written by `ck-bus install-apply`; re-run it rather than editing this file.\n\
             listen: {listen}\n\
             max_control_line: 65536\n\
             jetstream {{\n  store_dir: {js}\n}}\n\
             operator: {operator}\n\
             system_account: {system}\n\
             resolver {{\n  type: full\n  dir: {jwt}\n  allow_delete: false\n}}\n\
             resolver_preload {{\n  {system}: {system_jwt}\n}}\n",
            listen = quoted(&self.listen())?,
            js = path_text(self.js_dir)?,
            operator = path_text(self.operator_jwt)?,
            system = self.system_account,
            jwt = path_text(self.jwt_dir)?,
            system_jwt = quoted(self.system_account_jwt)?,
        ))
    }
}

/// The system account JWT preloaded for `system_account` in a `server.conf` this module
/// rendered, if there is one.
pub fn preloaded_jwt(conf: &str, system_account: &str) -> Option<String> {
    let block = conf.split_once("resolver_preload {")?.1.split_once('}')?.0;
    block.lines().find_map(|line| {
        let (key, value) = line.trim().split_once(':')?;
        (key.trim() == system_account).then(|| value.trim().trim_matches('"').to_string())
    })
}

/// The value of the top-level `listen` entry in a rendered `server.conf`.
#[cfg(test)]
pub fn listen_value(conf: &str) -> Option<String> {
    conf.lines().find_map(|line| {
        line.strip_prefix("listen:")
            .map(|value| value.trim().trim_matches('"').to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::{listen_value, preloaded_jwt, ServerConf};
    use crate::bootstrap::config::check_loopback_url;
    use std::path::Path;

    fn rendered() -> String {
        ServerConf {
            port: 14222,
            js_dir: Path::new(r"C:\nats dir\js"),
            operator_jwt: Path::new("/nats/operator.jwt"),
            jwt_dir: Path::new("/nats/jwt"),
            system_account: "ASYS",
            system_account_jwt: "aaa.bbb.ccc",
        }
        .render()
        .unwrap()
    }

    #[test]
    fn listen_is_ipv4_loopback_and_passes_the_loopback_check() {
        let conf = rendered();
        let listen = listen_value(&conf).unwrap();
        assert_eq!(listen, "127.0.0.1:14222");
        assert_eq!(conf.matches("listen").count(), 1, "{conf}");
        check_loopback_url(&format!("nats://{listen}")).expect("the written listener is loopback");
        // The same check refuses the address a wildcard listener would advertise, so the
        // assertion above can fail.
        let wildcard = conf.replace("127.0.0.1", "0.0.0.0");
        let listen = listen_value(&wildcard).unwrap();
        assert!(check_loopback_url(&format!("nats://{listen}")).is_err());
    }

    #[test]
    fn the_preload_reads_back_and_windows_paths_are_escaped() {
        let conf = rendered();
        assert_eq!(preloaded_jwt(&conf, "ASYS").as_deref(), Some("aaa.bbb.ccc"));
        assert_eq!(preloaded_jwt(&conf, "AOTHER"), None);
        assert!(conf.contains(r#"store_dir: "C:\\nats dir\\js""#), "{conf}");
    }
}
