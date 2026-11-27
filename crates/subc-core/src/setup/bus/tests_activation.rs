use std::collections::{BTreeMap, BTreeSet};

use cortexkit_test_support::ScratchDir;
use serde_json::json;

use crate::setup::{install_journal::Journal, runtime::CommandResult};

use super::{
    tests_preflight::{bus_nats_dir, read_observed_file, FIXED_ROOT, USE_FIXED_ROOT},
    *,
};

const NATS: &str = NATS_SERVER_MODULE;
const BUS: &str = CKBUS_MODULE;
const IDS: [&str; 3] = [keys::SIGNER, keys::SYSACCOUNT, keys::ACCOUNT];

struct Files {
    root: ScratchDir,
    manifest: PathBuf,
    nats: PathBuf,
    config: PathBuf,
    bus: PathBuf,
    auth: PathBuf,
    broker: PathBuf,
    connection: PathBuf,
    private: [Vec<u8>; 4],
}

impl Files {
    fn new() -> Self {
        let root = ScratchDir::new("setup-bus-activation");
        for dir in ["setup", "config", "bin"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        Self {
            manifest: root.join("setup/install.json"),
            nats: root.join("data").join("nats"),
            config: root.join("config/subc.jsonc"),
            bus: root.join(format!("bin/ck-bus{}", std::env::consts::EXE_SUFFIX)),
            auth: root.join(format!("bin/ck-auth{}", std::env::consts::EXE_SUFFIX)),
            broker: root.join(format!(
                "bin/ck-nats-server{}",
                std::env::consts::EXE_SUFFIX
            )),
            connection: root.join("runtime/connection.json"),
            root,
            private: [
                FIXED_ROOT.to_vec(),
                hex(&FIXED_ROOT).into_bytes(),
                URL_SAFE_NO_PAD.encode(FIXED_ROOT).into_bytes(),
                b"SOABOFYXC4LROFYXC4LROFYXC4LROFYXC4LROFYXC4LROFYXC4LROF3SXQ".to_vec(),
            ],
        }
    }

    fn lock(&self) -> Inventory {
        Inventory::load_locked(self.manifest.clone(), "test", "setup", Duration::ZERO).unwrap()
    }

    fn pending(&self) -> bool {
        let inventory = Inventory::load_snapshot(self.manifest.clone(), "test").unwrap();
        BusTrust::read(&inventory, &self.nats)
            .unwrap()
            .unwrap()
            .activation_pending
    }

    fn env(nats_dir: &Path) -> Value {
        json!({
            "CKBUS_NATS_URL": "nats://127.0.0.1:14222",
            "CKBUS_OPERATOR_JWT": nats_dir.join("operator.jwt").display().to_string(),
            "CKBUS_SYSTEM_ACCOUNT": "test-system-account",
        })
    }

    fn config(&self) -> Value {
        config::read_registration_config(&self.config)
            .unwrap()
            .unwrap()
            .1
    }

    fn check(&self, bytes: &[u8], location: &str) {
        for form in &self.private {
            assert!(
                !bytes.windows(form.len()).any(|window| window == form),
                "private root in {location}"
            );
        }
    }

    fn observe(&self, dir: &Path) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                self.observe(&path);
            } else {
                self.check(
                    &read_observed_file(&path, &self.manifest),
                    &path.display().to_string(),
                );
            }
        }
    }
}

#[derive(Default)]
struct Tools {
    root: Option<String>,
    minted: BTreeSet<String>,
    commands: Vec<Vec<String>>,
    re_render: bool,
    grants_change: bool,
}

struct Runner<'a>(&'a Files, &'a mut Tools);

fn key_half(id: &str) -> String {
    let byte = if id == keys::SIGNER { 1 } else { 2 };
    format!(
        "public_key_hex {}\nkey_id 0123456789abcdef\n",
        hex(&[byte; 32])
    )
}

impl CommandRunner for Runner<'_> {
    fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
        let Self(files, tools) = self;
        tools.commands.push(args.to_vec());
        files.check(program.as_bytes(), "program");
        for arg in args {
            files.check(arg.as_bytes(), "argument");
        }
        for (key, value) in std::env::vars_os() {
            files.check(key.as_encoded_bytes(), "child environment name");
            files.check(value.as_encoded_bytes(), "child environment value");
        }
        files.observe(files.root.path());
        let stdout = if Path::new(program) == files.auth {
            match args[0].as_str() {
                "list" => json!({"v":1,"credentials": tools.minted.iter().map(|id| json!({"id":id,"state":"active"})).collect::<Vec<_>>()}).to_string(),
                "mint-signing-key" => {
                    let id = &args[2];
                    assert!(IDS.contains(&id.as_str()));
                    assert!(tools.minted.insert(id.clone()));
                    assert!(!files.pending());
                    format!("created {id}\n{}", key_half(id))
                }
                "public-key" => key_half(&args[2]),
                "grant" if tools.grants_change => "granted".into(),
                "grant" => "already granted (no change)".into(),
                other => panic!("unexpected vault command {other}"),
            }
        } else {
            assert_eq!(Path::new(program), files.bus);
            let nats = bus_nats_dir(args);
            let root = &args[4];
            match args[0].as_str() {
                "install-plan" if tools.root.as_ref() == Some(root) => {
                    json!({"status":"no ceremony needed"}).to_string()
                }
                "install-plan" => {
                    let ceremony = nats.join("ceremony");
                    fs::create_dir_all(&ceremony).unwrap();
                    fs::write(nats.join("system_account"), "system account").unwrap();
                    let payloads = ["operator", "system_account"].map(|payload| {
                        let path = ceremony.join(format!("{payload}.input"));
                        fs::write(&path, payload).unwrap();
                        json!({"payload":payload,"path":path.display().to_string(),"sha256":hex(&Sha256::digest(payload.as_bytes()))})
                    });
                    json!({"status":"ceremony needed", "payloads":payloads}).to_string()
                }
                "install-apply" => {
                    if tools.root.as_ref() != Some(root) {
                        fs::write(nats.join("operator.jwt"), "public operator JWT").unwrap();
                        fs::write(nats.join("server.conf"), "server configuration").unwrap();
                        tools.root = Some(root.clone());
                    }
                    if tools.re_render {
                        fs::write(nats.join("server.conf"), "changed configuration").unwrap();
                    }
                    json!({"status":"applied","env":Files::env(&nats)}).to_string()
                }
                other => panic!("unexpected bus command {other}"),
            }
        };
        files.observe(files.root.path());
        Ok(CommandResult {
            success: true,
            exit_code: Some(0),
            stdout,
            stderr: String::new(),
        })
    }
}

#[derive(Clone, Copy)]
struct Module {
    enabled: bool,
    pid: Option<u32>,
    healthy: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Call {
    Status(String),
    Rescan,
    Pid(String, Option<u32>),
    Restart(String),
}

#[derive(Clone, Copy)]
enum Stuck {
    Pid,
    Down,
}

#[derive(Default)]
struct Daemon {
    modules: BTreeMap<String, Module>,
    calls: Vec<Call>,
    config_at_status: Vec<Option<Vec<u8>>>,
    reports: Vec<(Vec<String>, Vec<String>)>,
    next_pid: u32,
    elapsed: Duration,
    rescan_time: Duration,
    restart_time: Duration,
    fail_after: Option<usize>,
    skip: Option<&'static str>,
    stuck: Option<Stuck>,
    status_error: Option<&'static str>,
}

impl Daemon {
    fn spawn(&mut self, id: &str) {
        self.next_pid += 1;
        self.modules.insert(
            id.into(),
            Module {
                enabled: true,
                pid: Some(self.next_pid),
                healthy: true,
            },
        );
    }

    fn restarts(&self) -> Vec<&str> {
        self.calls
            .iter()
            .filter_map(|call| match call {
                Call::Restart(id) => Some(id.as_str()),
                _ => None,
            })
            .collect()
    }

    fn rescans(&self) -> usize {
        self.calls
            .iter()
            .filter(|call| **call == Call::Rescan)
            .count()
    }
}

struct Control<'a> {
    files: &'a Files,
    daemon: &'a mut Daemon,
    start: Instant,
}

impl Control<'_> {
    fn observe(&self) {
        self.files.observe(self.files.root.path());
    }
}

impl BusControl for Control<'_> {
    fn port_is_free(&mut self, _: u16) -> bool {
        self.observe();
        true
    }

    fn status(&mut self, id: &str) -> Result<Option<BusStatus>, String> {
        self.daemon.calls.push(Call::Status(id.into()));
        self.daemon
            .config_at_status
            .push(fs::read(&self.files.config).ok());
        self.observe();
        if self.daemon.status_error == Some(id) {
            return Err("fixture status error".into());
        }
        Ok(self.daemon.modules.get(id).map(|module| BusStatus {
            live: module.pid.is_some(),
            healthy: module.healthy,
        }))
    }

    fn provenance(&mut self, id: &str) -> Result<Option<u32>, String> {
        let pid = self.daemon.modules.get(id).and_then(|module| module.pid);
        self.daemon.calls.push(Call::Pid(id.into(), pid));
        self.observe();
        Ok(pid)
    }

    fn rescan(&mut self) -> Result<BusRescan, String> {
        self.daemon.calls.push(Call::Rescan);
        self.daemon.elapsed += self.daemon.rescan_time;
        self.observe();
        assert!(!Journal::exists(&self.files.manifest).unwrap());
        assert!(self.files.pending());
        let config = self.files.config();
        let mut result = BusRescan::default();
        // The daemon compares the current supervisor roster with the committed entries.
        for id in [BUS, NATS] {
            if self.daemon.fail_after == Some(result.added.len() + result.enabled_changes.len()) {
                return Err("fixture rescan error".into());
            }
            assert_eq!(config["modules"][id]["enabled"], true);
            if self.daemon.skip == Some(id) {
                continue;
            }
            match self.daemon.modules.get(id) {
                None => result.added.push(id.into()),
                Some(module) if !module.enabled => result.enabled_changes.push(id.into()),
                Some(_) => continue,
            }
            self.daemon.spawn(id);
        }
        self.daemon
            .reports
            .push((result.added.clone(), result.enabled_changes.clone()));
        Ok(result)
    }

    fn restart(&mut self, id: &str, drain: Duration) -> Result<(), String> {
        self.daemon.calls.push(Call::Restart(id.into()));
        assert_eq!(drain, Duration::from_secs(30));
        assert!(self.files.pending());
        assert!(!Journal::exists(&self.files.manifest).unwrap());
        self.daemon.elapsed += self.daemon.restart_time;
        if self.daemon.stuck.is_none() {
            self.daemon.spawn(id);
        }
        self.observe();
        Ok(())
    }

    fn now(&mut self) -> Instant {
        self.observe();
        self.start + self.daemon.elapsed
    }

    fn sleep(&mut self, duration: Duration) {
        self.observe();
        self.daemon.elapsed += duration;
    }
}

struct Fixture {
    files: Files,
    tools: Tools,
    daemon: Daemon,
}

impl Fixture {
    fn new() -> Self {
        let files = Files::new();
        let mut inventory = files.lock();
        for path in [&files.auth, &files.bus, &files.broker] {
            fs::write(path, "fixture binary").unwrap();
            inventory
                .persist_retained("managed-binary", path, Default::default())
                .unwrap();
        }
        Self {
            files,
            tools: Tools::default(),
            daemon: Daemon::default(),
        }
    }

    fn ready() -> Self {
        let mut fixture = Self::new();
        assert_eq!(fixture.run().unwrap(), Some(true));
        fixture.clear_calls();
        fixture
    }

    fn clear_calls(&mut self) {
        self.daemon.calls.clear();
        self.daemon.reports.clear();
        self.tools.commands.clear();
    }

    fn run(&mut self) -> Result<Option<bool>, String> {
        let mut inventory = self.files.lock();
        let result = self.run_with(&mut inventory);
        assert!(!inventory.transaction_is_open());
        assert!(!Journal::exists(&self.files.manifest).unwrap());
        result
    }

    fn run_with(&mut self, inventory: &mut Inventory) -> Result<Option<bool>, String> {
        let files = &self.files;
        take_ceremony_signature_count();
        let prior = USE_FIXED_ROOT.replace(true);
        self.daemon.config_at_status.clear();
        let result = configure(
            &BusPaths {
                nats_dir: &files.nats,
                config_path: &files.config,
                ck_bus: &files.bus,
                ck_auth: &files.auth,
            },
            &files.broker,
            &files.connection,
            inventory,
            &mut Runner(files, &mut self.tools),
            &mut Control {
                files,
                daemon: &mut self.daemon,
                start: Instant::now(),
            },
            |_| {},
        );
        USE_FIXED_ROOT.set(prior);
        files.observe(files.root.path());
        result
    }

    fn edit(&self, edit: impl FnOnce(&mut String)) -> Vec<u8> {
        let mut config = fs::read_to_string(&self.files.config).unwrap();
        edit(&mut config);
        fs::write(&self.files.config, &config).unwrap();
        config.into_bytes()
    }

    fn expect_repair(&mut self, rescans: usize, restarted: &[&str]) {
        assert_eq!(self.run().unwrap(), Some(true));
        assert_eq!(self.daemon.rescans(), rescans);
        assert_eq!(self.daemon.restarts(), restarted);
        assert!(!self.files.pending());
        assert_eq!(take_ceremony_signature_count(), 0);
    }
}

#[test]
fn fresh_activation_is_one_committed_rescan_and_settled_rerun_is_byte_identical() {
    let mut fixture = Fixture::new();
    assert_eq!(fixture.run().unwrap(), Some(true));
    assert_eq!(fixture.daemon.config_at_status[..2], [None, None]);
    assert_eq!(
        fixture.daemon.calls[..3],
        [
            Call::Status(NATS.into()),
            Call::Status(BUS.into()),
            Call::Rescan
        ]
    );
    assert_eq!(fixture.daemon.rescans(), 1);
    assert_eq!(
        fixture.daemon.reports,
        [(vec![BUS.to_owned(), NATS.to_owned()], vec![])]
    );
    assert!(fixture.daemon.restarts().is_empty());
    assert!(!fixture.files.pending());
    assert_eq!(take_ceremony_signature_count(), 2);
    let verbs: Vec<_> = fixture
        .tools
        .commands
        .iter()
        .map(|args| args[0].as_str())
        .collect();
    assert_eq!(
        verbs,
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
    assert_eq!(
        fixture.files.config()["modules"],
        json!({
            NATS: {
                "program": fixture.files.broker,
                "args": ["-c", fixture.files.nats.join("server.conf"), "--subc", fixture.files.connection],
                "protocol": "subc",
                "health": {"cadence_ms": 30000, "deadline_ms": 5000},
                "registration_channel": true, "reserved": true, "enabled": true,
            },
            BUS: {
                "program": fixture.files.bus, "args": [], "env": Files::env(&fixture.files.nats),
                "registration_channel": true, "reserved": true, "enabled": true,
            }
        })
    );

    fixture.clear_calls();
    let paths = [
        fixture.files.manifest.clone(),
        fixture.files.config.clone(),
        fixture.files.nats.join("server.conf"),
    ];
    let before = paths.clone().map(|path| fs::read(path).unwrap());
    let mut inventory = fixture.files.lock();
    assert_eq!(fixture.run_with(&mut inventory).unwrap(), Some(false));
    inventory.save().unwrap(); // A settled run must not even open a transaction.
    assert_eq!(paths.map(|path| fs::read(path).unwrap()), before);
    assert_eq!(fixture.daemon.rescans(), 0);
    assert!(fixture.daemon.restarts().is_empty());
    assert_eq!(take_ceremony_signature_count(), 0);
    assert!(!fixture
        .tools
        .commands
        .iter()
        .any(|args| args[0] == "mint-signing-key"));
}

#[test]
fn missing_entries_and_unregistered_modules_are_repaired_without_restarting() {
    let mut fixture = Fixture::ready();
    fs::remove_file(&fixture.files.config).unwrap();
    fixture.daemon.modules.clear();
    fixture.expect_repair(1, &[]);
    assert_eq!(
        fixture.daemon.reports,
        [(vec![BUS.to_owned(), NATS.to_owned()], vec![])]
    );
}

#[test]
fn stale_or_missing_ckbus_entry_restarts_the_preexisting_process_only() {
    for missing in [false, true] {
        let mut fixture = Fixture::ready();
        let old_pid = fixture.daemon.modules[BUS].pid;
        let before = fixture.edit(|config| {
            if missing {
                subc_jsonc::remove_member(config, &["modules", BUS]).unwrap();
            } else {
                subc_jsonc::set_member(
                    config,
                    &["modules", BUS, "env"],
                    &json!({"stale": "value"}),
                )
                .unwrap();
            }
        });
        fixture.expect_repair(1, &[BUS]);
        assert_eq!(
            fixture.daemon.config_at_status[..2],
            [Some(before.clone()), Some(before)]
        );
        assert_eq!(
            fixture.files.config()["modules"][BUS]["env"],
            Files::env(&fixture.files.nats)
        );
        assert_ne!(fixture.daemon.modules[BUS].pid, old_pid);
    }
}

#[test]
fn required_values_are_repaired_and_unmanaged_fields_are_preserved() {
    for key in ["registration_channel", "reserved", "enabled"] {
        let mut fixture = Fixture::ready();
        fixture.edit(|config| {
            subc_jsonc::set_member(config, &["modules", NATS, key], &json!(false)).unwrap();
            subc_jsonc::set_member(
                config,
                &["modules", NATS, "health"],
                &json!({"cadence_ms":12345,"deadline_ms":2345}),
            )
            .unwrap();
            subc_jsonc::set_member(
                config,
                &["modules", NATS, "env"],
                &json!({"CUSTOM": "kept"}),
            )
            .unwrap();
        });
        if key == "enabled" {
            fixture.daemon.modules.get_mut(NATS).unwrap().enabled = false;
        }
        fixture.expect_repair(1, if key == "enabled" { &[] } else { &[NATS] });
        let repaired = &fixture.files.config()["modules"][NATS];
        assert_eq!(repaired[key], true);
        assert_eq!(
            repaired["health"],
            json!({"cadence_ms":12345,"deadline_ms":2345})
        );
        assert_eq!(repaired["env"], json!({"CUSTOM":"kept"}));
        let reenabled: Vec<_> = (key == "enabled")
            .then(|| NATS.to_owned())
            .into_iter()
            .collect();
        assert_eq!(fixture.daemon.reports, [(vec![], reenabled)]);
    }

    let mut fixture = Fixture::ready();
    fixture.edit(|config| {
        subc_jsonc::set_member(config, &["modules", BUS, "protocol"], &json!("subc")).unwrap();
        subc_jsonc::set_member(config, &["unrelated"], &json!("kept")).unwrap();
        config.insert_str(0, "// An operator setting\n");
    });
    fixture.expect_repair(1, &[BUS]);
    assert!(fixture.files.config()["modules"][BUS]
        .get("protocol")
        .is_none());
    assert_eq!(fixture.files.config()["unrelated"], "kept");
    assert!(fs::read_to_string(&fixture.files.config)
        .unwrap()
        .starts_with("// An operator setting\n"));
}

#[test]
fn server_config_rerender_restarts_both_without_waiting_between_restarts() {
    let mut fixture = Fixture::ready();
    fixture.tools.re_render = true;
    let nats_pid = fixture.daemon.modules[NATS].pid;
    let bus_pid = fixture.daemon.modules[BUS].pid;
    fixture.expect_repair(1, &[NATS, BUS]);
    assert_eq!(
        fixture.daemon.calls[2..7],
        [
            Call::Rescan,
            Call::Pid(NATS.into(), nats_pid),
            Call::Restart(NATS.into()),
            Call::Pid(BUS.into(), bus_pid),
            Call::Restart(BUS.into())
        ]
    );
    assert_ne!(fixture.daemon.modules[NATS].pid, nats_pid);
    assert_ne!(fixture.daemon.modules[BUS].pid, bus_pid);
}

#[test]
fn prior_bit_recovers_a_rerender_before_activation() {
    let mut fixture = Fixture::ready();
    fixture.tools.re_render = true;
    {
        let files = &fixture.files;
        prepare(
            &BusPaths {
                nats_dir: &files.nats,
                config_path: &files.config,
                ck_bus: &files.bus,
                ck_auth: &files.auth,
            },
            &mut files.lock(),
            &mut Runner(files, &mut fixture.tools),
            &mut Control {
                files,
                daemon: &mut fixture.daemon,
                start: Instant::now(),
            },
            |_| {},
        )
        .unwrap()
        .unwrap();
    }
    assert!(fixture.files.pending());
    fixture.expect_repair(1, &[NATS, BUS]);
}

#[test]
fn registered_but_not_live_restarts_without_rescanning_and_accepts_any_pid() {
    let mut fixture = Fixture::ready();
    fixture.daemon.modules.get_mut(BUS).unwrap().pid = None;
    fixture.expect_repair(0, &[BUS]);
    assert_eq!(fixture.daemon.calls[2], Call::Pid(BUS.into(), None));
    assert!(fixture.daemon.modules[BUS].pid.is_some());
}

#[test]
fn failed_rescan_leaves_committed_entries_and_retries_with_the_prior_bit() {
    for prefix in [0, 1] {
        let mut fixture = Fixture::ready();
        fixture.daemon.modules.remove(BUS);
        fixture.edit(|config| {
            subc_jsonc::remove_member(config, &["modules", BUS]).unwrap();
        });
        fixture.daemon.fail_after = Some(prefix);
        assert_eq!(
            fixture.run().unwrap_err(),
            "ck module rescan failed: fixture rescan error"
        );
        assert!(fixture.files.pending());
        assert_eq!(
            fixture.files.config()["modules"][BUS]["env"],
            Files::env(&fixture.files.nats)
        );
        assert_eq!(fixture.daemon.modules.contains_key(BUS), prefix == 1);
        assert_eq!(fixture.daemon.rescans(), 1);
        assert!(fixture.daemon.restarts().is_empty());

        fixture.daemon.fail_after = None;
        fixture.clear_calls();
        // A retry cannot distinguish a retained prefix from a prior registration.
        let expected: &[&str] = if prefix == 1 { &[NATS, BUS] } else { &[NATS] };
        fixture.expect_repair(1, expected);
        let added: Vec<_> = (prefix == 0).then(|| BUS.to_owned()).into_iter().collect();
        assert_eq!(fixture.daemon.reports, [(added, vec![])]);
    }
}

#[test]
fn grant_change_counts_without_a_rescan_or_restart() {
    let mut fixture = Fixture::ready();
    fixture.tools.grants_change = true;
    fixture.expect_repair(0, &[]);
}

#[test]
fn hand_made_entries_do_not_activate() {
    let mut fixture = Fixture::new();
    let config = b"{\"modules\":{\"ckbus\":{\"reserved\":true}}}\n";
    fs::write(&fixture.files.config, config).unwrap();
    let manifest = fs::read(&fixture.files.manifest).unwrap();
    assert_eq!(fixture.run().unwrap(), None);
    assert!(fixture.daemon.calls.is_empty());
    assert_eq!(fs::read(&fixture.files.manifest).unwrap(), manifest);
    assert_eq!(fs::read(&fixture.files.config).unwrap(), config);
}

fn timeout_message(id: &str) -> String {
    format!("{id} did not come up after the message-bus step; check ck module status {id}, then rerun ck setup")
}

#[test]
fn unchanged_pid_times_out_after_one_budget_including_restart_time() {
    let mut fixture = Fixture::ready();
    fixture.daemon.stuck = Some(Stuck::Pid);
    fixture.daemon.rescan_time = Duration::from_secs(23);
    fixture.daemon.restart_time = Duration::from_secs(37);
    fixture.tools.re_render = true;
    assert_eq!(fixture.run().unwrap_err(), timeout_message(NATS));
    assert_eq!(fixture.daemon.restarts(), [NATS, BUS]);
    assert_eq!(fixture.daemon.elapsed, Duration::from_secs(203));
    assert!(fixture.files.pending());
}

#[test]
fn unhealthy_stopped_and_unregistered_modules_remain_pending_on_timeout() {
    for (mode, expected_rescans, expected_restarts) in [
        ("unhealthy", 0, &[][..]),
        ("stopped", 0, &[BUS][..]),
        ("unregistered", 1, &[][..]),
    ] {
        let mut fixture = Fixture::ready();
        assert!(!fixture.files.pending());
        match mode {
            "unhealthy" => fixture.daemon.modules.get_mut(BUS).unwrap().healthy = false,
            "stopped" => {
                fixture.daemon.modules.get_mut(BUS).unwrap().pid = None;
                fixture.daemon.stuck = Some(Stuck::Down);
            }
            "unregistered" => {
                fixture.daemon.modules.remove(BUS);
                fixture.daemon.skip = Some(BUS);
            }
            _ => unreachable!(),
        }
        assert_eq!(fixture.run().unwrap_err(), timeout_message(BUS));
        assert_eq!(fixture.daemon.elapsed, Duration::from_secs(180));
        assert_eq!(fixture.daemon.rescans(), expected_rescans);
        assert_eq!(fixture.daemon.restarts(), expected_restarts);
        assert!(fixture.files.pending());
    }
}

#[test]
fn status_failure_stops_before_the_entry_transaction() {
    let mut fixture = Fixture::ready();
    let before = fixture.edit(|config| {
        subc_jsonc::remove_member(config, &["modules", BUS]).unwrap();
    });
    fixture.daemon.status_error = Some(BUS);
    let mut inventory = fixture.files.lock();
    assert_eq!(
        fixture.run_with(&mut inventory).unwrap_err(),
        "ck module status ckbus failed: fixture status error"
    );
    inventory.save().unwrap();
    assert_eq!(fs::read(&fixture.files.config).unwrap(), before);
    assert_eq!(
        fixture.daemon.calls,
        [Call::Status(NATS.into()), Call::Status(BUS.into())]
    );
    assert!(fixture.files.pending());
}

#[test]
#[cfg(unix)]
fn entry_write_error_closes_and_undoes_the_bus_transaction() {
    let mut fixture = Fixture::ready();
    let config = &fixture.files.config;
    let original = config.with_file_name("operator-config.jsonc");
    fs::rename(config, &original).unwrap();
    fs::write(&original, "{}\n").unwrap();
    std::os::unix::fs::symlink(&original, config).unwrap();

    let mut inventory = fixture.files.lock();
    let error = fixture.run_with(&mut inventory).unwrap_err();
    assert!(
        error.contains(&fixture.files.config.display().to_string()),
        "{error}"
    );
    assert!(!inventory.transaction_is_open());
    assert!(inventory
        .save()
        .unwrap_err()
        .contains("after opening an install transaction"));
    assert!(!Journal::exists(&fixture.files.manifest).unwrap());
    assert_eq!(fs::read(&original).unwrap(), b"{}\n");
    assert_eq!(fixture.daemon.rescans(), 0);
    assert!(fixture.daemon.restarts().is_empty());
    assert!(fixture.files.pending());
}
