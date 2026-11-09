#![cfg(target_os = "macos")]

use std::{
    io::{BufRead, BufReader},
    os::unix::process::CommandExt,
    process::{Child, Stdio},
    time::{Duration, Instant},
};
use subc_os::privacy_identity::DisclaimedCommand;

const FIXTURE: &str = env!("CARGO_BIN_EXE_subc-os-privacy-fixture");

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Each handle is a direct child, retained until this kill and reap.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(group: i32, mode: &str) -> (OwnedChild, u32) {
    let fixture = std::path::Path::new(FIXTURE).canonicalize().unwrap();
    assert!(
        !fixture.starts_with("/Volumes"),
        "native macOS privacy fixtures must run from an internal-drive target directory; a removable-volume launch can wait for a privacy prompt"
    );
    let mut builder = DisclaimedCommand::new(FIXTURE, FIXTURE);
    builder
        .arg(mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let (mut command, confirmation) = builder.into_command().unwrap();
    command.process_group(group);
    let mut child = OwnedChild(command.spawn().unwrap());
    drop(command);
    confirmation
        .confirm(Instant::now() + Duration::from_secs(5))
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    (child, line.trim().parse().unwrap())
}

fn exit_without_reaping(child: &mut OwnedChild) {
    drop(child.0.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = subc_os::child_exit::observe(child.0.id()).unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "owned child did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn carried_p1_pin_across_two_replacements_and_group_escape() {
    use std::os::unix::process::ExitStatusExt;

    let (mut sentinel, sentinel_group) = spawn(0, "group-wait");
    assert_eq!(sentinel_group, sentinel.0.id());
    let (mut pin, pgid) = spawn(0, "group-wait");
    assert_eq!(
        pgid,
        pin.0.id(),
        "the trampoline must preserve group creation"
    );
    let (mut holder, holder_group) = spawn(pgid as i32, "group-wait");
    assert_eq!(
        holder_group, pgid,
        "the trampoline must preserve a joined group"
    );
    exit_without_reaping(&mut pin);

    // Each replacement joins after the original leader exited, while holders
    // keep the group live. The original leader remains waitable, unreaped,
    // across both replacements.
    for _ in 0..2 {
        let (mut replacement, joined) = spawn(pgid as i32, "group-wait");
        assert_eq!(joined, pgid);
        exit_without_reaping(&mut replacement);
        subc_os::fork_exec_test::wait_for_child_exit_without_reaping(pin.0.id()).unwrap();
        assert!(replacement.0.wait().unwrap().success());
    }

    let (mut escaped, escaped_group) = spawn(pgid as i32, "escape-group");
    assert_eq!(escaped_group, escaped.0.id());
    assert_ne!(escaped_group, pgid);
    let group = rustix::process::Pid::from_raw(pgid as i32).unwrap();
    rustix::process::kill_process_group(group, rustix::process::Signal::TERM)
        .expect("a live holder in the pinned group must remain signalable");
    subc_os::fork_exec_test::wait_for_child_exit_without_reaping(holder.0.id()).unwrap();
    assert_eq!(holder.0.wait().unwrap().signal(), Some(15));
    assert!(sentinel.0.try_wait().unwrap().is_none());
    assert!(escaped.0.try_wait().unwrap().is_none());

    // The original leader is still our unreaped child: no other process can
    // acquire its pid. The macOS kernel (XNU) reports EPERM for a zombie-only
    // group; ESRCH is also an empty-group result. Neither may redirect the
    // signal to an escaped process or another group. The live-holder TERM above
    // distinguishes emptiness from refusal.
    let empty = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    assert!(
        matches!(
            empty,
            Err(rustix::io::Errno::SRCH | rustix::io::Errno::PERM)
        ),
        "{empty:?}"
    );
    assert!(sentinel.0.try_wait().unwrap().is_none());
    assert!(escaped.0.try_wait().unwrap().is_none());
    assert!(pin.0.wait().unwrap().success());
    exit_without_reaping(&mut escaped);
    assert!(escaped.0.wait().unwrap().success());
    exit_without_reaping(&mut sentinel);
    assert!(sentinel.0.wait().unwrap().success());
}
