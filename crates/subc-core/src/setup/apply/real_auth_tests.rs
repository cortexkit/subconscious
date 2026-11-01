use super::*;
use cortexkit_test_support::ScratchDir;
use sha2::{Digest, Sha256};

/// Run the published ck-auth so it enforces the vault's real key-path policy.
#[test]
#[ignore = "downloads the published Linux x64 ck-auth; required by Linux CI"]
fn published_ck_auth_bootstrap_respects_key_store_selection() {
    let root = ScratchDir::new("setup-published-ck-auth");
    let archive = root.join("ck-auth.zip");
    let download = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--retry",
            "3",
            "--max-time",
            "120",
        ])
        .arg(
            "https://github.com/cortexkit/claustrum/releases/download/v0.2.0/ck-auth-linux-x64.zip",
        )
        .arg("--output")
        .arg(&archive)
        .output()
        .expect("curl is required for the published ck-auth regression");
    assert!(
        download.status.success(),
        "download: {}",
        String::from_utf8_lossy(&download.stderr)
    );
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&archive).unwrap())),
        "2bd4f8afa130af5fc0409bd5540efae77db8e8eb7f0e136cf61e74fb84a3debd",
        "published ck-auth archive checksum"
    );
    let unpack = Command::new("unzip")
        .arg("-q")
        .arg(&archive)
        .arg("-d")
        .arg(root.path())
        .output()
        .expect("unzip is required for the published ck-auth regression");
    assert!(
        unpack.status.success(),
        "unzip: {}",
        String::from_utf8_lossy(&unpack.stderr)
    );
    let auth = root.join("ck-auth");
    assert!(auth.is_file(), "archive must contain ck-auth");

    // Refusals can leave store.db, so neither case may reuse the other's home.
    for explicit_key in [false, true] {
        let home = root.join(if explicit_key {
            "file-home"
        } else {
            "default-home"
        });
        let config_home = home.join(".config");
        let data_home = home.join(".local/share");
        let runtime = home.join("runtime");
        for directory in [&home, &config_home, &data_home, &runtime] {
            file_modes::ensure_owned_directory(directory, 0o700).unwrap();
        }
        let data_dir = data_home.join("cortexkit");
        let run = |mut command: Command| {
            let output = command
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", &config_home)
                .env("XDG_DATA_HOME", &data_home)
                .env("XDG_CACHE_HOME", home.join(".cache"))
                .env("XDG_STATE_HOME", home.join(".local/state"))
                .env("XDG_RUNTIME_DIR", &runtime)
                // Do not borrow a developer's or runner's live Secret Service.
                .env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}", runtime.join("no-bus").display()),
                )
                .output()
                .unwrap();
            if output.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "ck auth bootstrap failed with {}\n{}\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ))
            }
        };

        if !explicit_key {
            let default_key = default_claustrum_key_path().unwrap();
            let refusal =
                run_auth_bootstrap(&auth, default_key.as_deref(), &data_dir, run).unwrap_err();
            assert!(!refusal.contains("forbidden co-location"), "{refusal}");
            assert!(
                refusal.contains("keyring") || refusal.contains("Secret Service"),
                "{refusal}"
            );
            let vault_text = refusal.split("\nhint:").next().unwrap();
            assert!(vault_text.contains("--key-path"), "{refusal}");
            assert!(refusal.contains("install -d -m 700 ~/.config/cortexkit/secrets && ck setup claustrum --key-path ~/.config/cortexkit/secrets/master.key"), "{refusal}");
            continue;
        }

        let key = config_home.join("cortexkit/secrets/master.key");
        run_auth_bootstrap(&auth, Some(&key), &data_dir, run).unwrap();
        let key_source = fs::read_to_string(data_dir.join("claustrum/key-source")).unwrap();
        assert_eq!(
            key_source.lines().collect::<Vec<_>>(),
            ["version:1", &format!("file:{}", key.display())]
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(key.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
