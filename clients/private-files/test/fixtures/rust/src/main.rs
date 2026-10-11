#[cfg(windows)]
fn main() -> std::io::Result<()> {
    use std::{fs::OpenOptions, io::Write, path::PathBuf};
    let root = PathBuf::from(std::env::args_os().nth(1).expect("fixture root required"));
    let lease_dir = root.join("rust-lease-dir");
    cortexkit_lease::create_private_dir(&lease_dir)?;
    let lease_file = lease_dir.join("protected-file");
    let mut file = OpenOptions::new().write(true).create_new(true).open(&lease_file)?;
    cortexkit_lease::protect_file(&lease_file)?;
    file.write_all(b"rust-lease")?;
    drop(file);
    let mut inherited = OpenOptions::new().write(true).create_new(true).open(lease_dir.join("inherited-file"))?;
    inherited.write_all(b"rust-inherited")?;
    drop(inherited);
    let mut subc_file = subc_os::windows_acl::create_private_file(&root.join("rust-subc-file"))?;
    subc_file.write_all(b"rust-subc")?;
    println!("Created cortexkit-lease 0.1.3 directory/protected/inherited files and subc-os 0.1.14 file");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    panic!("Rust ACL compatibility fixture requires real Windows");
}
