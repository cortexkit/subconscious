//! `server.conf` for the local `nats-server`, as `install-apply` writes it
//! (`docs/designs/nats-install-trust-chain.md`, section 4, with SUBC's decision 6.10).
//!
//! - `listen` binds the IPv4 loopback address. Stock nats-server has a single client
//!   listener, so `::1` cannot be bound alongside it, and a wildcard address is never
//!   written: the listener has no TLS and relies on loopback plus the JWT nonce challenge.
//! - `http` binds the same IPv4 loopback host for the daemon's plain `/healthz` probe.
//!   NATS monitoring has no authentication: any local process can read health, server,
//!   connection, account and JetStream statistics under the existing local trust model.
//!   It must never be exposed on a wildcard or non-loopback interface.
//! - `max_control_line` is 64 KiB: a CONNECT carrying ck-bus's box grant is longer than
//!   the server's 4 KiB default, which the server refuses as "maximum control line
//!   exceeded".
//! - The full (directory) resolver runs with deletion disabled, and only the system
//!   account is preloaded; ck-bus creates the box account at its first boot.

use std::path::Path;

/// The only host the client and monitoring listeners are ever written with.
pub const LISTEN_HOST: &str = "127.0.0.1";

const HEADER: &str =
    "# Written by `ck-bus install-apply`; re-run it rather than editing this file.\n";

pub struct ServerConf<'a> {
    pub port: u16,
    pub monitor_port: u16,
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

pub fn check_ports(port: u16, monitor_port: u16) -> Result<(), String> {
    if port == 0 || monitor_port == 0 {
        return Err("client and monitor ports must be between 1 and 65535".to_string());
    }
    if port == monitor_port {
        return Err("monitor port must differ from the client port".to_string());
    }
    Ok(())
}

fn monitoring_line(port: u16, monitor_port: u16) -> Result<String, String> {
    check_ports(port, monitor_port)?;
    Ok(format!("http: \"{LISTEN_HOST}:{monitor_port}\"\n"))
}

pub fn health_url(monitor_port: u16) -> String {
    format!("http://{LISTEN_HOST}:{monitor_port}/healthz")
}

/// Reads only the listener grammar this renderer writes, never a wildcard, IPv6 host,
/// hostname, port zero or an arbitrary NATS expression.
fn loopback_port(line: &str, name: &str) -> Result<u16, String> {
    line.trim()
        .strip_prefix(&format!("{name}:"))
        .and_then(|value| value.trim().strip_prefix('"'))
        .and_then(|value| value.strip_suffix('"'))
        .and_then(|value| value.strip_prefix(&format!("{LISTEN_HOST}:")))
        .and_then(|port| port.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| format!("{name} must be a quoted {LISTEN_HOST}:<port> listener"))
}

/// What [`with_monitoring`] does when the file already has a monitoring listener on a
/// different port than the one asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExistingListener {
    /// Refuse: the caller asked for the default port and did not name one, so it
    /// cannot be sure it wants to move the operator's listener.
    Refuse,
    /// Replace it: the caller named the port explicitly.
    Replace,
    /// Keep the operator's port and report it. `ck setup` upgrades an existing install
    /// this way, so a port someone chose is never overwritten.
    Keep,
}

/// A configuration with monitoring, and the port its listener ends up on (the
/// requested one, or the existing one when [`ExistingListener::Keep`] kept it).
pub struct Monitoring {
    pub conf: String,
    pub port: u16,
}

/// Adds monitoring to a recorded install configuration without re-rendering any JWT,
/// path or other directive. An existing listener on another port is handled as
/// `existing` says; an identical listener is a no-op so a live server's config mtime
/// stays unchanged. A listener on any host other than IPv4 loopback is refused under
/// every policy.
pub fn with_monitoring(
    conf: &str,
    monitor_port: u16,
    existing: ExistingListener,
) -> Result<Monitoring, String> {
    if !conf.starts_with(HEADER) {
        return Err("not an install-apply rendered file: the header is absent".to_string());
    }
    let mut listen = None;
    let mut http = None;
    let mut offset = 0;
    for line in conf.split_inclusive('\n') {
        for (name, entry) in [("listen", &mut listen), ("http", &mut http)] {
            if line.trim_start().starts_with(&format!("{name}:")) {
                if entry.is_some() {
                    return Err(format!(
                        "multiple {name} lines are not an install-apply config"
                    ));
                }
                *entry = Some((offset, offset + line.len(), loopback_port(line, name)?));
            }
        }
        offset += line.len();
    }
    let (_, insert_at, listen_port) = listen.ok_or("the install-apply listen line is absent")?;
    if let (ExistingListener::Keep, Some((_, _, current))) = (existing, http) {
        if current != monitor_port {
            // The kept port must still not collide with the client listener.
            check_ports(listen_port, current)?;
            return Ok(Monitoring {
                conf: conf.to_string(),
                port: current,
            });
        }
    }
    let monitoring = monitoring_line(listen_port, monitor_port)?;
    let mut updated = conf.to_string();
    match http {
        Some((_, _, current)) if current == monitor_port => {}
        Some((start, end, _)) => {
            if existing == ExistingListener::Refuse {
                return Err("http already uses a different port; pass --monitor-port explicitly to replace it".to_string());
            }
            updated.replace_range(start..end, &monitoring);
        }
        None => updated.insert_str(insert_at, &monitoring),
    }
    Ok(Monitoring {
        conf: updated,
        port: monitor_port,
    })
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
            "{HEADER}\
             listen: {listen}\n\
             {monitoring}\
             max_control_line: 65536\n\
             jetstream {{\n  store_dir: {js}\n}}\n\
             operator: {operator}\n\
             system_account: {system}\n\
             resolver {{\n  type: full\n  dir: {jwt}\n  allow_delete: false\n}}\n\
             resolver_preload {{\n  {system}: {system_jwt}\n}}\n",
            listen = quoted(&self.listen())?,
            monitoring = monitoring_line(self.port, self.monitor_port)?,
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
            monitor_port: 19222,
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
    fn monitoring_is_ipv4_loopback_at_the_chosen_port() {
        let conf = rendered();
        let http: Vec<_> = conf
            .lines()
            .filter(|line| line.starts_with("http:"))
            .collect();
        assert_eq!(http, ["http: \"127.0.0.1:19222\""]);
    }

    #[test]
    fn rendering_refuses_equal_client_and_monitor_ports() {
        let conf = ServerConf {
            port: 14222,
            monitor_port: 14222,
            js_dir: Path::new("/nats/js"),
            operator_jwt: Path::new("/nats/operator.jwt"),
            jwt_dir: Path::new("/nats/jwt"),
            system_account: "ASYS",
            system_account_jwt: "aaa.bbb.ccc",
        };
        assert!(conf.render().unwrap_err().contains("must differ"));
    }

    #[test]
    fn the_preload_reads_back_and_windows_paths_are_escaped() {
        let conf = rendered();
        assert_eq!(preloaded_jwt(&conf, "ASYS").as_deref(), Some("aaa.bbb.ccc"));
        assert_eq!(preloaded_jwt(&conf, "AOTHER"), None);
        assert!(conf.contains(r#"store_dir: "C:\\nats dir\\js""#), "{conf}");
    }
}
