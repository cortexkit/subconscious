//! The candidate daemon bundle is staged and verified before the installed bundle moves.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{inventory::Inventory, runtime::CommandRunner};

pub(super) const NAME: &str = "Subconscious.app";
#[cfg(unix)]
const APP_ID: &str = "5R5846NBPW.io.cortexkit.daemon";
const EXECUTABLE: &str = "Contents/MacOS/ck-subc";

pub(super) fn bundle_path(binary_home: &Path) -> PathBuf {
    binary_home
        .parent()
        .unwrap_or(binary_home)
        .join("apps")
        .join(NAME)
}

pub(super) fn executable(binary_home: &Path) -> PathBuf {
    bundle_path(binary_home).join(EXECUTABLE)
}

pub(super) fn check_destination(executable: &Path, inventory: &Inventory) -> Result<(), String> {
    let Some(bundle) = containing_bundle(executable) else {
        return Ok(());
    };
    match fs::symlink_metadata(bundle) {
        Ok(metadata) => {
            let owned = super::components::OWNED_BINARY_KINDS
                .iter()
                .any(|kind| inventory.owns_path(kind, executable));
            if !owned || !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(format!("refusal: app bundle {} is not owned by setup; move it aside and run ck setup again", bundle.display()));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not inspect app bundle {}: {error}",
            bundle.display()
        )),
    }
}

pub(super) fn containing_bundle(executable: &Path) -> Option<&Path> {
    let bundle = executable.ancestors().nth(3)?;
    (bundle.file_name().is_some_and(|name| name == NAME) && bundle.join(EXECUTABLE) == executable)
        .then_some(bundle)
}

pub(super) fn binary_home(daemon: &Path) -> Result<PathBuf, String> {
    if let Some(bundle) = containing_bundle(daemon) {
        return bundle
            .parent()
            .and_then(Path::parent)
            .map(|root| root.join("bin"))
            .ok_or_else(|| "daemon bundle has no data directory".into());
    }
    daemon
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "daemon has no binary directory".into())
}

fn run<R: CommandRunner + ?Sized>(
    runner: &mut R,
    program: &str,
    args: &[String],
) -> Result<String, String> {
    let result = runner.run(program, args)?;
    if !result.success {
        return Err(format!("{program}: {}", result.explanation()));
    }
    Ok(result.stdout)
}

/// Checks a daemon bundle's code signature and that its embedded profile
/// names the daemon's application identifier. The profile is decoded to a
/// scratch file beside the bundle, with a name no one can have prepared,
/// created exclusively and removed afterwards.
#[cfg(unix)]
pub(super) fn verify_bundle<R: CommandRunner + ?Sized>(
    bundle: &Path,
    runner: &mut R,
) -> Result<(), String> {
    run(
        runner,
        "codesign",
        &[
            "--verify".into(),
            "--strict".into(),
            bundle.to_string_lossy().into_owned(),
        ],
    )
    .map_err(|error| format!("refusal: daemon bundle signature verification failed: {error}"))?;
    let profile = bundle.join("Contents/embedded.provisionprofile");
    let decoded = run(
        runner,
        "security",
        &[
            "cms".into(),
            "-D".into(),
            "-i".into(),
            profile.to_string_lossy().into_owned(),
        ],
    )?;
    // Decode outside the signed bundle: an extra file inside it would
    // invalidate the resource seal we just checked.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let plist = bundle.with_file_name(format!(
        ".{NAME}.profile.{}.{nonce}.plist",
        std::process::id()
    ));
    {
        use std::io::Write as _;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
            options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
        }
        options
            .open(&plist)
            .and_then(|mut file| file.write_all(decoded.as_bytes()))
            .map_err(|error| format!("could not write {}: {error}", plist.display()))?;
    }
    let identifier = run(
        runner,
        "/usr/libexec/PlistBuddy",
        &[
            "-c".into(),
            "Print :Entitlements:com.apple.application-identifier".into(),
            plist.to_string_lossy().into_owned(),
        ],
    );
    let _ = fs::remove_file(&plist);
    if identifier?.trim() != APP_ID {
        return Err("refusal: embedded profile application-identifier does not match 5R5846NBPW.io.cortexkit.daemon".into());
    }
    Ok(())
}
/// Swaps two existing directories in one step (`renamex_np` with
/// `RENAME_SWAP` on macOS, `renameat2` with `RENAME_EXCHANGE` on Linux).
#[cfg(unix)]
pub(super) fn exchange(first: &Path, second: &Path) -> std::io::Result<()> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};
    renameat_with(CWD, first, CWD, second, RenameFlags::EXCHANGE).map_err(std::io::Error::from)
}
pub(super) fn extract<R: CommandRunner + ?Sized>(
    archive: &Path,
    destination: &Path,
    runner: &mut R,
) -> Result<(), String> {
    run(
        runner,
        "ditto",
        &[
            "-x".into(),
            "-k".into(),
            archive.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
        ],
    )?;
    Ok(())
}
