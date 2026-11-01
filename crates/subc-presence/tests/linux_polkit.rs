#![cfg(target_os = "linux")]

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

/// The subprocess is the unprivileged subject. It waits until its agent has
/// registered, and never authenticates in the root test runner's identity.
#[test]
fn polkit_child() {
    if std::env::var_os("SUBC_POLKIT_CHILD").is_none() {
        return;
    }
    println!("CHILD_PID:{}", std::process::id());
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "go");
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok(handle) = receive.recv() {
            let handle: std::sync::Arc<dyn subc_presence::Withdraw> = handle;
            for line in std::io::stdin().lock().lines() {
                if line.unwrap() == "withdraw" {
                    handle.withdraw();
                    break;
                }
            }
        }
    });
    let outcome = subc_presence::prompt(
        "approve a request from polkit-test: literal $(x) & <write> (requested by a local program)",
        Box::new(move |handle| {
            send.send(handle).unwrap();
        }),
    );
    println!("OUTCOME:{outcome:?}");
}

#[test]
fn real_polkit_password_text_no_agent_and_withdraw() {
    let required = std::env::var_os("SUBC_REQUIRE_POLKIT_TEST").is_some();
    let ready =
        rustix::process::getuid().is_root() && std::env::var_os("SUBC_POLKIT_TEST_USER").is_some();
    if !ready {
        assert!(
            !required,
            "real polkit test requires root and SUBC_POLKIT_TEST_USER"
        );
        eprintln!(
            "SKIP real polkit: requires root, test user, polkitd, pkttyagent and python3-dbus"
        );
        return;
    }
    let uid = Command::new("id")
        .args(["-u", &std::env::var("SUBC_POLKIT_TEST_USER").unwrap()])
        .output()
        .unwrap();
    assert!(uid.status.success());
    let uid: u32 = String::from_utf8(uid.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(uid, 0, "the test subject must not be root");
    let mut driver = Command::new("python3")
        .arg(format!(
            "{}/tests/polkit_driver.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(std::env::current_exe().unwrap())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    driver
        .stdin
        .take()
        .unwrap()
        .write_all(subc_presence::polkit::action_xml(uid).as_bytes())
        .unwrap();
    assert!(
        driver.wait().unwrap().success(),
        "real polkit driver failed"
    );
}
