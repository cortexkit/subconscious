#![allow(unsafe_code)]

use super::*;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::{io::AsRawFd, io::FromRawFd, process::CommandExt},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct HeldChild(Child);

impl Drop for HeldChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "re-exec fixture holding an inherited peer socket"]
fn inherited_socket_child() {
    let fd = std::env::var("SUBC_TEST_PEER_FD")
        .unwrap()
        .parse::<i32>()
        .unwrap();
    // SAFETY: the parent preserves this socket across exec and transfers the
    // child's descriptor to this stream exactly once.
    let mut socket = unsafe { TcpStream::from_raw_fd(fd) };
    socket.write_all(b"R").unwrap();
    let mut release = [0];
    assert_eq!(std::io::stdin().read(&mut release).unwrap(), 0);
}

#[test]
fn peer_socket_shared_by_two_processes_is_unresolved() {
    // Create the connecting socket in an isolated re-exec process: unrelated
    // tests' forks cannot inherit it or disturb the unique-holder control.
    if std::env::var_os("SUBC_TEST_PEER_OWNER").is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "socket_peer::shared_tests::peer_socket_shared_by_two_processes_is_unresolved",
                "--nocapture",
            ])
            .env("SUBC_TEST_PEER_OWNER", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "socket owner test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut accepted, _) = listener.accept().unwrap();
    let endpoints = (
        accepted.local_addr().unwrap(),
        accepted.peer_addr().unwrap(),
    );
    let duplicate = socket.try_clone().unwrap();
    let caller = loopback_socket_peer(endpoints.0, endpoints.1).unwrap();
    assert_eq!(caller.pid, std::process::id(), "dup is still one process");

    let fd = socket.as_raw_fd();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "socket_peer::shared_tests::inherited_socket_child",
            "--ignored",
        ])
        .env("SUBC_TEST_PEER_FD", fd.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    // SAFETY: after fork this closure only calls async-signal-safe fcntl on
    // a live descriptor. Only the child's copy loses close-on-exec.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = HeldChild(command.spawn().unwrap());
    accepted
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut ready = [0];
    accepted.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"R", "child holds the inherited socket after exec");
    assert_eq!(
        loopback_socket_peer(endpoints.0, endpoints.1),
        None,
        "two live processes hold the connecting socket"
    );

    drop(child);
    assert_eq!(loopback_socket_peer(endpoints.0, endpoints.1), Some(caller));
    drop(duplicate);
}
