use std::{fs, process::Command};
use subc_test_support::TestTempDir;

#[test]
fn generator_is_byte_identical_across_two_runs() {
    let temp = TestTempDir::new("agent-token-vectors");
    let first = temp.join("first.json");
    let second = temp.join("second.json");

    for output in [&first, &second] {
        let status = Command::new(env!("CARGO_BIN_EXE_generate"))
            .args(["--output", output.to_str().expect("UTF-8 temp path")])
            .status()
            .expect("run generator binary");
        assert!(status.success(), "generator exits successfully");
    }

    assert_eq!(
        fs::read(first).expect("first output"),
        fs::read(second).expect("second output")
    );
}
