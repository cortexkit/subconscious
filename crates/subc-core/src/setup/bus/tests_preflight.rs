use super::*;
use crate::setup::{install_journal::Journal, runtime::CommandResult};
use cortexkit_test_support::ScratchDir;
use ed25519_dalek::Signature;
use serde_json::json;
use std::{cell::RefCell, collections::VecDeque, ffi::OsString, rc::Rc};

pub(super) const FIXED_ROOT: [u8; 32] = [0x17; 32];
// Operator NKey seed for FIXED_ROOT, including the CRC-16/XMODEM checksum.
const FIXED_NKEY: &str = "SOABOFYXC4LROFYXC4LROFYXC4LROFYXC4LROFYXC4LROFYXC4LROF3SXQ";
const SIGNER_PUB: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SYSACCOUNT_PUB: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const PRIOR_ROOT: &str = "8888888888888888888888888888888888888888888888888888888888888888";
const OPERATOR_INPUT: &[u8] = b"header.operator-claims";
const SYSTEM_INPUT: &[u8] = b"header.system-account-claims";

thread_local! {
    pub(super) static USE_FIXED_ROOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn read_observed_file(path: &Path, manifest: &Path) -> Vec<u8> {
    if path == manifest.with_file_name("installer.lock") {
        // Inventory holds this lock during setup, so Windows blocks reads through
        // other handles. Check that it is empty; the scan still reads the separate
        // installer.lock.holder file that names the command holding the lock.
        let metadata = fs::symlink_metadata(path)
            .unwrap_or_else(|error| panic!("could not inspect {}: {error}", path.display()));
        assert!(metadata.is_file(), "not a regular file: {}", path.display());
        assert_eq!(
            metadata.len(),
            0,
            "installer lock file must be empty: {}",
            path.display()
        );
        return Vec::new();
    }
    fs::read(path).unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()))
}

#[test]
#[should_panic(expected = "installer lock file must be empty")]
fn scratch_scan_rejects_nonempty_installer_lock() {
    let root = ScratchDir::new("setup-bus-lock-scan");
    let manifest = root.join("installer-manifest.json");
    let lock = root.join("installer.lock");
    fs::write(&lock, b"unexpected contents").unwrap();
    let _inventory =
        Inventory::load_locked(manifest.clone(), "test", "setup", Duration::ZERO).unwrap();
    read_observed_file(&lock, &manifest);
}

#[derive(Debug, PartialEq, Eq)]
enum Call {
    Process(String, Vec<String>),
    Port(u16),
}

struct Observe {
    root: PathBuf,
    manifest: PathBuf,
    calls: Vec<Call>,
    child_env: Vec<Vec<(OsString, OsString)>>,
    before_process: Vec<Option<BusTrust>>,
    private: [Vec<u8>; 4],
}

impl Observe {
    fn inspect_bytes(&self, bytes: &[u8], location: &str) {
        for (label, form) in ["raw", "hex", "base64url", "NKey"]
            .into_iter()
            .zip(&self.private)
        {
            assert!(
                !bytes.windows(form.len()).any(|bytes| bytes == form),
                "{label} root leaked into {location}"
            );
        }
    }

    fn inspect_files(&self) {
        fn descend(observe: &Observe, root: &Path) {
            for entry in fs::read_dir(root).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    descend(observe, &path);
                } else {
                    observe.inspect_bytes(
                        &read_observed_file(&path, &observe.manifest),
                        &path.display().to_string(),
                    );
                }
            }
        }
        descend(self, &self.root);
    }

    fn process(&mut self, program: &str, args: &[String]) {
        self.inspect_files();
        self.inspect_bytes(program.as_bytes(), "program path");
        for arg in args {
            self.inspect_bytes(arg.as_bytes(), "runner argument");
        }
        // CommandRunner supplies no per-call env. Record the inherited child env.
        let env: Vec<_> = std::env::vars_os().collect();
        for (name, value) in &env {
            self.inspect_bytes(name.as_encoded_bytes(), "child environment name");
            self.inspect_bytes(value.as_encoded_bytes(), "child environment value");
        }
        self.child_env.push(env);
        self.calls
            .push(Call::Process(program.to_owned(), args.to_vec()));
        self.before_process.push(self.disk_record());
    }

    fn disk_record(&self) -> Option<BusTrust> {
        let disk = Inventory::load_snapshot(self.manifest.clone(), "test").unwrap();
        // Preflight reads the manifest even when a test intentionally planted a bad row.
        BusTrust::read(&disk, &self.root.join("data").join("nats"))
            .ok()
            .flatten()
    }
}

struct Control {
    observed: Rc<RefCell<Observe>>,
    busy: Option<u16>,
}

impl BusControl for Control {
    fn port_is_free(&mut self, port: u16) -> bool {
        let mut observed = self.observed.borrow_mut();
        observed.calls.push(Call::Port(port));
        observed.inspect_files();
        self.busy != Some(port)
    }
    fn status(&mut self, _: &str) -> Result<Option<BusStatus>, String> {
        panic!("trust must not read status")
    }
    fn provenance(&mut self, _: &str) -> Result<Option<u32>, String> {
        panic!("trust must not read provenance")
    }
    fn rescan(&mut self) -> Result<BusRescan, String> {
        panic!("trust must not rescan")
    }
    fn restart(&mut self, _: &str, _: Duration) -> Result<(), String> {
        panic!("trust must not restart")
    }
    fn now(&mut self) -> Instant {
        panic!("trust must not use activation clock")
    }
    fn sleep(&mut self, _: Duration) {
        panic!("trust must not wait for activation")
    }
}

fn reply(code: i32, stdout: impl Into<String>, stderr: &str) -> CommandResult {
    CommandResult {
        success: code == 0,
        exit_code: Some(code),
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

fn listing(keys: &[(&str, &str)]) -> CommandResult {
    reply(0, json!({"v": 1, "credentials": keys.iter().map(|(id, state)| json!({"id": id, "state": state})).collect::<Vec<_>>()}).to_string(), "")
}

fn public_key(id: &str) -> String {
    let half = match id {
        keys::SIGNER => SIGNER_PUB,
        keys::SYSACCOUNT => SYSACCOUNT_PUB,
        _ => panic!("the account needs no public-key read"),
    };
    format!("public_key_hex {half}\nkey_id 0123456789abcdef\n")
}

#[derive(Clone, Copy, Debug)]
enum BadPlan {
    Exit,
    Json,
    Status,
    Empty,
    Unknown,
    Duplicate,
    Missing,
    Outside,
    Digest,
    MissingFile,
    NoCeremony,
}

enum PlanReply {
    Full,
    CheckNeedsOne,
    NoCeremony,
    Bad(BadPlan),
}

#[derive(Clone, Copy, Debug)]
enum BadApply {
    Exit,
    Status,
    Missing,
    Empty,
    WrongPath,
}

struct Runner {
    observed: Rc<RefCell<Observe>>,
    auth: PathBuf,
    bus: PathBuf,
    lists: VecDeque<CommandResult>,
    mints: VecDeque<CommandResult>,
    public: VecDeque<CommandResult>,
    plans: VecDeque<PlanReply>,
    bad_apply: Option<BadApply>,
    re_render: bool,
    plant_seed_in: Option<PathBuf>,
}

fn flag<'a>(args: &'a [String], name: &str) -> &'a str {
    args.windows(2).find(|pair| pair[0] == name).unwrap()[1].as_str()
}

pub(super) fn bus_nats_dir(args: &[String]) -> PathBuf {
    // ck-bus resolves --nats-dir before joining output filenames.
    std::path::absolute(flag(args, "--nats-dir")).unwrap()
}

impl Runner {
    fn plan(&mut self, args: &[String]) -> CommandResult {
        assert_eq!(
            args.len(),
            9,
            "install-plan must use the default output directory"
        );
        assert_eq!(flag(args, "--signer-pub"), SIGNER_PUB);
        assert_eq!(flag(args, "--sysaccount-pub"), SYSACCOUNT_PUB);
        let nats = bus_nats_dir(args);
        let plan = self.plans.pop_front().unwrap_or(PlanReply::Full);
        let bad = match plan {
            PlanReply::Bad(bad) => Some(bad),
            _ => None,
        };
        if matches!(
            plan,
            PlanReply::NoCeremony | PlanReply::Bad(BadPlan::NoCeremony)
        ) {
            return reply(0, r#"{"status":"no ceremony needed"}"#, "");
        }
        match bad {
            Some(BadPlan::Exit) => return reply(1, "", "test plan refusal"),
            Some(BadPlan::Json) => return reply(0, "not JSON", ""),
            Some(BadPlan::Status) => return reply(0, r#"{"status":"unknown status"}"#, ""),
            _ => {}
        }
        if matches!(plan, PlanReply::CheckNeedsOne) {
            // The recorded-root check accepts a payload identity without a path or digest.
            return reply(
                0,
                r#"{"status":"ceremony needed","payloads":[{"payload":"system_account"}]}"#,
                "",
            );
        }
        let ceremony = nats.join("ceremony");
        fs::create_dir_all(&ceremony).unwrap();
        let path = |kind: &str| ceremony.join(format!("{kind}.input"));
        fs::write(nats.join("system_account"), "system account ID").unwrap();
        fs::write(path("operator"), OPERATOR_INPUT).unwrap();
        if !matches!(bad, Some(BadPlan::MissingFile)) {
            fs::write(path("system_account"), SYSTEM_INPUT).unwrap();
        }
        let mut payloads = vec![
            json!({"payload":"operator", "path":path("operator").display().to_string(), "sha256":hex(&Sha256::digest(OPERATOR_INPUT))}),
            json!({"payload":"system_account", "path":path("system_account").display().to_string(), "sha256":hex(&Sha256::digest(SYSTEM_INPUT))}),
        ];
        match bad {
            Some(BadPlan::Empty) => payloads.clear(),
            Some(BadPlan::Unknown) => payloads[1]["payload"] = json!("unknown"),
            Some(BadPlan::Duplicate) => payloads[1] = payloads[0].clone(),
            Some(BadPlan::Missing) => {
                payloads.pop();
            }
            Some(BadPlan::Outside) => payloads[1]["path"] = json!(nats.join("elsewhere.input")),
            Some(BadPlan::Digest) => payloads[1]["sha256"] = json!("0".repeat(64)),
            _ => {}
        }
        reply(
            0,
            json!({"status":"ceremony needed", "payloads":payloads}).to_string(),
            "",
        )
    }

    fn apply(&self, args: &[String]) -> CommandResult {
        let nats = bus_nats_dir(args);
        let root = flag(args, "--root-pub");
        if args.len() > 5 {
            assert_eq!(args.len(), 13, "install-apply must use its default ports");
            let key = SigningKey::from_bytes(&FIXED_ROOT).verifying_key();
            assert_eq!(root, hex(key.as_bytes()));
            for (kind, prefix, input) in [
                ("operator", "operator", OPERATOR_INPUT),
                ("system_account", "sysaccount", SYSTEM_INPUT),
            ] {
                let path = flag(args, &format!("--{prefix}-input"));
                assert_eq!(
                    Path::new(path),
                    nats.join("ceremony").join(format!("{kind}.input"))
                );
                assert_eq!(fs::read(path).unwrap(), input);
                let encoded = flag(args, &format!("--{prefix}-sig"));
                let decoded = URL_SAFE_NO_PAD.decode(encoded).unwrap();
                key.verify_strict(input, &Signature::from_slice(&decoded).unwrap())
                    .unwrap();
            }
            fs::write(nats.join("operator.jwt"), "operator public JWT").unwrap();
            fs::write(nats.join("server.conf"), "server config").unwrap();
        } else {
            assert_eq!(root, PRIOR_ROOT);
        }
        if self.re_render {
            fs::write(nats.join("server.conf"), "re-rendered config").unwrap();
        }
        if let Some(file) = &self.plant_seed_in {
            fs::write(nats.join(file), FIXED_NKEY.replace('X', "Y")).unwrap();
        }
        let mut doc = json!({"status": "applied", "env": {
            "CKBUS_NATS_URL":"nats://127.0.0.1:14222",
            "CKBUS_OPERATOR_JWT":nats.join("operator.jwt").display().to_string(),
            "CKBUS_SYSTEM_ACCOUNT":"system account ID"
        }});
        match self.bad_apply {
            Some(BadApply::Exit) => return reply(1, "", "test apply refusal"),
            Some(BadApply::Status) => doc["status"] = json!("rejected"),
            Some(BadApply::Missing) => {
                doc["env"].as_object_mut().unwrap().remove("CKBUS_NATS_URL");
            }
            Some(BadApply::Empty) => doc["env"]["CKBUS_SYSTEM_ACCOUNT"] = json!(""),
            Some(BadApply::WrongPath) => {
                doc["env"]["CKBUS_OPERATOR_JWT"] = json!(nats.join("other.jwt"))
            }
            None => {}
        }
        reply(0, doc.to_string(), "")
    }
}

impl CommandRunner for Runner {
    fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
        self.observed.borrow_mut().process(program, args);
        let result = if Path::new(program) == self.auth {
            match args[0].as_str() {
                "list" => {
                    assert_eq!(args, ["list", "--json"]);
                    self.lists.pop_front().unwrap_or_else(|| listing(&[]))
                }
                "mint-signing-key" => {
                    assert_eq!(args, ["mint-signing-key", "--id", args[2].as_str()]);
                    let id = &args[2];
                    self.mints.pop_front().unwrap_or_else(|| {
                        reply(
                            0,
                            format!(
                                "created {id}\n{}",
                                if id == keys::ACCOUNT {
                                    String::new()
                                } else {
                                    public_key(id)
                                }
                            ),
                            "",
                        )
                    })
                }
                "public-key" => {
                    assert_eq!(args, ["public-key", "--id", args[2].as_str()]);
                    self.public
                        .pop_front()
                        .unwrap_or_else(|| reply(0, public_key(&args[2]), ""))
                }
                "grant" => reply(0, "already granted (no change)", ""),
                _ => panic!("unexpected vault call {args:?}"),
            }
        } else {
            assert_eq!(Path::new(program), self.bus);
            match args[0].as_str() {
                "install-plan" => self.plan(args),
                "install-apply" => self.apply(args),
                _ => panic!("unexpected bus call {args:?}"),
            }
        };
        self.observed.borrow().inspect_files();
        Ok(result)
    }
}

struct Scenario {
    _root: ScratchDir,
    manifest: PathBuf,
    nats: PathBuf,
    config: PathBuf,
    inventory: Inventory,
    observed: Rc<RefCell<Observe>>,
    runner: Runner,
    control: Control,
}

impl Scenario {
    fn new() -> Self {
        let root = ScratchDir::new("setup-bus-preflight");
        let manifest = root.join("setup/installer-manifest.json");
        let auth = root.join("bin/ck-auth");
        let bus = root.join("bin/ck-bus");
        let nats = root.join("data").join("nats");
        let config = root.join("config/subc.jsonc");
        for dir in ["bin", "setup", "data", "config"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let mut inventory =
            Inventory::load_locked(manifest.clone(), "test", "setup", Duration::ZERO).unwrap();
        fs::write(&auth, "fixture vault").unwrap();
        fs::write(&bus, "fixture bus").unwrap();
        for program in [&auth, &bus] {
            inventory
                .persist_retained("managed-binary", program, Default::default())
                .unwrap();
        }
        let observed = Rc::new(RefCell::new(Observe {
            root: root.join(""),
            manifest: manifest.clone(),
            calls: vec![],
            child_env: vec![],
            before_process: vec![],
            private: [
                FIXED_ROOT.to_vec(),
                hex(&FIXED_ROOT).into_bytes(),
                URL_SAFE_NO_PAD.encode(FIXED_ROOT).into_bytes(),
                FIXED_NKEY.as_bytes().to_vec(),
            ],
        }));
        let runner = Runner {
            observed: observed.clone(),
            auth,
            bus,
            lists: VecDeque::new(),
            mints: VecDeque::new(),
            public: VecDeque::new(),
            plans: VecDeque::new(),
            bad_apply: None,
            re_render: false,
            plant_seed_in: None,
        };
        let control = Control {
            observed: observed.clone(),
            busy: None,
        };
        Self {
            _root: root,
            manifest,
            nats,
            config,
            inventory,
            observed,
            runner,
            control,
        }
    }

    fn run(&mut self) -> Result<Option<BusPrepared>, String> {
        take_ceremony_signature_count();
        let prior = USE_FIXED_ROOT.replace(true);
        let result = prepare(
            &BusPaths {
                nats_dir: &self.nats,
                config_path: &self.config,
                ck_bus: &self.runner.bus.clone(),
                ck_auth: &self.runner.auth.clone(),
            },
            &mut self.inventory,
            &mut self.runner,
            &mut self.control,
            |_| {},
        );
        USE_FIXED_ROOT.set(prior);
        self.observed.borrow().inspect_files();
        assert!(!Journal::path(&self.manifest).exists());
        result
    }

    fn put_row(&mut self, path: &Path, fields: Value) {
        self.inventory
            .persist_retained(BUS_TRUST, path, fields.as_object().unwrap().clone())
            .unwrap();
    }

    fn applied(&mut self, prior: bool) {
        let record = BusTrust {
            state: State::Applied,
            root_pub: PRIOR_ROOT.into(),
            signer_pub: SIGNER_PUB.into(),
            sysaccount_pub: SYSACCOUNT_PUB.into(),
            activation_pending: prior,
        };
        record.persist(&mut self.inventory, &self.nats).unwrap();
        self.runner.lists.push_back(listing(&[
            (keys::SIGNER, "active"),
            (keys::SYSACCOUNT, "active"),
            (keys::ACCOUNT, "active"),
        ]));
        fs::create_dir_all(&self.nats).unwrap();
        fs::write(self.nats.join("operator.jwt"), "old public JWT").unwrap();
        fs::write(self.nats.join("server.conf"), "old config").unwrap();
        fs::write(self.nats.join("system_account"), "system account ID").unwrap();
    }

    fn process_args(&self) -> Vec<Vec<String>> {
        self.observed
            .borrow()
            .calls
            .iter()
            .filter_map(|call| match call {
                Call::Process(_, args) => Some(args.clone()),
                Call::Port(_) => None,
            })
            .collect()
    }

    fn verbs(&self) -> Vec<String> {
        self.process_args()
            .into_iter()
            .map(|args| args[0].clone())
            .collect()
    }

    fn assert_pending(&self) {
        assert_eq!(
            self.observed.borrow().disk_record(),
            Some(BusTrust::pending())
        );
    }

    fn preflight_stop(&mut self, expected: &str, verbs: &[&str], ports: &[u16]) {
        let before = fs::read(&self.manifest).unwrap();
        assert_eq!(self.run().unwrap_err(), expected);
        assert_eq!(fs::read(&self.manifest).unwrap(), before);
        assert_eq!(self.verbs(), verbs);
        assert_eq!(self.ports(), ports);
        assert_eq!(take_ceremony_signature_count(), 0);
    }

    fn ports(&self) -> Vec<u16> {
        self.observed
            .borrow()
            .calls
            .iter()
            .filter_map(|call| match call {
                Call::Port(port) => Some(*port),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn fresh_install_writes_ahead_and_keeps_private_forms_in_memory() {
    let mut scenario = Scenario::new();
    scenario
        .runner
        .lists
        .push_back(listing(&[("signing:bus-leaf:1", "retired")]));
    let prepared = scenario.run().unwrap().unwrap();
    assert!(prepared.changed && !prepared.prior && prepared.trust_changed);
    assert_eq!(prepared.env["CKBUS_NATS_URL"], "nats://127.0.0.1:14222");
    assert_eq!(prepared.record.state, State::Applied);
    assert!(prepared.record.activation_pending);
    assert_eq!(
        prepared.record.root_pub,
        hex(SigningKey::from_bytes(&FIXED_ROOT)
            .verifying_key()
            .as_bytes())
    );
    assert_eq!(prepared.record.signer_pub, SIGNER_PUB);
    assert_eq!(prepared.record.sysaccount_pub, SYSACCOUNT_PUB);
    assert_eq!(take_ceremony_signature_count(), 2);
    let observed = scenario.observed.borrow();
    assert_eq!(observed.disk_record(), Some(prepared.record));
    assert_eq!(observed.child_env.len(), observed.before_process.len());
    assert_eq!(observed.before_process[0], None);
    assert!(observed.before_process[1..]
        .iter()
        .all(|row| *row == Some(BusTrust::pending())));
    assert_eq!(observed.calls[1..3], [Call::Port(14222), Call::Port(18222)]);
    assert_eq!(
        scenario.verbs(),
        [
            "list",
            "mint-signing-key",
            "mint-signing-key",
            "mint-signing-key",
            "grant",
            "grant",
            "grant",
            "grant",
            "grant",
            "grant",
            "install-plan",
            "install-apply"
        ]
    );
    let args = scenario.process_args();
    for (args, (id, op)) in args[4..10].iter().zip(
        [keys::SIGNER, keys::SYSACCOUNT, keys::ACCOUNT]
            .into_iter()
            .flat_map(|id| [(id, "sign"), (id, "read")]),
    ) {
        assert_eq!(
            args,
            &[
                "grant",
                "--principal",
                "reserved:ckbus",
                "--selector-kind",
                "exact",
                "--selector",
                id,
                "--operation",
                op
            ]
        );
    }
    assert!(!scenario.config.exists());
}

#[test]
fn other_path_and_malformed_rows_stop_before_list() {
    let mut scenario = Scenario::new();
    scenario.put_row(&scenario.nats.clone(), json!({"state":"invalid"}));
    let other = scenario.nats.with_file_name("old nats");
    scenario.put_row(&other, json!({"state":"invalid"}));
    scenario.preflight_stop(
        &format!(
            "setup's message-bus record names {}, not {}; nothing was changed",
            other.display(),
            scenario.nats.display()
        ),
        &[],
        &[],
    );
    for fields in [
        json!({"state":"pending", "root_pub":"", "signer_pub":"", "sysaccount_pub":""}),
        json!({"state":"pending", "root_pub":"", "signer_pub":"", "sysaccount_pub":"", "activation_pending":"false"}),
        json!({"state":"other", "root_pub":"", "signer_pub":"", "sysaccount_pub":"", "activation_pending":false}),
        json!({"state":"pending", "root_pub":PRIOR_ROOT, "signer_pub":"", "sysaccount_pub":"", "activation_pending":false}),
        json!({"state":"applied", "root_pub":PRIOR_ROOT, "signer_pub":SIGNER_PUB.to_uppercase(), "sysaccount_pub":SYSACCOUNT_PUB, "activation_pending":false}),
    ] {
        let mut scenario = Scenario::new();
        scenario.put_row(&scenario.nats.clone(), fields);
        scenario.preflight_stop(MALFORMED_ROW, &[], &[]);
    }
}

#[test]
fn list_failure_precedes_markers_and_leaves_no_row() {
    for (response, reason) in [
        (reply(4, "", "vault unreadable"), "vault unreadable"),
        (reply(0, "not json", ""), "invalid JSON or record fields"),
        (
            reply(0, r#"{"v":2,"credentials":[]}"#, ""),
            "unsupported v=2",
        ),
    ] {
        let mut scenario = Scenario::new();
        scenario.runner.lists.push_back(response);
        fs::write(&scenario.config, r#"{"modules":{"ckbus":{}}}"#).unwrap();
        let before = fs::read(&scenario.manifest).unwrap();
        let error = scenario.run().unwrap_err();
        assert_eq!(
            error,
            format!("ck auth list failed: {reason}; nothing was changed")
        );
        assert_eq!(fs::read(&scenario.manifest).unwrap(), before);
        assert_eq!(scenario.verbs(), ["list"]);
        assert!(scenario.ports().is_empty());
    }
}

#[test]
fn every_hand_made_marker_skips_before_binary_state_keys_and_ports() {
    for marker in [
        "ckbus",
        "nats-server",
        "operator.jwt",
        "system_account",
        "server.conf",
        "vault",
    ] {
        for absent in [false, true] {
            let mut scenario = Scenario::new();
            if absent {
                fs::remove_file(&scenario.runner.bus).unwrap();
            }
            let mut listed = vec![(keys::SIGNER, "retired")];
            let mut file = None;
            match marker {
                "vault" => listed.push(("signing:ck-bus-operator-root:1", "retired")),
                "ckbus" | "nats-server" => {
                    let config = format!("{{\n// kept\n\"modules\":{{\"{marker}\":{{}}}}}}\n");
                    fs::write(&scenario.config, &config).unwrap();
                    file = Some((scenario.config.clone(), config));
                }
                _ => {
                    fs::create_dir_all(&scenario.nats).unwrap();
                    let path = scenario.nats.join(marker);
                    let text = "hand-made bytes\n".to_owned();
                    fs::write(&path, &text).unwrap();
                    file = Some((path, text));
                }
            }
            scenario.runner.lists.push_back(listing(&listed));
            let before = fs::read(&scenario.manifest).unwrap();
            assert!(scenario.run().unwrap().is_none(), "{marker}");
            assert_eq!(fs::read(&scenario.manifest).unwrap(), before);
            if let Some((path, bytes)) = file {
                assert_eq!(fs::read(path).unwrap(), bytes.as_bytes());
            }
            assert_eq!(scenario.verbs(), ["list"]);
            assert!(scenario.ports().is_empty());
            assert_eq!(take_ceremony_signature_count(), 0);
        }
    }
}

#[test]
fn binary_key_state_and_port_stops_leave_no_row() {
    let mut scenario = Scenario::new();
    let unowned = scenario.runner.bus.with_file_name("unowned-ck-bus");
    fs::rename(&scenario.runner.bus, &unowned).unwrap();
    scenario.runner.bus = unowned;
    scenario
        .runner
        .lists
        .push_back(listing(&[(keys::SIGNER, "retired")]));
    scenario.preflight_stop(
        &format!(
            "{} is a ck-bus that setup did not install; it was not run, and nothing was changed",
            scenario.runner.bus.display()
        ),
        &["list"],
        &[],
    );

    let mut scenario = Scenario::new();
    fs::remove_file(&scenario.runner.bus).unwrap();
    scenario.preflight_stop(
        &format!(
            "{} is missing; rerun ck setup",
            scenario.runner.bus.display()
        ),
        &["list"],
        &[],
    );

    let mut scenario = Scenario::new();
    scenario
        .runner
        .lists
        .push_back(listing(&[(keys::ACCOUNT, "retired")]));
    scenario.control.busy = Some(14222);
    scenario.preflight_stop(
        &format!(
            "{} is in the vault but not active; nothing was changed",
            keys::ACCOUNT
        ),
        &["list"],
        &[],
    );

    for (port, seen) in [(14222, &[14222][..]), (18222, &[14222, 18222][..])] {
        let mut scenario = Scenario::new();
        scenario.control.busy = Some(port);
        scenario.preflight_stop(
            &format!("port {port} is in use; free it and rerun ck setup"),
            &["list"],
            seen,
        );
    }
}

#[test]
fn mint_recovers_existing_and_active_unknown_keys_and_retries_a_held_lease() {
    for (mint, relist, tries) in [
        (
            reply(
                1,
                "",
                "error: credential already exists (create-only; use a CAS overwrite)",
            ),
            None,
            1,
        ),
        (
            reply(5, "", "outcome unknown"),
            Some(listing(&[(keys::SIGNER, "active")])),
            1,
        ),
        (reply(3, "", "lease held"), None, 2),
    ] {
        let mut scenario = Scenario::new();
        scenario.runner.mints.push_back(mint);
        let unknown = relist.is_some();
        if let Some(relist) = relist {
            scenario.runner.lists.extend([listing(&[]), relist]);
        }
        scenario.run().unwrap().unwrap();
        let verbs = scenario.verbs();
        assert_eq!(verbs[0], "list");
        for verb in &verbs[1..=tries] {
            assert_eq!(verb, "mint-signing-key");
        }
        if unknown {
            assert_eq!(verbs[2], "list");
        }
        if tries == 1 {
            assert_eq!(
                scenario.process_args()[2 + usize::from(unknown)],
                ["public-key", "--id", keys::SIGNER]
            );
        } else {
            assert!(!verbs.iter().any(|verb| verb == "public-key"));
        }
        assert_eq!(take_ceremony_signature_count(), 2);
    }
}

#[test]
fn unknown_mint_outcome_requires_the_same_key_listed_active() {
    for (relist, expected) in [
        (
            listing(&[(keys::SIGNER, "retired")]),
            format!(
                "{} is in the vault but not active; rerun ck setup",
                keys::SIGNER
            ),
        ),
        (
            listing(&[(keys::SYSACCOUNT, "active")]),
            format!(
                "ck auth mint-signing-key --id {} ended with an unknown outcome; rerun ck setup",
                keys::SIGNER
            ),
        ),
        (
            reply(4, "", "vault unreadable"),
            format!(
                "ck auth mint-signing-key --id {} ended with an unknown outcome; rerun ck setup",
                keys::SIGNER
            ),
        ),
    ] {
        let mut scenario = Scenario::new();
        scenario.runner.lists.extend([listing(&[]), relist]);
        scenario.runner.mints.push_back(reply(5, "", "lost reply"));
        assert_eq!(scenario.run().unwrap_err(), expected);
        assert_eq!(scenario.verbs(), ["list", "mint-signing-key", "list"]);
        scenario.assert_pending();
        assert_eq!(take_ceremony_signature_count(), 0);
    }
}

#[test]
fn mint_refusal_and_public_key_failures_keep_the_pending_row() {
    for response in [
        reply(1, "", "error: invalid credential"),
        reply(4, "", "vault unreadable"),
    ] {
        let mut scenario = Scenario::new();
        let code = response.exit_code.unwrap();
        scenario.runner.mints.push_back(response);
        assert_eq!(
            scenario.run().unwrap_err(),
            format!(
                "ck auth mint-signing-key --id {} failed with exit {code}",
                keys::SIGNER
            )
        );
        assert_eq!(scenario.verbs(), ["list", "mint-signing-key"]);
        scenario.assert_pending();
    }
    for response in [
        reply(1, "", "unknown command"),
        reply(0, "public_key_hex 0123", ""),
    ] {
        let mut scenario = Scenario::new();
        scenario
            .runner
            .lists
            .push_back(listing(&[(keys::SIGNER, "active")]));
        scenario.runner.public.push_back(response);
        assert_eq!(
            scenario.run().unwrap_err(),
            format!(
                "setup needs ck auth public-key to read {}; run ck upgrade, then ck setup",
                keys::SIGNER
            )
        );
        assert_eq!(scenario.verbs(), ["list", "public-key"]);
        scenario.assert_pending();
    }
}

#[test]
fn recorded_root_and_fresh_root_refuse_bad_plan_responses_without_signing() {
    for recorded in [true, false] {
        let mut cases = vec![
            BadPlan::Exit,
            BadPlan::Json,
            BadPlan::Status,
            BadPlan::Empty,
            BadPlan::Unknown,
            BadPlan::Duplicate,
        ];
        if !recorded {
            cases.extend([
                BadPlan::Missing,
                BadPlan::Outside,
                BadPlan::Digest,
                BadPlan::MissingFile,
                BadPlan::NoCeremony,
            ]);
        }
        for bad in cases {
            let mut scenario = Scenario::new();
            if recorded {
                scenario.applied(false);
            }
            scenario.runner.plans.push_back(PlanReply::Bad(bad));
            let error = scenario.run().unwrap_err();
            let expected = match bad {
                BadPlan::Exit => "ck-bus install-plan failed: test plan refusal",
                BadPlan::Json => "ck-bus install-plan failed: invalid JSON",
                BadPlan::Status => "ck-bus install-plan failed: unknown status",
                _ => UNEXPECTED_PLAN,
            };
            assert_eq!(error, expected, "recorded={recorded} bad={bad:?}");
            assert_eq!(scenario.verbs().last().unwrap(), "install-plan");
            assert!(!scenario.verbs().iter().any(|verb| verb == "install-apply"));
            assert_eq!(
                take_ceremony_signature_count(),
                0,
                "recorded={recorded} bad={bad:?}"
            );
            if recorded {
                let row = scenario.observed.borrow().disk_record().unwrap();
                assert_eq!(row.root_pub, PRIOR_ROOT);
                assert!(!row.activation_pending);
            } else {
                scenario.assert_pending();
            }
        }
    }
}

#[test]
fn one_payload_check_replans_for_a_fresh_root_and_ignores_leftover_inputs() {
    let mut scenario = Scenario::new();
    scenario.applied(true);
    fs::create_dir_all(scenario.nats.join("ceremony")).unwrap();
    fs::write(scenario.nats.join("ceremony/old.input"), "leftover").unwrap();
    scenario
        .runner
        .plans
        .extend([PlanReply::CheckNeedsOne, PlanReply::Full]);
    let prepared = scenario.run().unwrap().unwrap();
    assert!(prepared.changed && prepared.prior && prepared.trust_changed);
    let args = scenario.process_args();
    let [check, fresh, apply] = &args[args.len() - 3..] else {
        panic!("expected two plans and apply")
    };
    assert_eq!(check[0], "install-plan");
    assert_eq!(fresh[0], "install-plan");
    assert_eq!(apply[0], "install-apply");
    assert_eq!(flag(check, "--root-pub"), PRIOR_ROOT);
    assert_eq!(flag(fresh, "--root-pub"), prepared.record.root_pub);
    assert_eq!(flag(apply, "--root-pub"), prepared.record.root_pub);
    assert_ne!(prepared.record.root_pub, PRIOR_ROOT);
    let observed = scenario.observed.borrow();
    assert_eq!(
        observed.before_process[args.len() - 2],
        Some(BusTrust::pending())
    );
    assert_eq!(take_ceremony_signature_count(), 2);
    assert_eq!(
        fs::read(scenario.nats.join("ceremony/old.input")).unwrap(),
        b"leftover"
    );
}

#[test]
fn recorded_root_applies_without_signatures_and_writes_ahead_before_rendering() {
    for re_render in [false, true] {
        let mut scenario = Scenario::new();
        scenario.applied(false);
        scenario.runner.plans.push_back(PlanReply::NoCeremony);
        scenario.runner.re_render = re_render;
        let prepared = scenario.run().unwrap().unwrap();
        assert_eq!(prepared.changed, re_render);
        assert_eq!(prepared.trust_changed, re_render);
        assert!(!prepared.prior);
        assert_eq!(prepared.record.root_pub, PRIOR_ROOT);
        assert!(prepared.record.activation_pending);
        assert!(!scenario
            .verbs()
            .iter()
            .any(|verb| verb == "mint-signing-key"));
        let observed = scenario.observed.borrow();
        assert!(
            !observed
                .before_process
                .iter()
                .rev()
                .nth(1)
                .unwrap()
                .as_ref()
                .unwrap()
                .activation_pending
        );
        assert!(
            observed
                .before_process
                .last()
                .unwrap()
                .as_ref()
                .unwrap()
                .activation_pending
        );
        assert_eq!(observed.disk_record(), Some(prepared.record));
        assert_eq!(take_ceremony_signature_count(), 0);
    }
}

#[test]
fn invalid_apply_response_retains_the_write_ahead_state_for_the_phase() {
    for recorded in [true, false] {
        for bad in [
            BadApply::Exit,
            BadApply::Status,
            BadApply::Missing,
            BadApply::Empty,
            BadApply::WrongPath,
        ] {
            let mut scenario = Scenario::new();
            if recorded {
                scenario.applied(false);
                scenario.runner.plans.push_back(PlanReply::NoCeremony);
            }
            scenario.runner.bad_apply = Some(bad);
            let error = scenario.run().unwrap_err();
            assert!(
                error.starts_with("ck-bus install-apply failed: "),
                "{bad:?}: {error}"
            );
            assert_eq!(scenario.verbs().last().unwrap(), "install-apply");
            let row = scenario.observed.borrow().disk_record().unwrap();
            if recorded {
                assert_eq!(row.root_pub, PRIOR_ROOT);
                assert!(row.activation_pending);
            } else {
                assert_eq!(row, BusTrust::pending());
            }
            assert!(!scenario.config.exists());
            assert_eq!(
                take_ceremony_signature_count(),
                if recorded { 0 } else { 2 }
            );
        }
    }
}

#[test]
fn planted_seed_in_any_scan_file_keeps_the_row_pending() {
    let ceremony = Path::new("ceremony");
    for file in [
        PathBuf::from("server.conf"),
        PathBuf::from("operator.jwt"),
        PathBuf::from("system_account"),
        ceremony.join("operator.input"),
        ceremony.join("system_account.input"),
    ] {
        let mut scenario = Scenario::new();
        scenario.runner.plant_seed_in = Some(file.clone());
        let error = scenario.run().expect_err(&file.display().to_string());
        assert_eq!(
            error,
            format!(
                "an nkey seed was found in written file: {}",
                scenario.nats.join(file).display()
            )
        );
        scenario.assert_pending();
        assert!(!scenario.config.exists());
        assert_eq!(take_ceremony_signature_count(), 2);
    }
}
