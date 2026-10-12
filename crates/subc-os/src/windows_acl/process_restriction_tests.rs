use super::{restrict_current_process, Handle};
use std::{
    io::{self, BufRead, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::ERROR_ACCESS_DENIED,
    Storage::FileSystem::WRITE_DAC,
    System::Threading::{
        OpenProcess, TerminateProcess, PROCESS_CREATE_THREAD, PROCESS_DUP_HANDLE,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE as SYNCHRONIZE, PROCESS_TERMINATE,
        PROCESS_VM_READ, PROCESS_VM_WRITE,
    },
};

mod descriptor;
mod privileges;

const CHILD_ENV: &str = "SUBC_OS_PROCESS_RESTRICTION_CHILD";
const OPENER_ENV: &str = "SUBC_OS_PROCESS_RESTRICTION_OPENER";
const READY: &str = "subc-os-process-restriction-ready";
const SDDL: &str = "subc-os-process-sddl:";
const PRIVILEGES: &str = "subc-os-opener-privileges:";
const OPEN_ERROR: &str = "subc-os-open-error:";
const TERMINATE_ERROR: &str = "subc-os-terminate-error:";

#[test]
fn process_restriction_child() {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return;
    };
    match mode.as_str() {
        "restricted" => {
            restrict_current_process().expect("restrict child process");
            let sddl = descriptor::current_process_sddl().expect("read back process DACL");
            // Start a separate line even if serial libtest printed a test prefix.
            println!("\n{SDDL}{sddl}");
        }
        "unrestricted" => {}
        _ => panic!("unknown process restriction child mode: {mode}"),
    }
    println!("{READY}");
    io::stdout().flush().unwrap();
    loop {
        thread::park();
    }
}

#[test]
fn process_restriction_opener() {
    let Ok(request) = std::env::var(OPENER_ENV) else {
        return;
    };
    // Remove the bypass privilege before parsing requests or opening any target.
    // Only this disposable process changes its token; the parent is untouched.
    let report = privileges::remove_debug_privilege().unwrap_or_else(|error| {
        println!("\n{PRIVILEGES}removal failed: {error}");
        panic!("remove opener SeDebugPrivilege: {error}");
    });
    println!("\n{PRIVILEGES}{report}");
    let fields: Vec<_> = request.split(':').collect();
    assert_eq!(fields.len(), 3, "opener requires PID:access:terminate");
    let pid: u32 = fields[0].parse().unwrap();
    let rights: u32 = fields[1].parse().unwrap();
    let terminate: bool = fields[2].parse().unwrap();
    // SAFETY: OpenProcess takes a numeric PID and access mask with no pointers.
    let raw = unsafe { OpenProcess(rights, 0, pid) };
    if raw.is_null() {
        let error = io::Error::last_os_error().raw_os_error().unwrap();
        println!("{OPEN_ERROR}{error}");
    } else {
        let handle = Handle(raw);
        println!("{OPEN_ERROR}0");
        if terminate {
            // SAFETY: The live handle was opened with PROCESS_TERMINATE.
            let result = unsafe { TerminateProcess(handle.0, 0) };
            let error = if result == 0 {
                io::Error::last_os_error().raw_os_error().unwrap()
            } else {
                0
            };
            println!("{TERMINATE_ERROR}{error}");
        }
    }
    io::stdout().flush().unwrap();
}

struct TestChild {
    child: Child,
    sddl: Option<String>,
}

impl TestChild {
    fn spawn(restricted: bool) -> Self {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows_acl::process_restriction_tests::process_restriction_child",
                "--nocapture",
            ])
            .env(
                CHILD_ENV,
                if restricted {
                    "restricted"
                } else {
                    "unrestricted"
                },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn process restriction child");
        let mut child = Self { child, sddl: None };
        let stdout = child.child.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        // Wait for an explicit post-restriction marker, not merely a live PID.
        // A bounded channel wait also catches a child that fails before startup.
        thread::spawn(move || {
            let mut output = String::new();
            for line in io::BufReader::new(stdout).lines() {
                match line {
                    // Serial libtest output can prefix the marker with the test name.
                    Ok(line) if line.ends_with(READY) => {
                        let _ = send.send(Ok(output));
                        return;
                    }
                    Ok(line) => {
                        output.push_str(&line);
                        output.push('\n');
                    }
                    Err(error) => {
                        let _ = send.send(Err(format!("read child readiness: {error}")));
                        return;
                    }
                }
            }
            let _ = send.send(Err(format!("child exited without readiness: {output}")));
        });
        let output = receive
            .recv_timeout(Duration::from_secs(30))
            .expect("child readiness timed out")
            .expect("child did not become ready");
        if restricted {
            child.sddl = Some(
                record(&output, SDDL)
                    .expect("restricted child must report process DACL")
                    .to_owned(),
            );
            eprintln!(
                "restricted child applied DACL: {}",
                child.sddl.as_deref().unwrap()
            );
        }
        child
    }

    fn open(&self, rights: u32, terminate: bool) -> OpenResult {
        open_from_stripped_process(self.child.id(), rights, terminate)
    }

    fn terminate(&mut self, rights: u32) {
        let result = self.open(rights, true);
        result.assert_success("termination handle");
        assert_eq!(
            result.termination_error,
            Some(0),
            "TerminateProcess must succeed: {}",
            result.diagnostics
        );
        assert!(self.child.wait().expect("reap terminated child").success());
    }
}

impl Drop for TestChild {
    fn drop(&mut self) {
        // The spawn handle predates the DACL change, so cleanup remains possible
        // even when an assertion panics before the explicit termination proof.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn record<'a>(output: &'a str, prefix: &str) -> Option<&'a str> {
    output.lines().find_map(|line| line.strip_prefix(prefix))
}

struct OpenResult {
    error: i32,
    termination_error: Option<i32>,
    diagnostics: String,
}

impl OpenResult {
    fn assert_success(&self, name: &str) {
        assert_eq!(
            self.error, 0,
            "OpenProcess({name}) must succeed: {}",
            self.diagnostics
        );
    }
}

fn open_from_stripped_process(pid: u32, rights: u32, terminate: bool) -> OpenResult {
    let parent_state =
        privileges::current_state().expect("query parent token elevation and SeDebugPrivilege");
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "windows_acl::process_restriction_tests::process_restriction_opener",
            "--nocapture",
        ])
        .env(OPENER_ENV, format!("{pid}:{rights}:{terminate}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn opener: {error}; parent token [{parent_state}]"));
    let mut opener = TestChild { child, sddl: None };
    let mut stdout = opener.child.stdout.take().unwrap();
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut output = String::new();
        let result = stdout.read_to_string(&mut output).map(|_| output);
        let _ = send.send(result);
    });
    let output = receive
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|error| {
            panic!("opener output timed out: {error}; parent token [{parent_state}]")
        })
        .unwrap_or_else(|error| {
            panic!("read opener output: {error}; parent token [{parent_state}]")
        });
    let status = opener.child.wait().expect("reap opener process");
    let diagnostics = format!("parent token [{parent_state}]; opener output:\n{output}");
    assert!(
        status.success(),
        "opener helper failed (including privilege removal): {diagnostics}"
    );
    let report = record(&output, PRIVILEGES)
        .unwrap_or_else(|| panic!("missing opener privilege report: {diagnostics}"));
    assert!(
        report.ends_with("SeDebugPrivilege=absent]"),
        "opener must have no debug privilege: {diagnostics}"
    );
    let error = record(&output, OPEN_ERROR)
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("missing OpenProcess result: {diagnostics}"));
    let termination_error = record(&output, TERMINATE_ERROR).map(|value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("invalid TerminateProcess result: {diagnostics}"))
    });
    eprintln!("OpenProcess mask {rights:#x}: {diagnostics}");
    OpenResult {
        error,
        termination_error,
        diagnostics,
    }
}

#[test]
fn restrict_current_process_has_exact_protected_dacl() {
    let mut child = TestChild::spawn(true);
    let sddl = child
        .sddl
        .as_deref()
        .expect("restricted child's readback SDDL");
    descriptor::assert_exact_protected_dacl(sddl);
    child.terminate(PROCESS_TERMINATE);
}

#[test]
fn restrict_current_process_keeps_daemon_rights() {
    let mut child = TestChild::spawn(true);
    child
        .open(PROCESS_QUERY_LIMITED_INFORMATION, false)
        .assert_success("PROCESS_QUERY_LIMITED_INFORMATION");
    child.open(SYNCHRONIZE, false).assert_success("SYNCHRONIZE");
    child
        .open(PROCESS_TERMINATE, false)
        .assert_success("PROCESS_TERMINATE");
    child.terminate(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE | PROCESS_TERMINATE);
}

fn assert_denied(rights: u32, name: &str) {
    let mut child = TestChild::spawn(true);
    let result = child.open(rights, false);
    assert_eq!(
        result.error, ERROR_ACCESS_DENIED as i32,
        "OpenProcess({name}) must fail with ERROR_ACCESS_DENIED; applied DACL {:?}; {}",
        child.sddl, result.diagnostics
    );
    child.terminate(PROCESS_TERMINATE);
}

#[test]
fn restrict_current_process_denies_vm_read() {
    assert_denied(PROCESS_VM_READ, "PROCESS_VM_READ");
}

#[test]
fn restrict_current_process_denies_vm_write() {
    assert_denied(PROCESS_VM_WRITE, "PROCESS_VM_WRITE");
}

#[test]
fn restrict_current_process_denies_create_thread() {
    assert_denied(PROCESS_CREATE_THREAD, "PROCESS_CREATE_THREAD");
}

#[test]
fn restrict_current_process_denies_dup_handle() {
    assert_denied(PROCESS_DUP_HANDLE, "PROCESS_DUP_HANDLE");
}

#[test]
fn restrict_current_process_denies_write_dac() {
    assert_denied(WRITE_DAC, "WRITE_DAC");
}

#[test]
fn unrestricted_child_allows_vm_read_control() {
    let mut child = TestChild::spawn(false);
    child
        .open(PROCESS_VM_READ, false)
        .assert_success("PROCESS_VM_READ unrestricted control");
    child.terminate(PROCESS_TERMINATE);
}
