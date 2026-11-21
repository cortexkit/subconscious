use super::*;
use std::cell::{Cell, RefCell};

enum ExecEvent {
    Uncommitted,
    Image(Option<subc_os::Observation>),
}

thread_local! {
    static EXEC_EVENTS: RefCell<Option<std::sync::mpsc::Sender<ExecEvent>>> = const { RefCell::new(None) };
}

pub(super) fn report_exec_state(committed: Option<bool>) {
    if committed == Some(false) {
        EXEC_EVENTS.with(|events| {
            if let Some(events) = events.borrow_mut().take() {
                let _ = events.send(ExecEvent::Uncommitted);
            }
        });
    }
}

#[test]
fn linux_spawn_observation_waits_for_a_paused_fork_to_exec() {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    let descriptor = std::fs::File::open("/dev/null").unwrap();
    let mut command = std::process::Command::new("/bin/sleep");
    command.arg("30");
    let (mut ready, mut release) =
        subc_os::fork_exec_test::pause_before_exec(&mut command, descriptor.as_raw_fd()).unwrap();
    let spawn = std::thread::spawn(move || command.spawn());
    let mut message = [0u8; 8];
    ready.read_exact(&mut message).unwrap();
    let pid = i32::from_ne_bytes(message[..4].try_into().unwrap()) as u32;
    let before = std::fs::read_to_string(format!("/proc/{pid}/stat"));
    let inherited = subc_os::Process::open(pid).unwrap().unwrap().observe();
    let (events, received) = std::sync::mpsc::channel();
    let observer = std::thread::spawn(move || {
        EXEC_EVENTS.with(|slot| *slot.borrow_mut() = Some(events.clone()));
        let image = observe_spawned_image(pid);
        let _ = events.send(ExecEvent::Image(image));
    });
    // Release only once the production observer has seen the uncommitted
    // exec, or has incorrectly returned the inherited image. Either way,
    // release and reap before asserting so a regression cannot leak a child.
    let first = received.recv_timeout(Duration::from_secs(5));
    release.write_all(&[1]).unwrap();
    let mut child = spawn.join().unwrap().unwrap();
    let after = match first {
        Ok(ExecEvent::Uncommitted) => match received.recv_timeout(Duration::from_secs(5)) {
            Ok(ExecEvent::Image(image)) => image,
            _ => None,
        },
        Ok(ExecEvent::Image(image)) => image,
        Err(_) => None,
    };
    observer.join().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();

    assert_eq!(linux_exec_committed(&before.unwrap()), Some(false));
    assert_eq!(
        inherited.unwrap().executable,
        subc_os::file_identity(&std::env::current_exe().unwrap())
    );
    assert_eq!(
        after.unwrap().executable,
        subc_os::file_identity(std::path::Path::new("/bin/sleep"))
    );
}

#[test]
fn spawned_image_is_not_sampled_until_linux_exec_commits() {
    let committed = Cell::new(false);
    let inherited = subc_os::Observation {
        start_time: 23,
        executable: Some(subc_os::FileIdentity {
            device: 29,
            inode: 31,
        }),
    };
    let module = subc_os::Observation {
        start_time: 23,
        executable: Some(subc_os::FileIdentity {
            device: 29,
            inode: 37,
        }),
    };
    let polls = Cell::new(0);
    let image = sample_after_linux_exec(
        || {
            polls.set(polls.get() + 1);
            // Hold the inherited image through the first poll, as in the
            // posix_spawn parent-wake window, then commit the module image.
            committed.set(polls.get() == 2);
            Some(committed.get())
        },
        || Some(if committed.get() { module } else { inherited }),
    );
    assert_eq!(image, Some(module));
    assert_eq!(polls.get(), 2);
}

#[test]
fn linux_exec_flag_is_read_from_flags_after_the_last_parenthesis() {
    assert_eq!(
        linux_exec_committed("17 (a ) b) R 1 2 3 4 5 4194368"),
        Some(false)
    );
    assert_eq!(
        linux_exec_committed("17 (a ) b) R 1 2 3 4 5 4194304"),
        Some(true)
    );
    assert_eq!(linux_exec_committed("17 (a) R 1 2 3 4 5 invalid"), None);
    assert_eq!(linux_exec_committed("17 (a) R"), None);
}

#[test]
fn unreadable_exec_state_never_samples_an_inherited_image() {
    assert_eq!(
        sample_after_linux_exec(
            || None,
            || panic!("must not sample before exec confirmation")
        ),
        None
    );
}

#[test]
fn uncommitted_exec_times_out_without_sampling_an_inherited_image() {
    assert_eq!(
        sample_after_linux_exec(
            || Some(false),
            || panic!("must not sample after an exec timeout")
        ),
        None
    );
}
