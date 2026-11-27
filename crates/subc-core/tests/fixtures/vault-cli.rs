#![forbid(unsafe_code)]

#[path = "message-bus.rs"]
mod message_bus;

use std::{fs, path::Path};

use message_bus::{
    bus_env, public_half, ACCOUNT_ID, BUS_VERSION, SIGNER_ID, SYSACCOUNT_ID, VAULT_VERSION,
};
use serde_json::json;
use sha2::{Digest, Sha256};

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let flag = |name: &str| {
        args.windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].as_str())
    };
    match args.first().map(String::as_str) {
        Some("--version") => {
            let path = std::env::current_exe().unwrap();
            match path.file_stem().and_then(|stem| stem.to_str()) {
                Some("ck-bus") => println!("ck-bus {BUS_VERSION}"),
                Some("ck-claustrum") => println!("ck-claustrum {VAULT_VERSION}"),
                _ => println!("ck-auth {VAULT_VERSION}"),
            }
        }
        Some("bootstrap") => {
            if let Some(path) = flag("--key-path") {
                fs::write(path, b"fixture vault key").unwrap();
            }
        }
        Some("grant") => println!("already granted (no change)"),
        Some("list") => {
            let credentials = [SIGNER_ID, SYSACCOUNT_ID, ACCOUNT_ID]
                .map(|id| json!({"id": id, "state": "active"}));
            println!("{}", json!({"v":1, "credentials": credentials}));
        }
        Some("mint-signing-key" | "public-key") => {
            let id = flag("--id").expect("key id");
            if args[0] == "mint-signing-key" {
                println!("created {id}");
            }
            println!(
                "public_key_hex {}\nkey_id 0123456789abcdef",
                public_half(id)
            );
        }
        Some("install-plan") => {
            let nats = Path::new(flag("--nats-dir").expect("NATS directory"));
            if nats.join("operator.jwt").is_file() {
                println!("{}", json!({"status":"no ceremony needed"}));
                return;
            }
            let ceremony = nats.join("ceremony");
            fs::create_dir_all(&ceremony).unwrap();
            let payloads = ["operator", "system_account"].map(|kind| {
                let path = ceremony.join(format!("{kind}.input"));
                fs::write(&path, kind).unwrap();
                json!({"payload":kind, "path":path, "sha256":format!("{:x}", Sha256::digest(kind))})
            });
            println!(
                "{}",
                json!({"status":"ceremony needed", "payloads": payloads})
            );
        }
        Some("install-apply") => {
            let nats = Path::new(flag("--nats-dir").expect("NATS directory"));
            if !nats.join("operator.jwt").is_file() {
                assert!(flag("--operator-sig").is_some() && flag("--sysaccount-sig").is_some());
                fs::write(nats.join("operator.jwt"), b"fixture operator JWT").unwrap();
                fs::write(nats.join("server.conf"), b"fixture NATS configuration").unwrap();
                fs::write(nats.join("system_account"), b"fixture-system-account").unwrap();
            }
            println!("{}", json!({"status":"applied", "env":bus_env(nats)}));
        }
        _ => std::process::exit(2),
    }
}
