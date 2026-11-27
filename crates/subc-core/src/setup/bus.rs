//! Set up message-bus trust and activate the managed daemon entries.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    apply::vault::{Auth, Credential},
    bus_monitoring::{CKBUS_MODULE, NATS_SERVER_MODULE},
    components::OWNED_BINARY_KINDS,
    config,
    inventory::Inventory,
    runtime::CommandRunner,
};

mod activation;
mod keys;
mod seeds;
#[cfg(all(test, feature = "test-support"))]
mod tests_activation;
#[cfg(all(test, feature = "test-support"))]
mod tests_preflight;

#[cfg(feature = "test-support")]
pub use seeds::seed_detector_test_vectors;

pub(super) const BUS_TRUST: &str = "bus-trust";
#[allow(
    dead_code,
    reason = "the caller reports a hand-made bus without activating it"
)]
pub(super) const HAND_MADE: &str = "message bus installed outside setup; left as is";
const MALFORMED_ROW: &str = "setup's message-bus record is malformed; nothing was changed";
const UNEXPECTED_PLAN: &str = "ck-bus install-plan returned an unexpected plan; nothing was signed";
const TRUST_FILES: [&str; 3] = ["server.conf", "operator.jwt", "system_account"];
const PORTS: [u16; 2] = [14222, 18222];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BusStatus {
    pub live: bool,
    pub healthy: bool,
}

#[derive(Debug, Default)]
pub(super) struct BusRescan {
    pub added: Vec<String>,
    pub enabled_changes: Vec<String>,
}

pub(super) trait BusControl {
    /// Whether this loopback port can be bound.
    fn port_is_free(&mut self, port: u16) -> bool;
    /// `None` is a not-supervised answer, not a failed status call.
    fn status(&mut self, module: &str) -> Result<Option<BusStatus>, String>;
    /// PID of the live process, or `None` when there is no live process.
    fn provenance(&mut self, module: &str) -> Result<Option<u32>, String>;
    fn rescan(&mut self) -> Result<BusRescan, String>;
    fn restart(&mut self, module: &str, drain: Duration) -> Result<(), String>;
    fn now(&mut self) -> Instant;
    fn sleep(&mut self, duration: Duration);
}

#[derive(Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Pending,
    Applied,
}

#[derive(Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(super) struct BusTrust {
    state: State,
    root_pub: String,
    signer_pub: String,
    sysaccount_pub: String,
    pub activation_pending: bool,
}

impl BusTrust {
    fn pending() -> Self {
        Self {
            state: State::Pending,
            root_pub: String::new(),
            signer_pub: String::new(),
            sysaccount_pub: String::new(),
            activation_pending: false,
        }
    }

    fn read(inventory: &Inventory, nats_dir: &Path) -> Result<Option<Self>, String> {
        let row = inventory.entry_for_path(BUS_TRUST, nats_dir);
        if let Some(other) = inventory
            .paths_for_kind(BUS_TRUST)
            .into_iter()
            .find(|path| {
                row.and_then(|row| row["path"].as_str()).map(Path::new) != Some(path.as_path())
            })
        {
            return Err(format!(
                "setup's message-bus record names {}, not {}; nothing was changed",
                other.display(),
                nats_dir.display()
            ));
        }
        row.map(|row| {
            let trust: Self = serde_json::from_value(row.clone()).map_err(|_| MALFORMED_ROW)?;
            let halves = [&trust.root_pub, &trust.signer_pub, &trust.sysaccount_pub];
            let valid = match trust.state {
                State::Pending => halves.iter().all(|half| half.is_empty()),
                State::Applied => halves.iter().all(|half| valid_half(half)),
            };
            valid.then_some(trust).ok_or_else(|| MALFORMED_ROW.into())
        })
        .transpose()
    }

    pub(super) fn persist(&self, inventory: &mut Inventory, nats_dir: &Path) -> Result<(), String> {
        let fields = serde_json::to_value(self).map_err(|error| error.to_string())?;
        let Value::Object(fields) = fields else {
            unreachable!("BusTrust serializes to an object");
        };
        inventory.persist_retained(BUS_TRUST, nats_dir, fields)
    }
}

fn valid_half(half: &str) -> bool {
    half.len() == 64
        && half
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) struct BusPaths<'a> {
    pub nats_dir: &'a Path,
    pub config_path: &'a Path,
    pub ck_bus: &'a Path,
    pub ck_auth: &'a Path,
}

#[derive(Debug)]
pub(super) struct BusPrepared {
    pub changed: bool,
    pub prior: bool,
    pub trust_changed: bool,
    pub env: BTreeMap<String, String>,
    pub record: BusTrust,
}

/// `None` leaves a hand-made bus alone: setup and upgrade report `HAND_MADE`
/// and count no change. Setup counts `Some(true)` toward `Done.` and lets
/// `Some(false)` keep an otherwise unchanged run at `Nothing changed.`.
/// Upgrade counts `Some(true)` in its changed report; `Some(false)` adds no change.
#[allow(dead_code, reason = "shared entry point for setup and upgrade")]
pub(super) fn configure(
    paths: &BusPaths<'_>,
    ck_nats_server: &Path,
    connection: &Path,
    inventory: &mut Inventory,
    runner: &mut dyn CommandRunner,
    control: &mut dyn BusControl,
    wait_for_lease: fn(Duration),
) -> Result<Option<bool>, String> {
    let Some(prepared) = prepare(paths, inventory, runner, control, wait_for_lease)? else {
        return Ok(None);
    };
    activation::activate(
        paths,
        ck_nats_server,
        connection,
        inventory,
        control,
        prepared,
    )
    .map(Some)
}

fn owned_binary(inventory: &Inventory, path: &Path) -> bool {
    OWNED_BINARY_KINDS
        .iter()
        .any(|kind| inventory.owns_path(kind, path))
}

fn path_exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("could not inspect {}: {error}", path.display())),
    }
}

fn hand_made(paths: &BusPaths<'_>, listed: &[Credential]) -> Result<bool, String> {
    if listed
        .iter()
        .any(|key| key.id == "signing:ck-bus-operator-root:1")
    {
        return Ok(true);
    }
    for file in TRUST_FILES {
        if path_exists(&paths.nats_dir.join(file))? {
            return Ok(true);
        }
    }
    Ok(
        config::read_registration_config(paths.config_path)?.is_some_and(|(_, config)| {
            let modules = &config["modules"];
            modules.get(CKBUS_MODULE).is_some() || modules.get(NATS_SERVER_MODULE).is_some()
        }),
    )
}

struct Preflight {
    record: Option<BusTrust>,
    listed: Vec<Credential>,
}

fn preflight(
    paths: &BusPaths<'_>,
    inventory: &Inventory,
    auth: &mut Auth<'_>,
    control: &mut dyn BusControl,
) -> Result<Option<Preflight>, String> {
    let record = BusTrust::read(inventory, paths.nats_dir)?;
    if !owned_binary(inventory, paths.ck_auth) {
        return Err(format!(
            "{} is a ck-auth that setup did not install; it was not run, and nothing was changed",
            paths.ck_auth.display()
        ));
    }
    let listed = auth.read_list().map_err(|error| {
        format!(
            "ck auth list failed: {}; nothing was changed",
            error.reason()
        )
    })?;
    if record.is_none() && hand_made(paths, &listed)? {
        return Ok(None);
    }

    let ck_bus = paths.ck_bus;
    let present = path_exists(ck_bus)?;
    if present && !owned_binary(inventory, ck_bus) {
        return Err(format!(
            "{} is a ck-bus that setup did not install; it was not run, and nothing was changed",
            ck_bus.display()
        ));
    }
    if !present || !ck_bus.is_file() {
        return Err(format!("{} is missing; rerun ck setup", ck_bus.display()));
    }
    keys::check_active(&listed)?;
    if record.is_none() {
        for port in PORTS {
            if !control.port_is_free(port) {
                return Err(format!("port {port} is in use; free it and rerun ck setup"));
            }
        }
    }
    Ok(Some(Preflight { record, listed }))
}

/// Prepare and persist trust before changing the daemon configuration.
fn prepare(
    paths: &BusPaths<'_>,
    inventory: &mut Inventory,
    runner: &mut dyn CommandRunner,
    control: &mut dyn BusControl,
    wait_for_lease: fn(Duration),
) -> Result<Option<BusPrepared>, String> {
    if inventory.transaction_is_open() {
        return Err("cannot prepare the message bus during an open install transaction".into());
    }
    let mut auth = Auth {
        program: paths.ck_auth,
        runner,
        wait: wait_for_lease,
    };
    let Some(Preflight {
        record: previous,
        listed,
    }) = preflight(paths, inventory, &mut auth, control)?
    else {
        return Ok(None);
    };
    let nats_dir = paths.nats_dir;
    let absolute = std::path::absolute(nats_dir)
        .map_err(|error| format!("could not resolve {}: {error}", nats_dir.display()))?;
    if previous.is_none() {
        BusTrust::pending().persist(inventory, nats_dir)?;
    }
    let keys = keys::ensure(&mut auth, &listed)?;
    let mut bus = BusTool {
        runner,
        program: paths.ck_bus,
        nats_dir: &absolute,
    };
    let prior = previous
        .as_ref()
        .is_some_and(|record| record.activation_pending);

    if let Some(mut record) = previous.filter(|record| {
        record.state == State::Applied
            && record.signer_pub == keys.signer_pub
            && record.sysaccount_pub == keys.sysaccount_pub
    }) {
        let before = trust_snapshot(nats_dir)?;
        match bus.plan(&record.root_pub, &keys)? {
            Plan::NoCeremony => {
                // The tool may re-render files before activation can run.
                record.activation_pending = true;
                record.persist(inventory, nats_dir)?;
                let env = bus.apply(&record.root_pub, &[])?;
                let after = trust_snapshot(nats_dir)?;
                return Ok(Some(BusPrepared {
                    changed: keys.changed || before[..2] != after[..2],
                    prior,
                    trust_changed: before != after,
                    env,
                    record,
                }));
            }
            Plan::Ceremony(payloads) => {
                check_payloads(&payloads)?;
            }
        }
    }

    BusTrust::pending().persist(inventory, nats_dir)?;
    let root = generate_root();
    let root_pub = hex(&root.verifying_key().to_bytes());
    let Plan::Ceremony(payloads) = bus.plan(&root_pub, &keys)? else {
        return Err(UNEXPECTED_PLAN.into());
    };
    let inputs = read_payloads(&absolute, &payloads)?;
    let signed = inputs.map(|(kind, path, bytes)| (kind, path, sign_ceremony_input(&root, &bytes)));
    drop(root);
    let env = bus.apply(&root_pub, &signed)?;
    seeds::scan_written_trust(&absolute)?;
    let record = BusTrust {
        state: State::Applied,
        root_pub,
        signer_pub: keys.signer_pub,
        sysaccount_pub: keys.sysaccount_pub,
        activation_pending: true,
    };
    record.persist(inventory, nats_dir)?;
    Ok(Some(BusPrepared {
        changed: true,
        prior,
        trust_changed: true,
        env,
        record,
    }))
}

fn trust_snapshot(nats_dir: &Path) -> Result<Vec<Option<Vec<u8>>>, String> {
    TRUST_FILES
        .iter()
        .map(|name| {
            let path = nats_dir.join(name);
            match fs::read(&path) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(format!("could not read {}: {error}", path.display())),
            }
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PayloadKind {
    Operator,
    SystemAccount,
}

impl PayloadKind {
    fn from_value(value: &Value) -> Option<Self> {
        match value["payload"].as_str()? {
            "operator" => Some(Self::Operator),
            "system_account" => Some(Self::SystemAccount),
            _ => None,
        }
    }

    fn file(self, nats_dir: &Path) -> PathBuf {
        nats_dir.join("ceremony").join(match self {
            Self::Operator => "operator.input",
            Self::SystemAccount => "system_account.input",
        })
    }

    fn apply_flags(self) -> (&'static str, &'static str) {
        match self {
            Self::Operator => ("--operator-input", "--operator-sig"),
            Self::SystemAccount => ("--sysaccount-input", "--sysaccount-sig"),
        }
    }
}

enum Plan {
    NoCeremony,
    Ceremony(Value),
}

struct BusTool<'a> {
    runner: &'a mut dyn CommandRunner,
    program: &'a Path,
    nats_dir: &'a Path,
}

impl BusTool<'_> {
    fn run(&mut self, verb: &str, root: &str, extra: Vec<String>) -> Result<Value, String> {
        let mut args = vec![
            verb.to_owned(),
            "--nats-dir".into(),
            self.nats_dir.to_string_lossy().into_owned(),
            "--root-pub".into(),
            root.to_owned(),
        ];
        args.extend(extra);
        let fail = |reason: &str| format!("ck-bus {verb} failed: {reason}");
        let result = self
            .runner
            .run(&self.program.to_string_lossy(), &args)
            .map_err(|reason| fail(&reason))?;
        if result.exit_code != Some(0) {
            let reason = if result.stderr.trim().is_empty() {
                result
                    .exit_code
                    .map_or_else(|| "no exit code".into(), |code| format!("exit {code}"))
            } else {
                result.stderr.trim().to_owned()
            };
            return Err(fail(&reason));
        }
        serde_json::from_str(&result.stdout).map_err(|_| {
            fail(if result.stderr.trim().is_empty() {
                "invalid JSON"
            } else {
                result.stderr.trim()
            })
        })
    }

    fn plan(&mut self, root: &str, keys: &keys::BusKeys) -> Result<Plan, String> {
        let mut reply = self.run(
            "install-plan",
            root,
            vec![
                "--signer-pub".into(),
                keys.signer_pub.clone(),
                "--sysaccount-pub".into(),
                keys.sysaccount_pub.clone(),
            ],
        )?;
        match reply["status"].as_str() {
            Some("no ceremony needed") => Ok(Plan::NoCeremony),
            Some("ceremony needed") => Ok(Plan::Ceremony(reply["payloads"].take())),
            status => Err(format!(
                "ck-bus install-plan failed: {}",
                status.unwrap_or("missing status")
            )),
        }
    }

    fn apply(
        &mut self,
        root: &str,
        signed: &[(PayloadKind, PathBuf, String)],
    ) -> Result<BTreeMap<String, String>, String> {
        let mut extra = Vec::new();
        for (kind, path, signature) in signed {
            let (input_flag, sig_flag) = kind.apply_flags();
            extra.extend([
                input_flag.into(),
                path.to_string_lossy().into_owned(),
                sig_flag.into(),
                signature.clone(),
            ]);
        }
        let mut reply = self.run("install-apply", root, extra)?;
        let status = reply["status"].as_str().unwrap_or("missing status");
        if status != "applied" {
            return Err(format!("ck-bus install-apply failed: {status}"));
        }
        let env: BTreeMap<String, String> = serde_json::from_value(reply["env"].take())
            .map_err(|_| "ck-bus install-apply failed: invalid env")?;
        for name in [
            "CKBUS_NATS_URL",
            "CKBUS_OPERATOR_JWT",
            "CKBUS_SYSTEM_ACCOUNT",
        ] {
            if env.get(name).is_none_or(String::is_empty) {
                return Err(format!(
                    "ck-bus install-apply failed: missing or empty {name}"
                ));
            }
        }
        if Path::new(&env["CKBUS_OPERATOR_JWT"]) != self.nats_dir.join("operator.jwt") {
            return Err(
                "ck-bus install-apply failed: CKBUS_OPERATOR_JWT names the wrong path".into(),
            );
        }
        Ok(env)
    }
}

/// Checking the recorded root doesn't sign its payloads. Only the payload
/// identities decide whether to ask for a new plan with a new root.
fn check_payloads(payloads: &Value) -> Result<Vec<(PayloadKind, &Value)>, String> {
    let mut seen = Vec::new();
    for payload in payloads.as_array().ok_or(UNEXPECTED_PLAN)? {
        let kind = PayloadKind::from_value(payload).ok_or(UNEXPECTED_PLAN)?;
        if seen.iter().any(|(previous, _)| *previous == kind) {
            return Err(UNEXPECTED_PLAN.into());
        }
        seen.push((kind, payload));
    }
    if seen.is_empty() {
        return Err(UNEXPECTED_PLAN.into());
    }
    Ok(seen)
}

type SigningInput = (PayloadKind, PathBuf, Vec<u8>);

fn read_payloads(nats_dir: &Path, payloads: &Value) -> Result<[SigningInput; 2], String> {
    let [first, second] = check_payloads(payloads)?
        .try_into()
        .map_err(|_| UNEXPECTED_PLAN)?;
    if !fs::symlink_metadata(nats_dir.join("ceremony")).is_ok_and(|meta| meta.is_dir()) {
        return Err(UNEXPECTED_PLAN.into());
    }
    let read = |(kind, payload): (PayloadKind, &Value)| -> Result<SigningInput, String> {
        let path = kind.file(nats_dir);
        if !path
            .to_str()
            .is_some_and(|path| payload["path"].as_str() == Some(path))
            || !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file())
        {
            return Err(UNEXPECTED_PLAN.into());
        }
        let bytes = fs::read(&path).map_err(|_| UNEXPECTED_PLAN)?;
        if payload["sha256"].as_str() != Some(&hex(&Sha256::digest(&bytes))) {
            return Err(UNEXPECTED_PLAN.into());
        }
        Ok((kind, path, bytes))
    };
    // Neither payload may be signed until both buffers and digests pass.
    Ok([read(first)?, read(second)?])
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
    }
    text
}

fn generate_root() -> SigningKey {
    #[cfg(all(test, feature = "test-support"))]
    if tests_preflight::USE_FIXED_ROOT.get() {
        return SigningKey::from_bytes(&tests_preflight::FIXED_ROOT);
    }
    SigningKey::generate(&mut rand_core::OsRng)
}

#[cfg(feature = "test-support")]
thread_local! {
    static SIGNATURES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Return the unpadded base64url Ed25519 signature of the exact input bytes.
pub fn sign_ceremony_input(key: &SigningKey, input: &[u8]) -> String {
    #[cfg(feature = "test-support")]
    SIGNATURES.set(SIGNATURES.get() + 1);
    URL_SAFE_NO_PAD.encode(key.sign(input).to_bytes())
}

/// Drain this thread's signature count, so parallel tests do not share it.
#[cfg(feature = "test-support")]
pub fn take_ceremony_signature_count() -> usize {
    SIGNATURES.replace(0)
}
