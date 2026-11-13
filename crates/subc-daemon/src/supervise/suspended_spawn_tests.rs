use super::*;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Probe {
    root: PathBuf,
    program: PathBuf,
}

impl Probe {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::current_dir()
            .unwrap()
            .join("target/scratch")
            .join(format!(
                "suspended-probe-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("probe.c");
        std::fs::write(&source, b"#include <fcntl.h>\n#include <unistd.h>\nint main(int argc, char **argv) { if (argc != 2) return 2; int fd = open(argv[1], O_WRONLY|O_CREAT|O_EXCL, 0600); if (fd < 0) return 3; if (write(fd, \"started\", 7) != 7) return 4; close(fd); for (;;) pause(); }\n").unwrap();
        let program = root.join("probe");
        assert!(std::process::Command::new("/usr/bin/cc")
            .arg(&source)
            .arg("-o")
            .arg(&program)
            .status()
            .unwrap()
            .success());
        assert!(std::process::Command::new("/usr/bin/codesign")
            .args([
                "--force",
                "--sign",
                "-",
                "--identifier",
                "org.cortexkit.suspended-probe"
            ])
            .arg(&program)
            .status()
            .unwrap()
            .success());
        Self { root, program }
    }

    fn spec(&self, marker: &Path) -> ModuleSpec {
        ModuleSpec {
            module_id: "suspended-probe".into(),
            program: self.program.clone(),
            args: vec![marker.to_string_lossy().into_owned()],
            env: vec![],
            reserved: false,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::None,
            overlap: ModuleOverlap::Exclusive,
            carry_containment: false,
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn runtime() -> SupervisorRuntimeConfig {
    Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
        .runtime_config()
}

pub(super) fn gated_owner(
    root: &Path,
    spec: &ModuleSpec,
    stamped: bool,
) -> Arc<crate::config_gate::runtime::RuntimeOwner> {
    use crate::config_gate::{
        capture::ConfigCapturer, runtime::RuntimeOwner, ApprovalFacts, ApprovalReply, PlanKind,
        PresenceDecision,
    };
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = root.join(format!(
        "gate-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(root.join("store")).unwrap();
    let daemon_path = root.join("subc.jsonc");
    std::fs::write(&daemon_path, serde_json::to_vec(&serde_json::json!({
        "version":1, "modules":{spec.module_id.clone():{
            "program":spec.program, "args":spec.args, "env":spec.env.iter().cloned().collect::<std::collections::BTreeMap<_,_>>(),
            "protocol":"none", "carry_containment":spec.carry_containment
        }}
    })).unwrap()).unwrap();
    let owner = Arc::new(RuntimeOwner::new(
        "suspended-test".into(),
        crate::config_store::ConfigStore::open_digest_only_for_tests(
            crate::config_store::StoreRoot::from_service_definition(root.join("store")).unwrap(),
        ),
        ConfigCapturer {
            daemon_path,
            config_home: root,
            projects: Arc::new(crate::config_gate::capture::UnavailableProjectRegistry),
        },
    ));
    if stamped {
        let prepared = owner.prepare(PlanKind::Stamp).unwrap();
        let plan = owner
            .finish_plan(prepared, crate::config_gate::runtime::daemon_review)
            .unwrap();
        let facts = ApprovalFacts {
            connection: 7,
            owner_uid: true,
            desktop_app: true,
            attested_operator_app: true,
            os_prompt_available: true,
            phone: None,
            caller: crate::caller_identity::CallerIdentity::default(),
        };
        owner
            .desktop_view_delivered(crate::ConnectionId::new(7), &plan)
            .unwrap();
        let challenge = owner.begin_approval(&plan.digest, &facts).unwrap();
        owner
            .finish_approval(
                challenge.attempt(),
                &facts,
                ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
                1,
            )
            .unwrap();
    }
    // Suspended verification applies only to gated launches. These tests need
    // real approved program authority, not just a configured trampoline.
    owner
}

fn gated_runtime(probe: &Probe, spec: &ModuleSpec, stamped: bool) -> SupervisorRuntimeConfig {
    Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
        .with_config_runtime(gated_owner(&probe.root, spec, stamped))
        .runtime_config()
}

#[tokio::test]
async fn macos_ungated_script_starts_without_a_code_directory() {
    use std::os::unix::fs::PermissionsExt;
    let probe = Probe::new();
    let marker = probe.root.join("ungated-script-marker");
    let script = probe.root.join("ungated-script");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf started > '{}'\nexec /bin/sleep 30\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut spec = probe.spec(&marker);
    spec.program = script.clone();
    spec.args.clear();
    for direct_interpreter in [false, true] {
        if direct_interpreter {
            spec.program = "/bin/sh".into();
            spec.args = vec![script.to_string_lossy().into_owned()];
        }
        for runtime in [runtime(), gated_runtime(&probe, &spec, false)] {
            let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
            let mut child = spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap();
            child.confirm_privacy_exec().await;
            assert_eq!(child.spawn_failure, None);
            tokio::time::timeout(Duration::from_secs(5), async {
                while std::fs::read(&marker).ok().as_deref() != Some(b"started") {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(std::fs::read(&marker).unwrap(), b"started");
            assert!(child.report_ready.get().is_some());
            child.start_kill().unwrap();
            child.wait().await.unwrap();
            child.release_roster();
            std::fs::remove_file(&marker).unwrap();
        }
    }
}

#[tokio::test]
async fn macos_suspended_refusal_never_runs_marker_target() {
    let probe = Probe::new();
    let marker = probe.root.join("refused-marker");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut spec = probe.spec(&marker);
    spec.env.push((
        "SUBC_TEST_BEFORE_EXEC_CONFIRM".into(),
        listener.local_addr().unwrap().to_string(),
    ));
    let runtime = gated_runtime(&probe, &spec, true);
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let mut child = spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap();
    let task = tokio::spawn(async move {
        child.confirm_privacy_exec().await;
        child
    });
    let (mut barrier, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut ready = [0; 5];
    barrier.read_exact(&mut ready).await.unwrap();
    assert_eq!(ready[0], b'R');
    // Hold admission after exec EOF. A running target has ample opportunity to
    // write before the injected refusal, whereas a suspended target cannot.
    tokio::time::sleep(Duration::from_millis(250)).await;
    barrier.write_all(b"F").await.unwrap();
    let mut child = task.await.unwrap();
    assert!(child
        .spawn_failure
        .as_deref()
        .is_some_and(|cause| cause.contains("injected late exec refusal")));
    child.wait().await.unwrap();
    child.release_roster();
    assert!(
        !marker.exists(),
        "target ran before launch verification refused it"
    );
    assert!(child.report_ready.get().is_none());
}

#[tokio::test]
async fn macos_gated_scripts_are_refused_before_interpreter_substitution() {
    let probe = Probe::new();
    use std::os::unix::fs::PermissionsExt;
    for replace_path in [false, true] {
        let marker = probe.root.join(format!("script-marker-{replace_path}"));
        let script = probe.root.join(format!("approved-script-{replace_path}"));
        let other = probe.root.join(format!("unapproved-script-{replace_path}"));
        std::fs::write(&script, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut spec = probe.spec(&marker);
        spec.program = script.clone();
        spec.args.clear();
        let owner = gated_owner(&probe.root, &spec, false);
        // Refuse the script before approval of its interpreter image can
        // grant launch authority, even when the script has not changed.
        let error = owner
            .prepare(crate::config_gate::PlanKind::Stamp)
            .err()
            .unwrap();
        assert_eq!(error.code, "program_refused");
        assert!(error.detail.contains("compiled program"));
        let unapproved = format!(
            "#!/bin/sh\nprintf started > '{}'\nexec /bin/sleep 30\n",
            marker.display()
        );
        if replace_path {
            // Both scripts use the same interpreter. Substituting a second
            // script at the configured path must not make it admissible.
            std::fs::write(&other, unapproved).unwrap();
            std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::remove_file(&script).unwrap();
            std::os::unix::fs::symlink(&other, &script).unwrap();
        } else {
            std::fs::write(&script, unapproved).unwrap();
        }
        let error = owner
            .prepare(crate::config_gate::PlanKind::Stamp)
            .err()
            .unwrap();
        assert_eq!(error.code, "program_refused", "replace_path={replace_path}");
        assert!(!marker.exists());
    }
}

#[tokio::test]
async fn macos_suspended_cdhash_mismatch_never_runs_marker_target() {
    let probe = Probe::new();
    let marker = probe.root.join("hash-refused-marker");
    let runtime = gated_runtime(&probe, &probe.spec(&marker), true);
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let mut child = spawn_and_mark_running(&probe.spec(&marker), &runtime, &snapshot).unwrap();
    let other = Path::new("/bin/sleep");
    child.privacy_exec.as_mut().unwrap().signature = Some(Arc::new(
        subc_os::privacy_identity::SuspendedSignature::capture(
            &std::fs::File::open(other).unwrap(),
            other,
        )
        .unwrap(),
    ));
    child.confirm_privacy_exec().await;
    assert!(
        child
            .spawn_failure
            .as_deref()
            .is_some_and(|cause| cause.contains("cdhash mismatch")),
        "{:?}",
        child.spawn_failure
    );
    child.wait().await.unwrap();
    child.release_roster();
    assert!(!marker.exists());
    assert!(child.report_ready.get().is_none());
}

#[tokio::test]
async fn macos_suspended_plain_restart_and_candidate_resume_only_after_verification() {
    let probe = Probe::new();
    let marker = probe.root.join("accepted");
    let runtime = gated_runtime(&probe, &probe.spec(&marker), true);
    // Reuse the plain slot after reaping, then both swap slots. A carried
    // launch needs its own approval because containment is part of the spec.
    for (role, alternate, carry) in [
        (SpawnRole::Plain, false, false),
        (SpawnRole::Plain, false, false),
        (SpawnRole::SwapCandidate, true, false),
        (SpawnRole::SwapCandidate, false, false),
        (SpawnRole::Plain, false, true),
    ] {
        let mut spec = probe.spec(&marker);
        spec.carry_containment = carry;
        let carried_runtime = carry.then(|| gated_runtime(&probe, &spec, true));
        let runtime = carried_runtime.as_ref().unwrap_or(&runtime);
        let mut child = spawn_child_in_slot_async(&spec, runtime, role, alternate)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !subc_os::privacy_identity::SuspendedSignature::is_suspended(child.pid).unwrap() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("approved target must be stopped before verification");
        assert!(!marker.exists());
        assert!(child.report_ready.get().is_none());
        child.confirm_privacy_exec().await;
        assert_eq!(child.spawn_failure, None);
        assert!(child.report_ready.get().is_some());
        tokio::time::timeout(Duration::from_secs(5), async {
            while std::fs::read(&marker).ok().as_deref() != Some(b"started") {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), b"started");
        child.start_kill().unwrap();
        child.wait().await.unwrap();
        child.release_roster();
        std::fs::remove_file(&marker).unwrap();
    }
}
