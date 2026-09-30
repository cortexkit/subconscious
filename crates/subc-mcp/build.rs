use sha2::{Digest, Sha256};
use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=CK_BUILD_REV");
    println!("cargo:rerun-if-env-changed=CK_BUILD_LOCK_DIGEST");
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    // Resolve git paths through git so linked worktrees track their own HEAD.
    for path in ["HEAD", "refs"] {
        if let Ok(output) = Command::new("git")
            .args(["rev-parse", "--git-path", path])
            .current_dir(&root)
            .output()
        {
            if output.status.success() {
                println!(
                    "cargo:rerun-if-changed={}",
                    root.join(String::from_utf8_lossy(&output.stdout).trim())
                        .display()
                );
            }
        }
    }
    let rev = env::var("CK_BUILD_REV")
        .ok()
        .or_else(|| {
            let output = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&root)
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
        .unwrap_or_else(|| "unavailable".to_owned());
    let lock = root.join("Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let digest = env::var("CK_BUILD_LOCK_DIGEST")
        .ok()
        .or_else(|| {
            std::fs::read(lock)
                .ok()
                .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
        })
        .unwrap_or_else(|| "unavailable".to_owned());
    println!("cargo:rustc-env=CK_BUILD_REV={rev}");
    println!("cargo:rustc-env=CK_BUILD_LOCK_DIGEST={digest}");
}
