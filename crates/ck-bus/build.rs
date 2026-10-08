use sha2::{Digest, Sha256};
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-env-changed=CK_BUILD_REV");
    println!("cargo:rerun-if-env-changed=CK_BUILD_LOCK_DIGEST");
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest_dir.join("../..");
    emit_git_rerun_paths(&manifest_dir);
    let rev = env::var("CK_BUILD_REV")
        .ok()
        .or_else(|| {
            let output = Command::new("git")
                .args(["--no-optional-locks", "rev-parse", "HEAD"])
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
    // Watch the lock only where it exists: cargo treats a missing watched path
    // as always changed and would rebuild this crate on every build.
    if lock.exists() {
        println!("cargo:rerun-if-changed={}", lock.display());
    }
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

// Keep this helper identical in the four build scripts; sharing it needs a build-dependency crate.
fn emit_git_rerun_paths(root: &Path) {
    for path in ["HEAD", "packed-refs", "index"] {
        emit_git_rerun_path(root, path);
    }

    if let Ok(output) = Command::new("git")
        .args(["--no-optional-locks", "symbolic-ref", "-q", "HEAD"])
        .current_dir(root)
        .output()
    {
        if output.status.success() {
            if let Ok(branch_ref) = String::from_utf8(output.stdout) {
                emit_git_rerun_path(root, branch_ref.trim());
            }
        }
    }
}

fn emit_git_rerun_path(root: &Path, path: &str) {
    let Some(path) = Command::new("git")
        .args(["--no-optional-locks", "rev-parse", "--git-path", path])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
    else {
        return;
    };
    let path = path.trim();
    if path.is_empty() {
        return;
    }
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
