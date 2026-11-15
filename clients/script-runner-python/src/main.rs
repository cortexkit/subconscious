//! ck-script-runner-python: runs one approved Python script with an embedded CPython 3.13.
//!
//!   ck-script-runner-python              run the script the daemon pipes in (SUBC_SCRIPT_FD)
//!   ck-script-runner-python --check FILE  print load warnings for FILE without running it
//!   ck-script-runner-python --version
//!
//! The interpreter, its native modules and its standard library are all inside this executable.
//! Startup is isolated (src/embed.c), and imports are served from memory only (src/bootstrap.py).

use std::ffi::{c_char, c_int, CString, OsString};
use std::process::ExitCode;

mod resources;

/// The startup code the interpreter runs before the script, NUL-terminated for C.
const BOOTSTRAP: &str = concat!(include_str!("bootstrap.py"), "\0");

const MODE_RUN: c_int = 0;
const MODE_CHECK: c_int = 1;

extern "C" {
    /// Starts the embedded interpreter and runs or checks `payload` (src/embed.c).
    fn ck_python_main(
        executable: *const c_char,
        bootstrap: *const c_char,
        mode: c_int,
        payload: *const u8,
        payload_len: usize,
    ) -> c_int;
}

fn executable_path() -> CString {
    let path = std::env::current_exe().map(|path| path.into_os_string()).unwrap_or_else(|_| "ck-script-runner-python".into());
    #[cfg(unix)]
    let bytes = std::os::unix::ffi::OsStringExt::into_vec(path);
    #[cfg(not(unix))]
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    CString::new(bytes).unwrap_or_else(|_| CString::from(c"ck-script-runner-python"))
}

fn run_python(mode: c_int, payload: &[u8]) -> ExitCode {
    let executable = executable_path();
    // SAFETY: every pointer stays valid for the call, and BOOTSTRAP ends with NUL.
    let code = unsafe {
        ck_python_main(executable.as_ptr(), BOOTSTRAP.as_ptr().cast(), mode, payload.as_ptr(), payload.len())
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn main() -> ExitCode {
    let names = std::env::vars_os().map(|(name, _)| name);
    if let Some(refused) = ck_script_transport::environment_refusal(names, cfg!(windows)) {
        eprintln!("{refused}");
        return ExitCode::FAILURE;
    }
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [flag] if flag == "--version" => {
            println!("ck-script-runner-python {} (CPython {})", env!("CARGO_PKG_VERSION"), env!("CK_PYTHON_VERSION"));
            ExitCode::SUCCESS
        }
        [flag, file] if flag == "--check" => match std::fs::read(file) {
            Ok(source) => run_python(MODE_CHECK, &source),
            Err(error) => {
                eprintln!("script_check_failed: cannot read {}: {error}", file.to_string_lossy());
                ExitCode::FAILURE
            }
        },
        // Like the Bun runner, any other invocation runs the piped script.
        _ => match ck_script_transport::read_script() {
            Ok(script) => run_python(MODE_RUN, &script),
            Err(refused) => {
                eprintln!("{refused}");
                ExitCode::FAILURE
            }
        },
    }
}
