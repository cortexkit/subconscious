//! Test-only runner: validate the transport without evaluating script code.

use sha2::{Digest, Sha256};
use std::io::Read;

pub(crate) fn main() {
    if let Err(error) = run() {
        eprintln!("script fixture refused: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--env-parent") {
        return env_parent();
    }
    let metadata = std::env::var(subc_os::script_pipe::SCRIPT_FD_ENV)?;
    #[cfg(unix)]
    let mut pipe = subc_os::script_pipe::take(&metadata)?;
    #[cfg(windows)]
    let mut pipe =
        subc_os::script_pipe::connect(&metadata, std::env::var("SUBC_DAEMON_PID")?.parse()?)?;
    let mut header = [0; 52];
    pipe.read_exact(&mut header)?;
    if &header[..8] != b"CKSCRIPT" || header[8..12] != 1u32.to_le_bytes() {
        return Err("bad header".into());
    }
    let length = u64::from_le_bytes(header[12..20].try_into()?);
    if length > 32 * 1024 * 1024 {
        return Err("script too large".into());
    }
    let mut bytes = vec![0; length as usize];
    pipe.read_exact(&mut bytes)?;
    match pipe.read(&mut [0]) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
        Ok(_) => return Err("extra byte".into()),
        Err(error) => return Err(error.into()),
    }
    if Sha256::digest(&bytes)[..] != header[20..52] {
        return Err("digest mismatch".into());
    }
    let output = std::env::args_os().nth(1).ok_or("missing receipt path")?;
    let forbidden = std::env::vars()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "NODE_OPTIONS" | "BUN_OPTIONS" | "BUN_INSTALL" | "NODE_PATH"
            ) || key.starts_with("BUN_CONFIG_")
                || key.starts_with("PYTHON")
        })
        .collect::<Vec<_>>();
    let report = serde_json::json!({"header":header.to_vec(),"bytes":bytes,"env":metadata,"forbidden_env":forbidden,"runner":std::env::current_exe()?});
    std::fs::write(output, serde_json::to_vec(&report)?)?;
    // Stay alive until the test stops us so the daemon can compare this
    // process's executable with the runner file it approved before spawning.
    std::thread::sleep(std::time::Duration::from_secs(600));
    Ok(())
}

fn env_parent() -> Result<(), Box<dyn std::error::Error>> {
    let script = std::env::args_os().nth(2).ok_or("missing script")?;
    let receipt = std::env::args_os().nth(3).ok_or("missing receipt")?;
    let script_runtime = match std::env::args().nth(4).as_deref() {
        Some("bun") => subc_daemon::ScriptRuntime::Bun,
        Some("python") => subc_daemon::ScriptRuntime::Python,
        _ => return Err("missing runtime".into()),
    };
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let supervisor = subc_daemon::Supervisor::new(
            std::sync::Arc::new(subc_daemon::Registry::default()),
            Default::default(),
        );
        let module = supervisor
            .spawn_async(subc_daemon::ModuleSpec {
                module_id: "script-env".into(),
                program: std::env::current_exe()?,
                script: Some(subc_daemon::ScriptEntry::new(script_runtime, script.into())),
                args: vec![receipt.to_string_lossy().into_owned()],
                env: vec![],
                reserved: false,
                reserved_prefixes: vec![],
                protocol: subc_control::ModuleProtocol::None,
                overlap: Default::default(),
                carry_containment: false,
            })
            .await?;
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !std::path::Path::new(&receipt).is_file() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        module.stop().await?;
        received?;
        Ok(())
    })
}
