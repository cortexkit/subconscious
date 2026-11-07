//! Isolated file-store test driver; native mode is for operator-run signed
//! checks only. Never prints the key and never chooses a production service.

use std::{error::Error, path::PathBuf};
use subc_os::config_keychain::{testing::FileKeyStore, KeychainStore, StoreAnchor, StoreKey};

macro_rules! dispatch {
    ($store:expr, $method:ident $(, $arg:expr)*) => {
        match $store {
            Store::File(store) => store.$method($($arg),*),
            Store::Native(store) => store.$method($($arg),*),
        }
    };
}

enum Store {
    File(FileKeyStore),
    Native(KeychainStore),
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let executable = std::env::current_exe()?;
    if !executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("ckdev-"))
    {
        return Err("fixture requires a ckdev- executable name".into());
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        return Err("usage: ckdev-os-keychain (--file PATH | --keychain SERVICE) [--deny | --fail-updates] (read | create | anchor REV SHA256 | clear REV | delete)".into());
    }
    let mut store = match args[0].as_str() {
        "--file" => Store::File(FileKeyStore::open(PathBuf::from(&args[1]))?),
        "--keychain" => {
            if !args[1].starts_with("io.cortexkit.keychain-manual.") {
                return Err(
                    "native fixture service must start with io.cortexkit.keychain-manual.".into(),
                );
            }
            Store::Native(KeychainStore::open(&args[1])?)
        }
        _ => return Err("choose --file or --keychain".into()),
    };
    let mut index = 2;
    while args.get(index).is_some_and(|arg| arg.starts_with("--")) {
        let Store::File(file) = &mut store else {
            return Err("failure injection is file-only".into());
        };
        match args[index].as_str() {
            "--deny" => file.set_access_denied(true),
            "--fail-updates" => file.set_update_failure(true),
            _ => return Err("unknown failure injection".into()),
        }
        index += 1;
    }
    match args.get(index).map(String::as_str) {
        Some("read") => match dispatch!(&store, load)? {
            None => println!("missing"),
            Some(state) => {
                println!("high_water={}", state.anchor.high_water);
                match state.anchor.current {
                    Some(anchor) => {
                        println!("current={} sha256={}", anchor.revision, hex(&anchor.sha256))
                    }
                    None => println!("current=none"),
                }
            }
        },
        Some("create") => {
            dispatch!(&store, create, random_key()?)?;
            println!("created");
        }
        Some("anchor") => {
            let revision = args.get(index + 1).ok_or("missing revision")?.parse()?;
            let digest = args.get(index + 2).ok_or("missing digest")?;
            if digest.len() != 64 || !digest.is_ascii() {
                return Err("digest must be 64 hex characters".into());
            }
            let mut sha256 = [0; 32];
            for (i, byte) in sha256.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&digest[i * 2..i * 2 + 2], 16)?;
            }
            dispatch!(&store, update_anchor, StoreAnchor { revision, sha256 })?;
            println!("anchored");
        }
        Some("clear") => {
            let revision = args
                .get(index + 1)
                .ok_or("missing unstamp revision")?
                .parse()?;
            dispatch!(&store, clear_anchor, revision)?;
            println!("cleared");
        }
        Some("delete") => {
            dispatch!(&store, delete)?;
            println!("deleted");
        }
        _ => return Err("unknown or missing operation".into()),
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random_key() -> Result<StoreKey, Box<dyn Error>> {
    #[cfg(unix)]
    {
        use std::io::Read;
        let mut bytes = zeroize::Zeroizing::new([0; 32]);
        std::fs::File::open("/dev/urandom")?.read_exact(&mut *bytes)?;
        Ok(StoreKey::new(*bytes))
    }
    #[cfg(not(unix))]
    {
        // Windows uses this fixture only for the isolated file-backed seam.
        Ok(StoreKey::new([0x51; 32]))
    }
}
