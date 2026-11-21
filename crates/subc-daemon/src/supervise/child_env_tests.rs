#[cfg(windows)]
use super::{
    clear_current_process_facts, observed_launch_nonce_source, ModuleState, SupervisorSnapshot,
};
#[cfg(windows)]
use std::sync::{Arc, OnceLock};
#[cfg(windows)]
#[test]
fn windows_wire_spawn_prepares_pipe_and_keeps_phase_one_env() {
    let spec = spec(Vec::new());
    let mut command = Command::new("unused");
    let handoff = apply_wire_spawn_args(&mut command, &spec, None, None)
        .unwrap()
        .unwrap();
    let name = command
        .as_std()
        .get_envs()
        .find(|(key, _)| *key == OsStr::new(subc_os::launch_nonce::LAUNCH_NONCE_PIPE_ENV))
        .and_then(|(_, value)| value)
        .unwrap();
    assert_eq!(name, OsStr::new(handoff.name()));
    assert!(handoff.name().starts_with(r"\\.\pipe\subc-launch-"));
    assert!(command.as_std().get_envs().any(|(key, value)| key
        == OsStr::new(subc_os::LAUNCH_NONCE_PIPE_FALLBACK_ENV)
        && value == Some(OsStr::new("env"))));
    assert!(command
        .as_std()
        .get_envs()
        .any(|(key, value)| key == OsStr::new(SUBC_LAUNCH_NONCE_ENV) && value.is_some()));
}

#[cfg(windows)]
#[test]
fn windows_source_reporting_tracks_consumption_and_resets_per_spawn() {
    let mut snapshot = SupervisorSnapshot::new(ModuleState::Running, true);
    snapshot.process_alive = true;
    snapshot.spawned_protocol = Some(ModuleProtocol::Subc);
    assert_eq!(
        observed_launch_nonce_source(&snapshot).as_deref(),
        Some("env")
    );
    let consumed = Arc::new(OnceLock::new());
    snapshot.pipe_consumed = Some(consumed.clone());
    assert_eq!(
        observed_launch_nonce_source(&snapshot).as_deref(),
        Some("env")
    );
    consumed.set(()).unwrap();
    assert_eq!(
        observed_launch_nonce_source(&snapshot).as_deref(),
        Some("pipe")
    );
    snapshot.pipe_consumed = Some(Arc::new(OnceLock::new()));
    assert_eq!(
        observed_launch_nonce_source(&snapshot).as_deref(),
        Some("env")
    );
    clear_current_process_facts(&mut snapshot);
    assert_eq!(observed_launch_nonce_source(&snapshot), None);
}
use super::{
    apply_child_env, apply_spawn_role, apply_wire_spawn_args, LaunchSpecDigest, ModuleProtocol,
    ModuleSpec, SpawnRole, SupervisorHandle, SPAWN_ROLE_SWAP_CANDIDATE, SUBC_ARG,
    SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV, SUBC_SPAWN_ROLE_ENV,
};
use std::{ffi::OsStr, path::PathBuf};
use tokio::process::Command;

fn spec(env: Vec<(String, String)>) -> ModuleSpec {
    ModuleSpec {
        script: None,
        module_id: "env-plan".to_string(),
        program: PathBuf::from("/nonexistent"),
        args: Vec::new(),
        env,
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    }
}

#[test]
fn configured_launch_spec_digest_is_canonical_and_excludes_injected_inputs() {
    let base = spec(vec![("A".into(), "B=C".into()), ("Z".into(), "v".into())]);
    let digest = LaunchSpecDigest::from_spec(&base);
    let mut reordered = base.clone();
    reordered.env.reverse();
    reordered.module_id = "different-id".into();
    reordered.reserved = true;
    reordered.overlap = Default::default();
    assert_eq!(digest, LaunchSpecDigest::from_spec(&reordered));
    let mut delimiter_collision = base.clone();
    delimiter_collision.env[0] = ("A=B".into(), "C".into());
    assert_ne!(digest, LaunchSpecDigest::from_spec(&delimiter_collision));
    let mut changed = base.clone();
    changed.protocol = ModuleProtocol::None;
    assert_ne!(digest, LaunchSpecDigest::from_spec(&changed));
    changed = base.clone();
    changed.args.push("--flag".into());
    assert_ne!(digest, LaunchSpecDigest::from_spec(&changed));
    changed = base.clone();
    changed.program = PathBuf::from("/other");
    assert_ne!(digest, LaunchSpecDigest::from_spec(&changed));
    changed = base.clone();
    changed.env[1].1 = "other".into();
    assert_ne!(digest, LaunchSpecDigest::from_spec(&changed));
    assert!(!format!("{digest:?}").contains("B=C"));
    assert!(!format!("{digest:?}").contains("[u8"));
}

#[cfg(unix)]
#[test]
fn native_program_bytes_do_not_collapse_through_lossy_utf8() {
    use std::os::unix::ffi::OsStringExt;
    let mut left = spec(Vec::new());
    left.program = std::ffi::OsString::from_vec(vec![b'/', 0xff]).into();
    let mut right = left.clone();
    right.program = std::ffi::OsString::from_vec(vec![b'/', 0xfe]).into();
    assert_eq!(
        left.program.to_string_lossy(),
        right.program.to_string_lossy()
    );
    assert_ne!(
        LaunchSpecDigest::from_spec(&left),
        LaunchSpecDigest::from_spec(&right)
    );
}

/// Ambient `CK_LOG` is REMOVED for an unconfigured module, and a configured
/// one still gets its own.
///
/// This is the narrow goal `env_clear()` was reached for, and the reason the
/// fix is `env_remove` rather than deleting the line: an operator's ambient
/// filter silently becoming an unconfigured module's log level is a real
/// defect, just a much smaller one than clearing the environment.
///
/// Asserted on the command plan rather than a spawned child because proving
/// the ABSENCE of an inherited variable needs the parent's environment
/// mutated, and `forbid(unsafe_code)` refuses that. `get_envs()` reports a
/// removal as `(key, None)`, which is exactly the distinction wanted: not
/// "absent because nobody set it" but "explicitly unset for the child".
#[test]
fn ambient_ck_log_is_removed_and_a_configured_one_survives() {
    let mut command = Command::new("/nonexistent");
    apply_child_env(&mut command, &spec(Vec::new()));
    let removed = command
        .as_std()
        .get_envs()
        .any(|(key, value)| key == OsStr::new("CK_LOG") && value.is_none());
    assert!(
        removed,
        "ambient CK_LOG must be explicitly removed for an unconfigured module"
    );

    let mut configured = Command::new("/nonexistent");
    apply_child_env(
        &mut configured,
        &spec(vec![("CK_LOG".to_string(), "debug".to_string())]),
    );
    let effective = configured
        .as_std()
        .get_envs()
        .filter(|(key, _)| *key == OsStr::new("CK_LOG"))
        .last()
        .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()));
    assert_eq!(
        effective,
        Some(Some("debug".to_string())),
        "a module's configured CK_LOG must survive the ambient removal"
    );
}

/// A `protocol: "none"` spawn carries NO `--subc` argument and NO launch
/// nonce; a subc-wire spawn carries both. Asserted on the command plan for
/// the same reason as the CK_LOG test above.
///
/// The argument is the load-bearing half: a stock binary exits on an
/// unknown flag before it listens, so with `--subc` appended the mode
/// could not supervise the one process it exists for. Found by the first
/// conformance run (nats-server: `flag provided but not defined: -subc`).
#[test]
fn protocol_none_spawn_carries_no_subc_argument_and_no_nonce() {
    let connection_file = std::path::Path::new("/run/subc-connection.json");
    let handle = SupervisorHandle::new();

    let mut none_spec = spec(Vec::new());
    none_spec.protocol = ModuleProtocol::None;
    let mut none = Command::new("/nonexistent");
    let none_handoff =
        apply_wire_spawn_args(&mut none, &none_spec, Some(connection_file), Some(&handle))
            .expect("protocol-none spawn args apply");
    assert!(
        none_handoff.is_none(),
        "protocol:none spawn must not receive a nonce descriptor"
    );
    #[cfg(windows)]
    assert!(!none.as_std().get_envs().any(|(key, value)| key
        == OsStr::new(subc_os::LAUNCH_NONCE_PIPE_FALLBACK_ENV)
        && value.is_some()));
    assert!(
        !none
            .as_std()
            .get_envs()
            .any(|(key, value)| key == OsStr::new(subc_os::LAUNCH_NONCE_FD_ENV) && value.is_some()),
        "protocol:none spawn must not name a nonce descriptor"
    );
    let none_args: Vec<String> = none
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        !none_args.iter().any(|a| a == SUBC_ARG),
        "protocol:none argv must not carry --subc; got {none_args:?}"
    );
    let none_has_nonce = none
        .as_std()
        .get_envs()
        .any(|(key, value)| key == OsStr::new(SUBC_LAUNCH_NONCE_ENV) && value.is_some());
    assert!(
        !none_has_nonce,
        "protocol:none spawn must not receive a launch nonce"
    );
    let none_has_module_id = none
        .as_std()
        .get_envs()
        .any(|(key, value)| key == OsStr::new(SUBC_MODULE_ID_ENV) && value.is_some());
    assert!(
        none_has_module_id,
        "SUBC_MODULE_ID is inert and stays on every path"
    );
    assert!(
        handle.spawn_nonce(&none_spec.module_id).is_none(),
        "no nonce record for a process that will never present one"
    );

    // Control: the subc-wire path is unchanged by the branch above.
    let wire_spec = spec(Vec::new());
    let mut wire = Command::new("/nonexistent");
    let wire_handoff =
        apply_wire_spawn_args(&mut wire, &wire_spec, Some(connection_file), Some(&handle))
            .expect("subc-wire spawn args apply");
    let wire_fd_env = wire
        .as_std()
        .get_envs()
        .find(|(key, _)| *key == OsStr::new(subc_os::LAUNCH_NONCE_FD_ENV))
        .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()));
    #[cfg(unix)]
    assert_eq!(
        wire_fd_env,
        Some(wire_handoff.expect("a descriptor handoff").fd_env_value()),
        "a subc-wire spawn names the pipe it will receive at descriptor 3"
    );
    #[cfg(windows)]
    assert!(wire_handoff.is_some() && wire_fd_env.is_none());
    #[cfg(not(any(unix, windows)))]
    assert!(wire_handoff.is_none() && wire_fd_env.is_none());
    let wire_args: Vec<String> = wire
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        wire_args,
        vec![
            SUBC_ARG.to_string(),
            connection_file.to_string_lossy().into_owned()
        ],
        "a subc-wire spawn still carries --subc <path>"
    );
    assert_eq!(
        wire.as_std()
            .get_envs()
            .any(|(key, value)| key == OsStr::new(SUBC_LAUNCH_NONCE_ENV) && value.is_some()),
        !cfg!(unix),
        "only Windows supplies the environment nonce"
    );
    assert!(handle.spawn_nonce(&wire_spec.module_id).is_some());
}

/// A plain spawn EXPLICITLY REMOVES the spawn role, even when the module's
/// spec tries to set it; only a swap candidate carries it.
///
/// "Set it only on candidates" is not enough, because spawn applies the
/// spec's env verbatim and the daemon's own environment is inherited: either
/// could hand a plain restart the swap role, and a module reading it would
/// warm on its long swap budget while callers wait. Asserted as an explicit
/// removal (`(key, None)`), not mere absence, for the reason the `CK_LOG`
/// test above gives.
#[test]
fn plain_spawn_removes_the_spawn_role_even_when_the_spec_sets_it() {
    let role = |command: &Command| {
        command
            .as_std()
            .get_envs()
            .filter(|(key, _)| *key == OsStr::new(SUBC_SPAWN_ROLE_ENV))
            .last()
            .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    };
    let forged = spec(vec![(
        SUBC_SPAWN_ROLE_ENV.to_string(),
        SPAWN_ROLE_SWAP_CANDIDATE.to_string(),
    )]);

    let mut plain = Command::new("/nonexistent");
    apply_child_env(&mut plain, &forged);
    apply_spawn_role(&mut plain, SpawnRole::Plain);
    assert_eq!(
        role(&plain),
        Some(None),
        "a plain spawn must remove SUBC_SPAWN_ROLE, whatever the spec says"
    );

    let mut candidate = Command::new("/nonexistent");
    apply_child_env(&mut candidate, &spec(Vec::new()));
    apply_spawn_role(&mut candidate, SpawnRole::SwapCandidate);
    assert_eq!(
        role(&candidate),
        Some(Some(SPAWN_ROLE_SWAP_CANDIDATE.to_string()))
    );
}

/// Daemon-private capture retention keys never reach the child.
///
/// cortexkit-log exposes retention as a Rust struct with no environment
/// names, so these entries are supervisor metadata. Passing them through
/// would invent a public child-process contract by accident.
#[test]
fn daemon_private_capture_keys_are_not_passed_to_the_child() {
    let mut command = Command::new("/nonexistent");
    apply_child_env(
        &mut command,
        &spec(vec![
            (super::CAPTURE_KEEP_ENV.to_string(), "5".to_string()),
            ("KEPT".to_string(), "yes".to_string()),
        ]),
    );
    let keys: Vec<String> = command
        .as_std()
        .get_envs()
        .filter(|(_, value)| value.is_some())
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect();
    assert!(keys.contains(&"KEPT".to_string()), "got {keys:?}");
    assert!(
        !keys.contains(&super::CAPTURE_KEEP_ENV.to_string()),
        "daemon-private capture key leaked to the child: {keys:?}"
    );
}
