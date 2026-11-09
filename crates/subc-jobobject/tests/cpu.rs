#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn helper(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "job_cpu_fixture", "--nocapture"])
        .env("SUBC_JOB_CPU_FIXTURE", mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    command
}

fn until(reader: &mut impl BufRead, prefix: &str) -> String {
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "fixture exited before {prefix}"
        );
        if let Some(at) = line.find(prefix) {
            return line[at + prefix.len()..].trim().to_owned();
        }
    }
}

#[test]
fn job_cpu_fixture() {
    let Ok(mode) = std::env::var("SUBC_JOB_CPU_FIXTURE") else {
        return;
    };
    if mode == "burn" {
        let deadline = Instant::now() + Duration::from_millis(350);
        while Instant::now() < deadline {
            std::hint::black_box(
                (0_u64..10_000).fold(1_u64, |a, b| a.wrapping_mul(31).wrapping_add(b)),
            );
        }
        println!("BURNED");
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        return;
    }
    // A nested job gives an independent kernel counter for only the child.
    // Its CPU also belongs to the outer root's job, including after exit.
    let contained = subc_jobobject::spawn_contained(&mut helper("burn")).unwrap();
    let mut child = contained.child;
    let query = contained.job.query().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    until(&mut output, "BURNED");
    println!("CHILD_CPU {}", query.cpu_ms().unwrap());
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    writeln!(child.stdin.take().unwrap(), "exit").unwrap();
    assert!(child.wait().unwrap().success());
    println!("REAPED {}", query.cpu_ms().unwrap());
    std::io::stdout().flush().unwrap();
    line.clear();
    std::io::stdin().read_line(&mut line).unwrap();
}

#[test]
fn job_cpu_survives_child_reap_and_root_exit() {
    let contained = subc_jobobject::spawn_contained(&mut helper("parent")).unwrap();
    let mut root = contained.child;
    let query = contained.job.query().unwrap();
    let mut output = BufReader::new(root.stdout.take().unwrap());
    let child_cpu: u64 = until(&mut output, "CHILD_CPU ").parse().unwrap();
    assert!(child_cpu >= 100, "the child must really burn CPU");
    let before = query.cpu_ms().unwrap();
    assert!(before >= child_cpu);
    assert!(
        query
            .process_ids(Instant::now() + Duration::from_secs(1))
            .unwrap()
            .len()
            >= 2
    );
    writeln!(root.stdin.as_mut().unwrap(), "reap").unwrap();
    let final_child_cpu: u64 = until(&mut output, "REAPED ").parse().unwrap();
    let after = query.cpu_ms().unwrap();
    writeln!(root.stdin.take().unwrap(), "exit").unwrap();
    assert!(root.wait().unwrap().success());
    let finished = query.cpu_ms().unwrap();
    assert!(
        after >= before,
        "job CPU fell across a child reap: {before} -> {after}"
    );
    assert!(after >= final_child_cpu);
    assert!(finished >= after, "job CPU fell across the root's exit");
    assert!(finished >= final_child_cpu);
}

#[test]
fn job_membership_is_queried_through_the_pinned_process_handle() {
    let first = subc_jobobject::spawn_contained(&mut helper("burn")).unwrap();
    let second = subc_jobobject::spawn_contained(&mut helper("burn")).unwrap();
    let mut first_child = first.child;
    let mut second_child = second.child;
    let mut first_output = BufReader::new(first_child.stdout.take().unwrap());
    let mut second_output = BufReader::new(second_child.stdout.take().unwrap());
    until(&mut first_output, "BURNED");
    until(&mut second_output, "BURNED");
    let query = first.job.query().unwrap();
    let member = query.contains_process(&first_child).unwrap();
    let foreign = query.contains_process(&second_child).unwrap();
    writeln!(first_child.stdin.take().unwrap(), "exit").unwrap();
    writeln!(second_child.stdin.take().unwrap(), "exit").unwrap();
    assert!(first_child.wait().unwrap().success());
    assert!(second_child.wait().unwrap().success());
    assert!(member);
    assert!(!foreign);
}
