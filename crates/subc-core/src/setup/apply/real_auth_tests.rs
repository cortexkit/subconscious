use super::*;
use cortexkit_test_support::ScratchDir;
use sha2::{Digest, Sha256};

/// The published Linux x64 ck-auth these tests run, pinned by archive digest so
/// a re-published asset can't change what they prove.
const CK_AUTH_URL: &str =
    "https://github.com/cortexkit/claustrum/releases/download/v0.3.0/ck-auth-linux-x64.zip";
const CK_AUTH_SHA256: &str = "64245680ce56fd31dd07813a2100d065aba4cce5b98ab29245ea3f2bfb7892fa";

/// Linux CI sets this to the address of a private D-Bus session that runs an
/// unlocked Secret Service, so the Secret Service test never touches a
/// developer's or runner's own keyring.
const SECRET_SERVICE_BUS_ENV: &str = "SUBC_TEST_SECRET_SERVICE_BUS";

/// Uses a pre-fetched archive when supplied, otherwise downloads the pinned
/// ck-auth, and verifies it before unpacking into `root`.
fn published_ck_auth(root: &ScratchDir) -> PathBuf {
    let archive = if let Some(path) = std::env::var_os("SUBC_TEST_CK_AUTH_ZIP") {
        PathBuf::from(path)
    } else {
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
            .arg(CK_AUTH_URL)
            .arg("--output")
            .arg(&archive)
            .output()
            .expect("curl is required for the published ck-auth regression");
        assert!(
            download.status.success(),
            "download: {}",
            String::from_utf8_lossy(&download.stderr)
        );
        archive
    };
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&archive).unwrap())),
        CK_AUTH_SHA256,
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
    auth
}

#[test]
fn published_ck_auth_rejects_wrong_prefetched_archive() {
    let root = ScratchDir::new("setup-published-ck-auth-wrong-archive");
    let archive = root.join("wrong.zip");
    fs::write(&archive, b"not the pinned ck-auth archive").unwrap();
    // A subprocess isolates the archive override from tests running in parallel.
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "setup::apply::real_auth_tests::published_ck_auth_bootstrap_respects_key_store_selection",
            "--ignored",
            "--nocapture",
        ])
        .env("SUBC_TEST_CK_AUTH_ZIP", archive)
        .output()
        .unwrap();
    let output = format!(
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!result.status.success(), "{output}");
    assert!(
        output.contains("published ck-auth archive checksum"),
        "{output}"
    );
}

/// A private home for one case. Each case gets its own, because a refused
/// bootstrap can leave a vault store behind that the next case would read.
struct CaseHome {
    home: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
    runtime: PathBuf,
}

impl CaseHome {
    fn new(root: &ScratchDir, name: &str) -> Self {
        let home = root.join(name);
        let case = Self {
            config_home: home.join(".config"),
            data_home: home.join(".local/share"),
            runtime: home.join("runtime"),
            home,
        };
        for directory in [
            &case.home,
            &case.config_home,
            &case.data_home,
            &case.runtime,
        ] {
            file_modes::ensure_owned_directory(directory, 0o700).unwrap();
        }
        case
    }

    fn data_dir(&self) -> PathBuf {
        self.data_home.join("cortexkit")
    }

    /// Runs `command` with only this home's environment. `bus` is the D-Bus
    /// session to use; `None` points at a socket that doesn't exist, so the
    /// vault finds no Secret Service.
    fn run(&self, mut command: Command, bus: Option<&str>) -> Result<(), String> {
        let bus = match bus {
            Some(address) => address.to_owned(),
            None => format!("unix:path={}", self.runtime.join("no-bus").display()),
        };
        let output = command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_DATA_HOME", &self.data_home)
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("DBUS_SESSION_BUS_ADDRESS", bus)
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
    }
}

/// Run the published ck-auth so it enforces the vault's real key-path policy.
#[test]
#[ignore = "downloads the published Linux x64 ck-auth; required by Linux CI"]
fn published_ck_auth_bootstrap_respects_key_store_selection() {
    let root = ScratchDir::new("setup-published-ck-auth");
    let auth = published_ck_auth(&root);

    // No key path and no Secret Service: the vault refuses, and setup's hint
    // names the key-file route.
    let case = CaseHome::new(&root, "default-home");
    let default_key = default_claustrum_key_path().unwrap();
    let refusal = run_auth_bootstrap(&auth, default_key.as_deref(), &case.data_dir(), |c| {
        case.run(c, None)
    })
    .unwrap_err();
    assert!(!refusal.contains("forbidden co-location"), "{refusal}");
    assert!(
        refusal.contains("keyring") || refusal.contains("Secret Service"),
        "{refusal}"
    );
    let vault_text = refusal.split("\nhint:").next().unwrap();
    assert!(vault_text.contains("--key-path"), "{refusal}");
    assert!(refusal.contains("install -d -m 700 ~/.config/cortexkit/secrets && ck setup claustrum --key-path ~/.config/cortexkit/secrets/master.key"), "{refusal}");

    // An explicit key file outside the data folder: the vault takes it and
    // records it, in an owner-only folder.
    let case = CaseHome::new(&root, "file-home");
    let key = case.config_home.join("cortexkit/secrets/master.key");
    run_auth_bootstrap(&auth, Some(&key), &case.data_dir(), |c| case.run(c, None)).unwrap();
    let key_source = fs::read_to_string(case.data_dir().join("claustrum/key-source")).unwrap();
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

/// With a working Secret Service and no key path, the published ck-auth keeps
/// its master key in the system keyring and setup succeeds: the default path a
/// Linux desktop user takes.
#[test]
#[ignore = "needs a private D-Bus session with an unlocked Secret Service; run by Linux CI"]
fn published_ck_auth_bootstrap_uses_secret_service() {
    let Ok(bus) = std::env::var(SECRET_SERVICE_BUS_ENV) else {
        panic!("{SECRET_SERVICE_BUS_ENV} must name the test D-Bus session");
    };
    let root = ScratchDir::new("setup-published-ck-auth-secret-service");
    let auth = published_ck_auth(&root);
    assert!(
        default_claustrum_key_path().unwrap().is_none(),
        "setup must pass no key path by default"
    );

    // A clean home, then one where an earlier refused setup left an empty
    // vault store behind (v0.2.0 refused that one as an old vault).
    for leftover_store in [false, true] {
        let case = CaseHome::new(
            &root,
            if leftover_store {
                "leftover-home"
            } else {
                "keyring-home"
            },
        );
        let vault_dir = case.data_dir().join("claustrum");
        if leftover_store {
            file_modes::ensure_owned_directory(&case.data_dir(), 0o700).unwrap();
            file_modes::ensure_owned_directory(&vault_dir, 0o700).unwrap();
            fs::write(vault_dir.join("store.db"), b"").unwrap();
        }
        if let Err(err) =
            run_auth_bootstrap(&auth, None, &case.data_dir(), |c| case.run(c, Some(&bus)))
        {
            panic!("default setup (leftover store: {leftover_store}): {err}");
        }

        let key_source = fs::read_to_string(vault_dir.join("key-source")).unwrap();
        std::println!("key-source (leftover store: {leftover_store}):\n{key_source}");
        assert_eq!(
            key_source.lines().collect::<Vec<_>>(),
            ["version:1", "secret-service"],
            "the vault must record the Secret Service as its key source"
        );
        let key_files = files_named(&case.data_dir(), "master.key");
        assert!(
            key_files.is_empty(),
            "no key file may be written: {key_files:?}"
        );

        // A later start finds the same key in the keyring, with no key flag.
        let mut status = Command::new(&auth);
        status.arg("status").arg("--data-dir").arg(&vault_dir);
        if let Err(err) = case.run(status, Some(&bus)) {
            panic!("ck-auth status after setup: {err}");
        }
    }
}

/// Every file named `name` anywhere under `dir`.
fn files_named(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().is_some_and(|file| file == name) {
                found.push(path);
            }
        }
    }
    found
}
