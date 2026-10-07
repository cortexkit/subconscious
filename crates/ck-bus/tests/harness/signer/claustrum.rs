//! The real claustrum serving side: binaries found through `CK_CLAUSTRUM_BIN` and
//! `CK_CK_BIN`, and the operator ceremony run with the placed `ck auth` commands into a
//! fixture vault under the run's data home.
//!
//! The ceremony runs offline, before the daemon starts, with `--data-dir` and
//! `--key-path` inside the fixture tree, so it never reaches the operator's vault or
//! keychain.

use std::path::{Path, PathBuf};

/// The recorded condition when either binary is missing.
pub const CLAUSTRUM_BINARY_ABSENT: &str = "claustrum-binary-absent";

pub struct RealClaustrum {
    pub claustrum_bin: PathBuf,
    pub ck_bin: PathBuf,
    pub claustrum_version: String,
    pub ck_version: String,
    /// Where the daemon's storage section puts the vault for module `claustrum`.
    pub vault_dir: PathBuf,
    pub master_key_path: PathBuf,
}

impl RealClaustrum {
    /// Finds both binaries, or returns the observation for `claustrum-binary-absent`.
    /// Both run as published `ckdev-` copies (`ckdev-claustrum`, `ckdev-ck`),
    /// never under the production names they were installed with.
    pub fn discover(data_home: &Path, key_dir: &Path) -> Result<Self, String> {
        let claustrum_bin = subc_test_support::ckdev_binary(binary_from_env("CK_CLAUSTRUM_BIN")?);
        let ck_bin = subc_test_support::ckdev_binary(binary_from_env("CK_CK_BIN")?);
        Ok(Self {
            claustrum_version: version(&claustrum_bin, &["--version"]),
            ck_version: version(&ck_bin, &["--version"]),
            claustrum_bin,
            ck_bin,
            vault_dir: data_home.join("cortexkit/claustrum"),
            master_key_path: key_dir.join("claustrum-master.key"),
        })
    }

    /// `ck auth <args>` against the fixture vault; panics with the output on failure.
    pub fn ck_auth(&self, args: &[&str]) -> String {
        let output = subc_test_support::dev_command(&self.ck_bin)
            .arg("auth")
            .arg("--data-dir")
            .arg(&self.vault_dir)
            .arg("--key-path")
            .arg(&self.master_key_path)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("run {} auth: {error}", self.ck_bin.display()));
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "ck auth {args:?} failed ({}): stdout {stdout} stderr {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    }

    /// Initialises the fixture vault and mints one signing key, returning the printed
    /// public key hex and key id.
    pub fn bootstrap_and_mint(&self, credential_id: &str) -> (String, String) {
        std::fs::create_dir_all(&self.vault_dir).expect("fixture vault dir");
        self.ck_auth(&["bootstrap"]);
        let printed = self.ck_auth(&["mint-signing-key", "--id", credential_id]);
        let field = |name: &str| {
            printed
                .lines()
                .find_map(|line| line.strip_prefix(name).map(|rest| rest.trim().to_string()))
                .unwrap_or_else(|| panic!("mint-signing-key printed no {name}: {printed}"))
        };
        (field("public_key_hex"), field("key_id"))
    }

    /// One exact grant of `operation` on `credential_id` to `reserved:ckbus`.
    pub fn grant(&self, credential_id: &str, operation: &str) {
        self.ck_auth(&[
            "grant",
            "--principal",
            "reserved:ckbus",
            "--selector-kind",
            "exact",
            "--selector",
            credential_id,
            "--operation",
            operation,
        ]);
    }
}

fn binary_from_env(name: &str) -> Result<PathBuf, String> {
    match std::env::var_os(name) {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            if path.is_file() {
                Ok(path)
            } else {
                Err(format!("{name}={} is not a file", path.display()))
            }
        }
        _ => Err(format!("{name} is not set")),
    }
}

fn version(binary: &Path, args: &[&str]) -> String {
    subc_test_support::dev_command(binary)
        .args(args)
        .output()
        .map(|output| {
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines().next().unwrap_or_default().trim().to_string()
        })
        .unwrap_or_else(|error| format!("unreadable: {error}"))
}
