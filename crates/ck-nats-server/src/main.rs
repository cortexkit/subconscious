use std::{ffi::OsString, path::PathBuf};

fn arguments() -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let mut config = None;
    let mut engine = None;
    let mut connection = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-c" | "--config") => {
                config = Some(PathBuf::from(args.next().ok_or("missing config path")?))
            }
            Some("--engine") => {
                engine = Some(PathBuf::from(args.next().ok_or("missing engine path")?))
            }
            Some("--subc") => {
                connection = Some(PathBuf::from(
                    args.next().ok_or("missing subc connection path")?,
                ));
            }
            _ => return Err(format!("unknown argument: {}", arg.to_string_lossy())),
        }
    }
    let config = config.ok_or("usage: ck-nats-server -c <server.conf> [--engine <path>]")?;
    let engine = match engine {
        Some(path) => path,
        None => std::env::current_exe()
            .map_err(|e| e.to_string())?
            .with_file_name(OsString::from(if cfg!(windows) {
                "ck-nats-engine.exe"
            } else {
                "ck-nats-engine"
            })),
    };
    Ok((
        config,
        engine,
        connection.ok_or("--subc <connection file> is required")?,
    ))
}

#[tokio::main]
async fn main() {
    let result = async {
        let (config, engine, connection) = arguments()?;
        // Consume the launch-secret pipe before the engine can inherit it.
        subc_client_rs::launch_nonce().map_err(|e| e.to_string())?;
        ck_nats_server::run(config, engine, connection).await
    }
    .await;
    if let Err(error) = result {
        eprintln!("ck-nats-server: {error}");
        std::process::exit(1);
    }
}
