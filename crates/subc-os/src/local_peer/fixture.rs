//! Native signed socket peer for macOS tests. The fixture's stdin selects a
//! new connection, allowing a daemon restart without restarting its peer.

use super::{capture_unix_peer, CapturedPeer};
use std::{
    io::Write,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

#[non_exhaustive]
pub struct SignedPeerFixture {
    child: Child,
    directory: PathBuf,
}

fn run(command: &mut Command) {
    let output = command.output().expect("fixture tool starts");
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

impl SignedPeerFixture {
    /// Compile a tiny peer with a sealed, embedded plist, then sign it ad hoc.
    pub fn new(
        directory: &Path,
        identifier: &str,
        version: Option<&str>,
        runtime: bool,
        entitlement: Option<(&str, bool)>,
    ) -> Self {
        std::fs::create_dir_all(directory).unwrap();
        let directory = directory.canonicalize().unwrap();
        let source = directory.join("peer.c");
        let plist = directory.join("Info.plist");
        let binary = directory.join("peer");
        std::fs::write(&source, include_str!("fixture.c")).unwrap();
        std::fs::write(&plist, format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{identifier}</string>{}</dict></plist>",
            version.map(|version| format!("<key>CFBundleShortVersionString</key><string>{version}</string>")).unwrap_or_default())).unwrap();
        run(Command::new("cc")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .arg("-Wl,-sectcreate,__TEXT,__info_plist")
            .arg(format!("-Wl,{}", plist.display())));
        let mut sign = Command::new("/usr/bin/codesign");
        sign.args([
            "--force",
            "--sign",
            "-",
            "--timestamp=none",
            "--identifier",
            identifier,
        ]);
        if runtime {
            sign.args(["--options", "runtime"]);
        }
        if let Some((entitlement, value)) = entitlement {
            let entitlements = directory.join("entitlements.plist");
            std::fs::write(&entitlements, format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>{entitlement}</key><{value}/></dict></plist>")).unwrap();
            sign.arg("--entitlements").arg(entitlements);
        }
        run(sign.arg(&binary));
        let child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        Self { child, directory }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Accept a fresh connection from the same still-running signed process.
    pub fn connect(&mut self) -> (UnixStream, CapturedPeer) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // The short alias keeps sockaddr_un within its limit, while the socket
        // itself lives on the fixture's local filesystem, not a mounted checkout.
        std::fs::create_dir_all("target/scratch").unwrap();
        let alias = format!(
            "target/scratch/code-peer-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        std::os::unix::fs::symlink(&self.directory, &alias).unwrap();
        let path = format!("{alias}/s");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        writeln!(self.child.stdin.as_mut().unwrap(), "{path}").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        self.child.try_wait().unwrap().is_none(),
                        "fixture exited before connect"
                    );
                    assert!(Instant::now() < deadline, "fixture connect timed out");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept fixture: {error}"),
            }
        };
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(alias).unwrap();
        let peer = capture_unix_peer(&stream);
        (stream, peer)
    }
}

impl Drop for SignedPeerFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
