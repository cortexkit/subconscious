use super::*;
use std::{io::Write, path::Path};
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
    let runtime = runtime();
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
async fn macos_suspended_shebang_has_no_wildcard_admission() {
    let probe = Probe::new();
    let marker = probe.root.join("script-marker");
    let script = probe.root.join("script");
    let mut file = std::fs::File::create(&script).unwrap();
    writeln!(
        file,
        "#!/bin/sh\nprintf started > '{}'\nexec /bin/sleep 30",
        marker.display()
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut spec = probe.spec(&marker);
    spec.program = script;
    spec.args.clear();
    let runtime = runtime();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let refused = match spawn_and_mark_running(&spec, &runtime, &snapshot) {
        Err(error) => error
            .to_string()
            .contains("scripts are not executable images"),
        Ok(mut child) => {
            child.confirm_privacy_exec().await;
            let refused = child.spawn_failure.is_some();
            let _ = child.start_kill();
            child.wait().await.unwrap();
            child.release_roster();
            refused
        }
    };
    assert!(
        refused,
        "a shebang script was admitted through the interpreter wildcard"
    );
    assert!(!marker.exists());
}

#[tokio::test]
async fn macos_suspended_cdhash_mismatch_never_runs_marker_target() {
    let probe = Probe::new();
    let marker = probe.root.join("hash-refused-marker");
    let runtime = runtime();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let mut child = spawn_and_mark_running(&probe.spec(&marker), &runtime, &snapshot).unwrap();
    let other = Path::new("/bin/sleep");
    child.privacy_exec.as_mut().unwrap().signature = Arc::new(
        subc_os::privacy_identity::SuspendedSignature::capture(
            &std::fs::File::open(other).unwrap(),
            other,
        )
        .unwrap(),
    );
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
    let runtime = runtime();
    // The second plain launch exercises the same slot after a reaped leader;
    // both swap slots and the async preparation entry use the same admission.
    for (index, role, alternate, carry) in [
        (0, SpawnRole::Plain, false, false),
        (1, SpawnRole::Plain, false, false),
        (2, SpawnRole::SwapCandidate, true, false),
        (3, SpawnRole::SwapCandidate, false, false),
        (4, SpawnRole::Plain, false, true),
    ] {
        let marker = probe.root.join(format!("accepted-{index}"));
        let mut spec = probe.spec(&marker);
        spec.carry_containment = carry;
        let mut child = spawn_child_in_slot_async(&spec, &runtime, role, alternate)
            .await
            .unwrap();
        assert!(!marker.exists());
        assert!(child.report_ready.get().is_none());
        child.confirm_privacy_exec().await;
        assert_eq!(child.spawn_failure, None);
        assert!(child.report_ready.get().is_some());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), b"started");
        child.start_kill().unwrap();
        child.wait().await.unwrap();
        child.release_roster();
    }
}
