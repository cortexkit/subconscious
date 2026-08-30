use super::*;
use std::{fs, path::Path, process::Command as ProcessCommand};
use tempfile::TempDir;

const DECLARATION: &str = r#"{
  "version": 1,
  "trains": [{
    "id": "synthetic",
    "intended_commit": "abc123",
    "tag": "v1.0.0",
    "signing_profile": "none",
    "operator_gates": ["first_public_trigger"],
    "artifacts": [{
      "id": "archive",
      "kind": "archive",
      "identity_channel": "asset_sha256"
    }],
    "phases": [
      {"id": "preflight", "type": "preflight"},
      {"id": "publish-assets", "type": "assets"},
      {"id": "stage-artifacts", "type": "stage"}
    ]
  }]
}"#;

fn mint_repository() -> TempDir {
    let repository = tempfile::tempdir().unwrap();
    let root = repository.path();
    run_git(root, ["init"]);
    run_git(root, ["config", "user.name", "ck-release e2e"]);
    run_git(
        root,
        ["config", "user.email", "ck-release-e2e@example.invalid"],
    );
    fs::create_dir_all(root.join(".cortexkit")).unwrap();
    fs::write(root.join(".cortexkit/release.jsonc"), DECLARATION).unwrap();
    fs::write(
        root.join("archive.bin"),
        b"runtime-minted synthetic artifact",
    )
    .unwrap();
    fs::write(root.join("README.md"), "runtime-minted repository\n").unwrap();
    run_git(root, ["add", "."]);
    run_git(root, ["commit", "-m", "mint synthetic release repository"]);
    repository
}

fn run_git<const N: usize>(root: &Path, arguments: [&str; N]) {
    let result = ProcessCommand::new("git")
        .args(arguments)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn machine(
    state_root: &Path,
    arguments: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<MachineResponse, CliFailure> {
    let arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_owned())
        .collect::<Vec<_>>();
    let cli = Cli::try_parse_from(arguments).unwrap();
    execute(cli, Some(state_root.to_path_buf()))
}

fn common_arguments<'a>(repository: &'a Path, artifact: &'a Path) -> Vec<String> {
    vec![
        "ck-release".to_owned(),
        "--json".to_owned(),
        "--repo".to_owned(),
        repository.display().to_string(),
        "--train".to_owned(),
        "synthetic".to_owned(),
        "--artifact".to_owned(),
        format!("archive={}", artifact.display()),
    ]
}

#[test]
fn synthetic_train_drives_cli_commands_through_interruption_and_write_ahead_replay() {
    let repository = mint_repository();
    let state_root = tempfile::tempdir().unwrap();
    let artifact = repository.path().join("archive.bin");
    let repo = repository.path().display().to_string();

    let declare = machine(
        state_root.path(),
        ["ck-release", "--json", "declare", "--repo", repo.as_str()],
    )
    .unwrap();
    assert_eq!(serde_json::to_value(&declare).unwrap()["version"], 1);
    assert_eq!(declare.command, "declare");

    let validate = machine(
        state_root.path(),
        [
            "ck-release",
            "--json",
            "validate",
            "--repo",
            repo.as_str(),
            "--train",
            "synthetic",
        ],
    )
    .unwrap();
    assert_eq!(validate.data["provider_accessed"], false);

    let mut plan_arguments = common_arguments(repository.path(), &artifact);
    plan_arguments.splice(2..2, ["plan".to_owned(), "--dry-run".to_owned()]);
    let plan = machine(state_root.path(), &plan_arguments).unwrap();
    assert_eq!(plan.command, "plan");
    assert_eq!(plan.data["provider_accessed"], false);
    assert!(plan.data["approval_subject"]["public_effects"].is_array());

    let mut execute_arguments = common_arguments(repository.path(), &artifact);
    execute_arguments.splice(2..2, ["execute".to_owned()]);
    execute_arguments.extend([
        "--synthetic-provider".to_owned(),
        "--confirm-first-public-trigger".to_owned(),
        "--interrupt-after-effect".to_owned(),
    ]);
    let interrupted = machine(state_root.path(), &execute_arguments).unwrap_err();
    assert!(matches!(interrupted.class, FailureClass::Internal));
    assert_eq!(interrupted.detail.code, "synthetic_interruption");

    let status = machine(
        state_root.path(),
        [
            "ck-release",
            "--json",
            "status",
            "--repo",
            repo.as_str(),
            "--train",
            "synthetic",
        ],
    )
    .unwrap();
    assert_eq!(status.data["provider_accessed"], false);
    assert_eq!(status.data["pending_intents"].as_array().unwrap().len(), 1);
    assert_eq!(
        status.data["probe_conclusions"][0]["conclusion"]["kind"],
        "not_probed"
    );

    fs::write(
        repository.path().join(".cortexkit/release.jsonc"),
        DECLARATION.replace(
            "\"signing_profile\": \"none\"",
            "\"signing_profile\": \"minisign\"",
        ),
    )
    .unwrap();
    let mut mismatch_arguments = common_arguments(repository.path(), &artifact);
    mismatch_arguments.splice(2..2, ["resume".to_owned()]);
    mismatch_arguments.push("--synthetic-provider".to_owned());
    let mismatch = machine(state_root.path(), &mismatch_arguments).unwrap_err();
    assert_eq!(mismatch.detail.code, "declaration_digest_mismatch");

    fs::write(
        repository.path().join(".cortexkit/release.jsonc"),
        DECLARATION,
    )
    .unwrap();
    let resumed = machine(state_root.path(), &mismatch_arguments).unwrap();
    assert_eq!(resumed.data["synthetic_executor_calls"], 0);
    assert_eq!(resumed.data["outcomes"][0], "reconciled");
    assert_eq!(
        resumed.data["placement_instructions"]["terminal_state"],
        "verified_staged_artifacts"
    );

    fs::write(
        repository.path().join(".cortexkit/release.jsonc"),
        DECLARATION.replace(
            "\"signing_profile\": \"none\"",
            "\"signing_profile\": \"minisign\"",
        ),
    )
    .unwrap();
    let preview = machine(
        state_root.path(),
        [
            "ck-release",
            "--json",
            "rebind",
            "--repo",
            repo.as_str(),
            "synthetic",
        ],
    )
    .unwrap();
    let replacement_digest = preview.data["preview"]["replacement_digest"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(preview.data["requires_confirmation"], true);

    let rebound = machine(
        state_root.path(),
        [
            "ck-release",
            "--json",
            "rebind",
            "--repo",
            repo.as_str(),
            "synthetic",
            "--confirm",
            replacement_digest.as_str(),
        ],
    )
    .unwrap();
    assert_eq!(rebound.data["approval_reconstruction_required"], true);

    let abandoned = machine(
        state_root.path(),
        [
            "ck-release",
            "--json",
            "abandon",
            "--repo",
            repo.as_str(),
            "synthetic",
        ],
    )
    .unwrap();
    assert_eq!(abandoned.data["evidence_retained"], true);
}
