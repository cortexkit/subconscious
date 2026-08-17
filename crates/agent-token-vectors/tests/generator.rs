use std::{fs, process::Command};

#[test]
fn generator_is_byte_identical_across_two_runs() {
    let temp = std::env::temp_dir().join(format!("agent-token-vectors-{}", std::process::id()));
    let first = temp.join("first.json");
    let second = temp.join("second.json");
    fs::create_dir_all(&temp).expect("create temporary output directory");

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
    fs::remove_dir_all(temp).expect("remove temporary output directory");
}
