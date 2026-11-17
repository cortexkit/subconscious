//! The hub instance's `server.conf`, at `<cortexkit data dir>/nats-hub/server.conf`.
//!
//! - The hub operator (`operator.jwt` beside the file) and its system account, with a
//!   `full` (directory) resolver under the same directory that preloads the system
//!   account and the user's hub account. Later account updates (revocations) are pushed
//!   through the system account and kept by the resolver.
//! - JetStream with domain `hub`, its store under the same directory. A box's own
//!   server has domain `{box}`; the two must differ for an inbox to source across.
//! - `leafnodes { no_advertise: true }`. The leaf port and its TLS are not here: the
//!   engine builds the listener from the `hub_listener` state ck-bus sends, because the
//!   certificate's key lives in the vault.
//! - One client listener, on IPv4 loopback only, for ck-bus's own admin connection.
//!   Its port is fixed at [`HUB_ADMIN_PORT`] (see there).

use std::path::Path;

/// The hub's leaf port, which faces the network.
pub const HUB_LEAF_PORT: u16 = 7422;

/// The hub instance's client listener, which only ck-bus uses. It cannot meet the local
/// server's listeners: `ck setup` installs those on 14222 (client) and 18222 (monitor),
/// and hub setup refuses to write the file when the local server's client URL names
/// this port anyway. It is also below every platform's ephemeral range (Linux starts at
/// 32768, macOS and Windows at 49152), so no outgoing connection can be holding it.
pub const HUB_ADMIN_PORT: u16 = 17422;

/// The only host the admin listener is ever written with.
pub const HUB_ADMIN_HOST: &str = "127.0.0.1";

/// The JetStream domain of every hub, self-hosted or hosted.
pub const HUB_DOMAIN: &str = "hub";

const HEADER: &str = "# Written by ck-bus hub setup; ck-bus rewrites it when a hub key rotates.\n";

/// What the file names. Paths are absolute.
pub struct HubConf<'a> {
    pub js_dir: &'a Path,
    pub operator_jwt: &'a Path,
    pub jwt_dir: &'a Path,
    pub system_account: &'a str,
    pub system_account_jwt: &'a str,
    pub hub_account: &'a str,
    pub hub_account_jwt: &'a str,
}

impl HubConf<'_> {
    pub fn render(&self) -> Result<String, String> {
        for key in [self.system_account, self.hub_account] {
            if !key.starts_with('A') || !key.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(format!("{key:?} is not an account public key"));
            }
        }
        Ok(format!(
            "{HEADER}\
             listen: {listen}\n\
             max_control_line: 65536\n\
             jetstream {{\n  store_dir: {js}\n  domain: {HUB_DOMAIN}\n}}\n\
             operator: {operator}\n\
             system_account: {system}\n\
             resolver {{\n  type: full\n  dir: {jwt}\n  allow_delete: false\n}}\n\
             resolver_preload {{\n  {system}: {system_jwt}\n  {hub}: {hub_jwt}\n}}\n\
             leafnodes {{\n  no_advertise: true\n}}\n",
            listen = quoted(&format!("{HUB_ADMIN_HOST}:{HUB_ADMIN_PORT}"))?,
            js = path_text(self.js_dir)?,
            operator = path_text(self.operator_jwt)?,
            system = self.system_account,
            jwt = path_text(self.jwt_dir)?,
            system_jwt = quoted(self.system_account_jwt)?,
            hub = self.hub_account,
            hub_jwt = quoted(self.hub_account_jwt)?,
        ))
    }
}

/// The admin connection's URL.
pub fn admin_url() -> String {
    format!("nats://{HUB_ADMIN_HOST}:{HUB_ADMIN_PORT}")
}

/// The port of a `nats://host:port` URL, for the collision check against the local
/// server.
pub fn url_port(url: &str) -> Option<u16> {
    let authority = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = authority.split('/').next()?;
    authority.rsplit_once(':')?.1.parse().ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_has_one_loopback_listener_domain_hub_and_both_accounts_preloaded() {
        let conf = HubConf {
            js_dir: Path::new(r"C:\hub dir\js"),
            operator_jwt: Path::new("/hub/operator.jwt"),
            jwt_dir: Path::new("/hub/jwt"),
            system_account: "ASYS",
            system_account_jwt: "s.s.s",
            hub_account: "AHUB",
            hub_account_jwt: "h.h.h",
        }
        .render()
        .unwrap();
        assert!(conf.starts_with(HEADER));
        let listens: Vec<_> = conf.lines().filter(|l| l.starts_with("listen")).collect();
        assert_eq!(listens, ["listen: \"127.0.0.1:17422\""]);
        assert!(!conf.contains("http"), "{conf}");
        assert!(conf.contains("  domain: hub\n"), "{conf}");
        assert!(conf.contains("  type: full\n"), "{conf}");
        assert!(
            conf.contains("  ASYS: \"s.s.s\"\n  AHUB: \"h.h.h\"\n"),
            "{conf}"
        );
        assert!(
            conf.contains("leafnodes {\n  no_advertise: true\n}"),
            "{conf}"
        );
        assert!(conf.contains(r#"store_dir: "C:\\hub dir\\js""#), "{conf}");
        // The leaf port and TLS come from the engine's state, never from this file.
        assert!(!conf.contains("7422\n") && !conf.contains("tls"), "{conf}");
    }

    #[test]
    fn the_admin_port_differs_from_the_local_servers_defaults_and_reads_from_a_url() {
        assert_ne!(HUB_ADMIN_PORT, 14222);
        assert_ne!(HUB_ADMIN_PORT, 18222);
        assert_eq!(url_port("nats://127.0.0.1:14222"), Some(14222));
        assert_eq!(url_port(&admin_url()), Some(HUB_ADMIN_PORT));
    }
}
