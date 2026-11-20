use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use subc_jsonc::{read_document, set_member};

use super::{
    file_modes, inventory::Inventory, retained_binary::RetainedBinary, runtime::CommandRunner,
};

const ENTRY: &[&str] = &["modules", "nats-server"];

pub(super) struct Migration {
    config: PathBuf,
    connection: PathBuf,
    program: PathBuf,
    before: Value,
    after: Value,
    text: Option<(String, String)>,
    legacy: RetainedBinary,
    enabled: bool,
}

impl Migration {
    pub(super) fn needs_rewrite(&self) -> bool {
        self.before != self.after
    }
    pub(super) fn capture(
        config: &Path,
        home: &Path,
        connection: &Path,
        inventory: &Inventory,
    ) -> Result<Option<Self>, String> {
        if !config.exists() {
            return Ok(None);
        }
        let (_, document) = read_document(config)?;
        let Some(entry) = document.get("modules").and_then(|m| m.get("nats-server")) else {
            return Ok(None);
        };
        let suffix = if cfg!(windows) { ".exe" } else { "" };
        let program = home.join(format!("ck-nats-server{suffix}"));
        let declared = entry
            .get("program")
            .and_then(Value::as_str)
            .unwrap_or("<missing program>");
        if Path::new(declared) == program {
            return Ok(Some(Self {
                config: config.into(),
                connection: connection.into(),
                program: program.clone(),
                before: entry.clone(),
                after: entry.clone(),
                text: None,
                legacy: RetainedBinary::capture(&program, &program, inventory),
                enabled: entry
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            }));
        }
        let stock = PathBuf::from(declared);
        if !stock.is_absolute()
            || stock
                .file_name()
                .is_none_or(|n| n != format!("nats-server{suffix}").as_str())
        {
            return Err(format!("refusal: modules.nats-server uses operator program {declared}; expected nats-server; nothing was changed"));
        }
        let args = entry
            .get("args")
            .and_then(Value::as_array)
            .ok_or("refusal: nats-server has no config arguments")?;
        if entry.get("protocol").and_then(Value::as_str) != Some("none")
            || args.len() != 2
            || args[0] != "-c"
        {
            return Err(format!(
                "refusal: {declared} is not the stock protocol:none -c <server.conf> entry"
            ));
        }
        let conf = args[1]
            .as_str()
            .filter(|p| {
                Path::new(p).is_absolute()
                    && Path::new(p).file_name().is_some_and(|n| n == "server.conf")
            })
            .ok_or("refusal: nats-server must name an absolute server.conf")?;
        let mut after = entry.clone();
        after["program"] = json!(program);
        after["args"] = json!(["-c", conf, "--subc", connection]);
        after["protocol"] = json!("subc");
        after["registration_channel"] = json!(true);
        after["reserved"] = json!(true);
        if let Some(health) = after.get_mut("health").and_then(Value::as_object_mut) {
            health.remove("http");
            if health.is_empty() {
                after.as_object_mut().unwrap().remove("health");
            }
        }
        Ok(Some(Self {
            config: config.into(),
            connection: connection.into(),
            program: program.clone(),
            before: entry.clone(),
            after,
            text: None,
            legacy: RetainedBinary::capture(&stock, &program, inventory),
            enabled: entry
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        }))
    }

    pub(super) fn apply(&mut self) -> Result<(), String> {
        self.apply_with(|path, bytes| {
            file_modes::write_atomic(path, bytes, 0o600, 0o600, "nats.tmp")
        })
    }

    pub(super) fn apply_recorded(&mut self, inventory: &mut Inventory) -> Result<(), String> {
        if let Some(transaction) = inventory.transaction() {
            self.apply_with(|path, bytes| transaction.write(path, bytes, 0o600))
        } else {
            self.apply()
        }
    }

    fn apply_with(
        &mut self,
        write: impl FnOnce(&Path, &[u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.text.is_some() {
            return Ok(());
        }
        for name in ["ck-nats-server", "ck-nats-engine"] {
            let file = self
                .program
                .with_file_name(format!("{name}{}", if cfg!(windows) { ".exe" } else { "" }));
            if !file.is_file() {
                return Err(format!(
                    "refusal: NATS companion {} is missing",
                    file.display()
                ));
            }
        }
        let (before, document) = read_document(&self.config)?;
        if document["modules"]["nats-server"] != self.before {
            return Err(
                "refusal: modules.nats-server changed during migration; run setup or upgrade again"
                    .into(),
            );
        }
        let mut after = before.clone();
        if self.before == self.after {
            self.text = Some((before, after));
            return Ok(());
        }
        set_member(&mut after, ENTRY, &self.after)?;
        write(&self.config, after.as_bytes())?;
        self.text = Some((before, after));
        Ok(())
    }

    pub(super) fn complete(&mut self, inventory: &mut Inventory) -> Result<(), String> {
        self.legacy.complete(inventory)?;
        #[cfg(test)]
        if !inventory.transaction_is_open() {
            inventory.save()?;
        }
        self.text = None;
        Ok(())
    }

    pub(super) fn verify<R: CommandRunner + ?Sized>(
        &self,
        ck: &Path,
        runner: &mut R,
        timeout: Duration,
    ) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let args = vec![
                "--subc".into(),
                self.connection.to_string_lossy().into_owned(),
                "--json".into(),
                "module".into(),
                "status".into(),
                "nats-server".into(),
            ];
            let status = runner
                .run(&ck.to_string_lossy(), &args)
                .ok()
                .filter(|r| r.success)
                .and_then(|r| serde_json::from_str::<Value>(&r.stdout).ok());
            if status.as_ref().is_some_and(|v| {
                v["module"]["live"] == true
                    && v["module"]["protocol"] == "subc"
                    && v["health"]["status"] == "ok"
            }) {
                let args = vec![
                    "--subc".into(),
                    self.connection.to_string_lossy().into_owned(),
                    "--json".into(),
                    "provenance".into(),
                    "nats-server".into(),
                ];
                let provenance = runner
                    .run(&ck.to_string_lossy(), &args)
                    .ok()
                    .filter(|r| r.success)
                    .and_then(|r| serde_json::from_str::<Value>(&r.stdout).ok());
                if provenance.as_ref().is_some_and(|v| {
                    let m = &v["modules"][0];
                    m["module_id"] == "nats-server"
                        && m["registration"] == "channel"
                        && m["registration_declaration"] == "declared"
                        && m["daemon_observed"]["spawned_from"]
                            .as_str()
                            .is_some_and(|p| Path::new(p) == self.program)
                        && m["daemon_observed"]["running_image"]["status"] == "match"
                }) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "nats-server did not register from {} and become healthy within {} seconds",
                    self.program.display(),
                    timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::runtime::CommandResult;
    use super::*;

    struct StatusRunner {
        status: Value,
        provenance: Value,
    }
    impl CommandRunner for StatusRunner {
        fn run(&mut self, _: &str, args: &[String]) -> Result<CommandResult, String> {
            let value = if args.iter().any(|a| a == "provenance") {
                &self.provenance
            } else {
                &self.status
            };
            Ok(CommandResult {
                success: true,
                stdout: value.to_string(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn nats_verification_requires_channel_registration_and_exact_program() {
        let root = cortexkit_test_support::ScratchDir::new("nats-verify");
        let home = root.join("bin");
        let config = root.join("subc.jsonc");
        std::fs::write(&config, json!({"modules":{"nats-server":{"program":home.join(format!("nats-server{}", if cfg!(windows) { ".exe" } else { "" })),"args":["-c", root.join("server.conf")],"protocol":"none"}}}).to_string()).unwrap();
        let inventory = Inventory::load(root.join("inventory.json"), "linux-x64").unwrap();
        let migration =
            Migration::capture(&config, &home, &root.join("connection.json"), &inventory)
                .unwrap()
                .unwrap();
        let mut runner = StatusRunner {
            status: json!({"module":{"live":true,"protocol":"subc"},"health":{"status":"ok"}}),
            provenance: json!({"modules":[{"module_id":"nats-server","registration":"channel","registration_declaration":"declared","daemon_observed":{"spawned_from":home.join(format!("ck-nats-server{}", if cfg!(windows) { ".exe" } else { "" })),"running_image":{"status":"match"}}}]}),
        };
        migration
            .verify(Path::new("ck"), &mut runner, Duration::ZERO)
            .unwrap();
        runner.provenance["modules"][0]["registration"] = json!("nonce");
        assert!(
            migration
                .verify(Path::new("ck"), &mut runner, Duration::ZERO)
                .is_err(),
            "legacy nonce registration must not pass"
        );
        runner.provenance["modules"][0]["registration"] = json!("channel");
        runner.provenance["modules"][0]["daemon_observed"]["spawned_from"] = json!(home.join(
            format!("nats-server{}", if cfg!(windows) { ".exe" } else { "" })
        ));
        assert!(
            migration
                .verify(Path::new("ck"), &mut runner, Duration::ZERO)
                .is_err(),
            "old program must not pass"
        );
    }
}
