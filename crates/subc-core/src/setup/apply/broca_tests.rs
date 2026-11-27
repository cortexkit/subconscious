use super::*;
use crate::setup::{
    apply::module_steps_tests::{inventory, restart},
    install_journal::bootstrap::FileRecovery,
    runtime::{CommandResult, CommandRunner},
};
use cortexkit_test_support::ScratchDir;
use serde_json::json;
use std::{collections::VecDeque, fs, io::Write, path::PathBuf};

#[derive(Clone, Copy)]
enum Put {
    Create,
    Collision,
    LostCreated,
    Refused,
}

struct FakeVault {
    root: PathBuf,
    put: VecDeque<Put>,
    calls: Vec<Vec<String>>,
}

impl FakeVault {
    fn new(root: &Path, put: impl IntoIterator<Item = Put>) -> Self {
        Self {
            root: root.to_owned(),
            put: put.into_iter().collect(),
            calls: Vec::new(),
        }
    }
    fn records(&self) -> Vec<Value> {
        fs::read(self.root.join("vault.json")).map_or_else(
            |error| {
                assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
                Vec::new()
            },
            |bytes| serde_json::from_slice(&bytes).unwrap(),
        )
    }
    fn add(&self, id: &str, state: &str) {
        let mut records = self.records();
        records.push(json!({"id":id,"state":state}));
        fs::write(
            self.root.join("vault.json"),
            serde_json::to_vec(&records).unwrap(),
        )
        .unwrap();
    }
    fn puts(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("puts.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
    fn auth(&mut self) -> Auth<'_> {
        Auth {
            program: Path::new("ck-auth"),
            runner: self,
            wait: |_| panic!("unexpected wait"),
        }
    }
    fn grants(&self) -> Vec<&str> {
        self.calls
            .iter()
            .filter(|args| args[0] == "grant")
            .map(|args| args[6].as_str())
            .collect()
    }
}

impl CommandRunner for FakeVault {
    fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
        assert_eq!(program, "ck-auth");
        self.calls.push(args.to_vec());
        let (code, stdout) = match args[0].as_str() {
            "list" => {
                assert_eq!(args, ["list", "--json"]);
                (0, json!({"v":1,"credentials":self.records()}).to_string())
            }
            "grant" => {
                assert!(args[6].starts_with(PREFIX));
                assert_eq!(
                    args,
                    [
                        "grant",
                        "--principal",
                        "reserved:broca",
                        "--selector-kind",
                        "exact",
                        "--selector",
                        &args[6],
                        "--operation",
                        "read"
                    ]
                );
                (0, "already granted (no change)\n".into())
            }
            "put" => {
                assert_eq!(
                    args,
                    [
                        "put",
                        "--id",
                        &args[2],
                        "--kind",
                        "opaque",
                        "--generate-hex",
                        "32"
                    ]
                );
                let key = &args[2];
                assert!(self.records().iter().all(|record| record["id"] != *key));
                let mut log = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.root.join("puts.log"))
                    .unwrap();
                writeln!(log, "{key}").unwrap();
                let put = self.put.pop_front().unwrap_or(Put::Create);
                let code = match put {
                    Put::Create => 0,
                    Put::Collision | Put::Refused => 1,
                    Put::LostCreated => 5,
                };
                if !matches!(put, Put::Refused) {
                    self.add(key, "active");
                }
                (code, String::new())
            }
            _ => panic!("unexpected auth verb: {args:?}"),
        };
        Ok(CommandResult {
            success: code == 0,
            exit_code: Some(code),
            stdout,
            stderr: String::new(),
        })
    }
}

fn keyfile(key: u16) -> String {
    format!("{{\"session_storage\":{{\"deployment_key_id\":{key}}}}}\n")
}

fn run(root: &Path, vault: &mut FakeVault) -> Result<String, String> {
    let path = root.join("broca.jsonc");
    let mut inventory = restart(root);
    inventory.start_transaction().unwrap();
    let result = ensure(&path, &mut inventory, &mut vault.auth());
    if result.is_ok() {
        inventory.commit_transaction(&mut FileRecovery).unwrap();
    } else {
        inventory.undo_transaction(&mut FileRecovery).unwrap();
    }
    result
}

#[test]
fn existing_key_one_is_granted_without_touching_unpinned_settings() {
    let root = ScratchDir::new("broca-state-1");
    let file = root.join("broca.jsonc");
    let settings = "{ // keep comment\n}\n";
    fs::write(&file, settings).unwrap();
    let mut vault = FakeVault::new(root.path(), []);
    vault.add(&id(1), "active");
    assert_eq!(run(root.path(), &mut vault).unwrap(), "no change");
    assert_eq!(fs::read_to_string(file).unwrap(), settings);
    assert_eq!(vault.calls.len(), 2);
    assert_eq!(vault.grants(), [id(1)]);
    assert!(vault.puts().is_empty());
}

#[test]
fn explicit_settings_key_need_not_exist_in_vault_and_is_not_changed() {
    let settings = keyfile(42);
    for state in [None, Some("retired")] {
        let root = ScratchDir::new("broca-state-2");
        let file = root.join("broca.jsonc");
        fs::write(&file, &settings).unwrap();
        let mut vault = FakeVault::new(root.path(), []);
        if let Some(state) = state {
            vault.add(&id(42), state);
        }
        vault.add(&id(7), "active");
        assert_eq!(run(root.path(), &mut vault).unwrap(), "no change");
        assert_eq!(vault.grants(), [id(7)]);
        assert!(vault.puts().is_empty());
        assert_eq!(fs::read_to_string(file).unwrap(), settings);
    }
}

#[test]
fn pending_active_key_is_granted_along_with_other_active_keys() {
    let root = ScratchDir::new("broca-state-3");
    let file = root.join("broca.jsonc");
    save_intent(&mut inventory(root.path()), &file, 2).unwrap();
    let mut vault = FakeVault::new(root.path(), []);
    vault.add(&id(2), "active");
    vault.add(&id(3), "active");
    assert_eq!(run(root.path(), &mut vault).unwrap(), "ensured");
    assert!(!file.exists());
    assert_eq!(vault.grants(), [id(2), id(3)]);
    assert!(vault.puts().is_empty());
    let rows = inventory(root.path());
    assert!(!rows.owns_path(INTENT, &file));
    assert!(!rows.owns_path("settings-receipt", &file));
}

#[test]
fn state4_pending_absent_key_is_created_but_inactive_key_refuses() {
    for active in [true, false] {
        let root = ScratchDir::new("broca-state-4");
        let file = root.join("broca.jsonc");
        save_intent(&mut inventory(root.path()), &file, 42).unwrap();
        let mut vault = FakeVault::new(root.path(), []);
        if !active {
            vault.add(&id(42), "retired");
        }
        let result = run(root.path(), &mut vault);
        if active {
            assert_eq!(result.unwrap(), "ensured");
            assert_eq!(vault.puts(), [id(42)]);
            assert_eq!(vault.grants(), [id(42)]);
            assert!(!file.exists());
        } else {
            assert!(result.unwrap_err().contains(&id(42)));
            assert_eq!(vault.calls, [vec!["list", "--json"]]);
            assert!(!file.exists());
        }
    }
}

#[test]
fn state5_adopts_sole_active_key_including_one() {
    let root = ScratchDir::new("broca-state-5");
    let mut vault = FakeVault::new(root.path(), []);
    vault.add(&id(1), "active");
    vault.add(&id(11), "retired");
    assert_eq!(run(root.path(), &mut vault).unwrap(), "no change");
    assert_eq!(vault.grants(), [id(1)]);
    assert!(!root.join("broca.jsonc").exists());
    assert!(vault.puts().is_empty());
}

#[test]
fn multiple_active_keys_are_granted_without_minting_another() {
    let root = ScratchDir::new("broca-state-6");
    let mut vault = FakeVault::new(root.path(), []);
    vault.add(&id(2), "active");
    vault.add(&id(34), "active");
    assert_eq!(run(root.path(), &mut vault).unwrap(), "no change");
    assert_eq!(vault.grants(), [id(2), id(34)]);
    assert!(vault.puts().is_empty());
    assert!(!root.join("broca.jsonc").exists());
}

#[test]
fn setup_mints_and_grants_without_writing_key_setting() {
    let root = ScratchDir::new("broca-state-7");
    let mut vault = FakeVault::new(root.path(), []);
    vault.add(&id(2), "needs_reauth");
    assert_eq!(run(root.path(), &mut vault).unwrap(), "ensured");
    let puts = vault.puts();
    assert_eq!(puts.len(), 1);
    let key = parse_id(puts[0].strip_prefix(PREFIX).unwrap()).unwrap();
    assert!(key > 2);
    assert_eq!(vault.grants(), [id(key)]);
    assert!(!root.join("broca.jsonc").exists());
}

#[test]
fn key_step_does_not_read_or_repair_broca_settings() {
    for invalid in [
        "{",
        r#"{"session_storage":{"deployment_key_id":0}}"#,
        r#"{"session_storage":{"deployment_key_id":70000}}"#,
    ] {
        let root = ScratchDir::new("broca-invalid-settings");
        let path = root.join("broca.jsonc");
        fs::write(&path, invalid).unwrap();
        let mut vault = FakeVault::new(root.path(), []);
        let rows = inventory(root.path());
        assert!(observe(&path, &rows, None)
            .unwrap()
            .starts_with("reads the vault"));
        assert!(observe(&path, &rows, Some(&mut vault.auth()))
            .unwrap()
            .starts_with("found no active keys"));
        drop(rows);
        assert!(run(root.path(), &mut vault).is_ok());
        assert_eq!(fs::read_to_string(path).unwrap(), invalid);
    }
}

#[test]
fn invalid_listed_id_refuses_at_observation_or_after_deferred_read() {
    let root = ScratchDir::new("broca-invalid-record");
    let path = root.join("broca.jsonc");
    let mut vault = FakeVault::new(root.path(), []);
    vault.add("opaque:broca-deployment:70000", "active");
    let rows = inventory(root.path());
    assert!(observe(&path, &rows, None)
        .unwrap()
        .starts_with("reads the vault"));
    assert!(observe(&path, &rows, Some(&mut vault.auth()))
        .unwrap_err()
        .contains("70000"));
    drop(rows);
    assert!(run(root.path(), &mut vault).unwrap_err().contains("70000"));
    assert_eq!(
        vault.calls,
        [vec!["list", "--json"], vec!["list", "--json"]]
    );
    assert!(!path.exists());
}

#[test]
fn operator_file_naming_pending_id_is_left_unmanaged() {
    let root = ScratchDir::new("broca-operator-pending-settings");
    let path = root.join("broca.jsonc");
    let human = "{\n// written by the operator\n\"session_storage\":{\"deployment_key_id\":78}}\n";
    fs::write(&path, human).unwrap();
    save_intent(&mut inventory(root.path()), &path, 78).unwrap();
    let mut vault = FakeVault::new(root.path(), []);
    vault.add(&id(78), "active");
    assert_eq!(run(root.path(), &mut vault).unwrap(), "ensured");
    assert_eq!(fs::read_to_string(&path).unwrap(), human);
    let rows = inventory(root.path());
    assert!(!rows.owns_path("settings-receipt", &path));
    assert!(!rows.owns_path(INTENT, &path));
    assert!(vault.puts().is_empty());
}

#[test]
fn lost_put_active_is_adopted_but_refusal_without_record_stops() {
    for (put, succeeds) in [(Put::LostCreated, true), (Put::Refused, false)] {
        let root = ScratchDir::new("broca-put-reread");
        let mut vault = FakeVault::new(root.path(), [put]);
        let path = root.join("broca.jsonc");
        let result = run(root.path(), &mut vault);
        assert_eq!(result.is_ok(), succeeds);
        assert_eq!(vault.calls[2], ["list", "--json"]);
        assert!(!path.exists());
        assert_eq!(inventory(root.path()).owns_path(INTENT, &path), !succeeds);
        assert_eq!(vault.puts().len(), 1);
    }
}

#[test]
fn three_collisions_refuse_minting_and_drop_intent_before_next_run() {
    let root = ScratchDir::new("broca-three-collisions");
    let path = root.join("broca.jsonc");
    let mut vault = FakeVault::new(root.path(), [Put::Collision; 3]);
    assert!(run(root.path(), &mut vault)
        .unwrap_err()
        .contains("three Broca key collisions"));
    assert!(!inventory(root.path()).owns_path(INTENT, &path));
    let puts = vault.puts();
    assert_eq!(puts.len(), 3);
    assert_ne!(puts[0], puts[1]);
    assert_ne!(puts[1], puts[2]);
    assert_ne!(puts[0], puts[2]);
    assert!(vault.grants().is_empty());
    assert_eq!(run(root.path(), &mut vault).unwrap(), "no change");
    assert_eq!(vault.puts(), puts);
    assert_eq!(vault.grants().len(), 3);
    assert!(!path.exists());
}

#[cfg(feature = "test-support")]
#[test]
fn child_broca_checkpoint() {
    let Some(root) = std::env::var_os("CK_MODULE_STEP_TEST_ROOT").map(PathBuf::from) else {
        return;
    };
    let puts = if std::env::var_os("CK_MODULE_TEST_COLLISION").is_some() {
        vec![Put::Collision]
    } else {
        vec![]
    };
    let mut vault = FakeVault::new(&root, puts);
    run(&root, &mut vault).unwrap();
}

#[cfg(feature = "test-support")]
#[test]
fn process_restart_recovers_saved_broca_intent_at_each_put_and_grant_cut() {
    use crate::setup::apply::module_steps_tests::crash_at;
    for (cut, collision, before_puts, adds_put) in [
        ("broca-intent-saved", false, 0, true),
        ("broca-put-complete", false, 1, false),
        ("broca-grants-complete", false, 1, false),
        ("broca-put-refused", true, 1, false),
        ("broca-collision-intent-saved", true, 1, false),
        ("broca-put-complete", true, 2, false),
    ] {
        let root = ScratchDir::new("broca-process-cut");
        let file = root.join("broca.jsonc");
        let env = if collision {
            vec![("CK_MODULE_TEST_COLLISION", "1")]
        } else {
            vec![]
        };
        crash_at(
            root.path(),
            concat!(module_path!(), "::child_broca_checkpoint"),
            cut,
            &env,
        );
        let mut vault = FakeVault::new(root.path(), []);
        assert_eq!(vault.puts().len(), before_puts, "{cut}");
        assert!(!file.exists(), "{cut}");
        let rows = restart(root.path());
        assert!(!file.exists(), "{cut}");
        assert!(intent_key(&rows, &file).unwrap().is_some(), "{cut}");
        drop(rows);
        assert_eq!(run(root.path(), &mut vault).unwrap(), "ensured", "{cut}");
        assert_eq!(
            vault.puts().len(),
            before_puts + usize::from(adds_put),
            "{cut}"
        );
        assert!(!file.exists(), "{cut}");
        assert!(!vault.grants().is_empty(), "{cut}");
        assert!(!inventory(root.path()).owns_path(INTENT, &file), "{cut}");
    }
}
