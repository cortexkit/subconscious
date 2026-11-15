//! End-to-end tests of the built runner. Scripts reach it through the shared pipe harness
//! (clients/script-runner/tests/pipe-launcher.py), the same daemon wire format the Bun runner's
//! tests use, run by the CPython the build used (CK_TEST_PYTHON).

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ck_script_transport::{frame, HEADER_LEN};
use serde_json::Value;

const REFUSAL: &str = "is not in the approved runner (no disk imports)";

fn runner() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ck-script-runner-python"))
}

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// A fresh scratch folder under the crate's target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = manifest_dir()
        .join("target/scratch")
        .join(format!("{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch folder");
    dir
}

/// The test's environment without anything the runner refuses or the harness replaces.
fn clean_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    for (name, _) in std::env::vars_os() {
        let key = name.to_string_lossy().to_uppercase();
        let refused = key.starts_with("PYTHON")
            || key.starts_with("BUN_")
            || key.starts_with("SUBC_")
            || key == "NODE_OPTIONS"
            || key == "NODE_PATH";
        if refused {
            command.env_remove(&name);
        }
    }
    command
}

struct Outcome {
    code: i64,
    out: String,
    err: String,
}

/// Runs `bytes` through the daemon's channel. `scenario` is the harness's fault to inject.
fn run_with(
    executable: &Path,
    bytes: &[u8],
    scenario: &str,
    env: &[(&str, &str)],
    cwd: Option<&Path>,
) -> Outcome {
    let launcher = manifest_dir().join("../script-runner/tests/pipe-launcher.py");
    let mut command = clean_command(env!("CK_TEST_PYTHON"));
    // -I keeps the harness's own interpreter from reading the PYTHON* variables under test.
    command
        .arg("-I")
        .arg(launcher)
        .arg(executable)
        .arg(scenario);
    command.envs(env.iter().copied());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("harness starts");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(bytes)
        .expect("payload written");
    let output = child.wait_with_output().expect("harness ends");
    assert!(
        output.status.success(),
        "harness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("harness JSON");
    assert_eq!(result["write_errors"], serde_json::json!([]));
    Outcome {
        code: result["code"].as_i64().expect("exit code"),
        out: result["out"].as_str().expect("stdout").to_string(),
        err: result["err"].as_str().expect("stderr").to_string(),
    }
}

fn run(script: &str) -> Outcome {
    run_with(&runner(), &frame(script.as_bytes()), "valid", &[], None)
}

fn check(file: &Path) -> Outcome {
    let output = clean_command(runner())
        .arg("--check")
        .arg(file)
        .output()
        .expect("runner starts");
    Outcome {
        code: i64::from(output.status.code().unwrap_or(-1)),
        out: String::from_utf8_lossy(&output.stdout).into_owned(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Writes a zip bundle with the harness's Python: the runner's tests carry no zip writer.
fn bundle(path: &Path, entries: &[(&str, &[u8])]) {
    let mut program =
        String::from("import sys, zipfile\nwith zipfile.ZipFile(sys.argv[1], 'w') as z:\n");
    for (name, data) in entries {
        program.push_str(&format!(
            "    z.writestr({name:?}, bytes({:?}))\n",
            data.to_vec()
        ));
    }
    let status = clean_command(env!("CK_TEST_PYTHON"))
        .arg("-I")
        .arg("-c")
        .arg(program)
        .arg(path)
        .status()
        .expect("python");
    assert!(status.success());
}

#[test]
fn version_probe_runs_without_a_script_pipe() {
    let output = clean_command(runner())
        .arg("--version")
        .output()
        .expect("runner starts");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "ck-script-runner-python 0.1.0 (CPython 3.13.18)\n"
    );
    assert!(output.status.success() && output.stderr.is_empty());
}

fn shared_vector() -> (Vec<u8>, Vec<u8>) {
    let text =
        fs::read_to_string(manifest_dir().join("../script-runner/tests/script-pipe-vector.json"))
            .expect("vector");
    let vector: Value = serde_json::from_str(&text).expect("vector JSON");
    let hex = vector["header_hex"].as_str().expect("header");
    let header = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect();
    (
        header,
        vector["source"]
            .as_str()
            .expect("source")
            .as_bytes()
            .to_vec(),
    )
}

#[test]
fn shared_vector_header_admits_its_bytes_to_python() {
    // The vector's body is JavaScript, so Python fails on it: proof the header was accepted.
    let (header, source) = shared_vector();
    let result = run_with(&runner(), &[header, source].concat(), "valid", &[], None);
    assert_eq!(result.code, 1, "{}", result.err);
    assert!(
        !result.err.contains("script_pipe_refused"),
        "{}",
        result.err
    );
    assert!(
        result
            .err
            .contains("NameError: name 'console' is not defined"),
        "{}",
        result.err
    );
}

#[test]
fn shared_vector_header_refuses_other_bytes() {
    let (header, mut source) = shared_vector();
    source[0] ^= 1;
    let result = run_with(&runner(), &[header, source].concat(), "valid", &[], None);
    assert_eq!((result.code, result.out.as_str()), (1, ""));
    assert!(
        result.err.contains("script_pipe_refused: digest mismatch"),
        "{}",
        result.err
    );
}

#[test]
fn transport_faults_are_refused_before_any_code_runs() {
    let good = frame(b"print('EVALUATED')\n");
    let faults: Vec<(&str, Vec<u8>)> = vec![
        ("wrong-identity", good.clone()),
        ("not-pipe", good.clone()),
        ("valid", good[..HEADER_LEN - 1].to_vec()),
        ("valid", good[..good.len() - 1].to_vec()),
        ("valid", [good.clone(), vec![0]].concat()),
        ("valid", {
            let mut bytes = good.clone();
            bytes[0] ^= 1;
            bytes
        }),
        ("valid", {
            let mut bytes = good.clone();
            bytes[8] = 2;
            bytes
        }),
        ("valid", {
            let mut bytes = good.clone();
            bytes[20] ^= 1;
            bytes
        }),
    ];
    for (index, (scenario, bytes)) in faults.into_iter().enumerate() {
        let result = run_with(&runner(), &bytes, scenario, &[], None);
        assert_ne!(result.code, 0, "fault {index} ({scenario})");
        assert_eq!(result.out, "", "fault {index} ({scenario})");
        assert!(
            result.err.contains("script_pipe_refused:"),
            "fault {index} ({scenario}): {}",
            result.err
        );
    }
}

#[test]
fn python_and_loader_variables_are_refused_at_start() {
    for name in [
        "PYTHONSTARTUP",
        "PYTHONPATH",
        "PYTHONHOME",
        "PYTHONSAFEPATH",
        "NODE_OPTIONS",
        "BUN_CONFIG_TEST",
    ] {
        let result = run_with(
            &runner(),
            &frame(b"print('EVALUATED')\n"),
            "valid",
            &[(name, "")],
            None,
        );
        assert_eq!((result.code, result.out.as_str()), (1, ""), "{name}");
        assert!(
            result
                .err
                .contains(&format!("script_environment_refused: {name}")),
            "{name}: {}",
            result.err
        );
    }
}

#[test]
fn nothing_beside_the_runner_or_in_the_working_folder_is_read() {
    let dir = scratch("disk");
    let bin = dir.join("bin");
    let cwd = dir.join("cwd");
    let pth = dir.join("pth");
    for folder in [&bin, &cwd, &pth] {
        fs::create_dir_all(folder).expect("folder");
    }
    let executable = bin.join("ck-script-runner-python");
    fs::copy(runner(), &executable).expect("copy runner");
    for folder in [&bin, &cwd, &pth] {
        fs::write(folder.join("diskmod.py"), "print('DISK_EVALUATED')\n").expect("module");
        fs::write(folder.join("nativemod.so"), b"\x7fELF not really").expect("native");
        fs::write(
            folder.join("nativemod.cpython-313-darwin.so"),
            b"\x7fELF not really",
        )
        .expect("native");
        fs::write(
            folder.join("nativemod.cpython-313-x86_64-linux-gnu.so"),
            b"\x7fELF not really",
        )
        .expect("native");
    }
    // Startup files CPython would honour beside an ordinary interpreter.
    fs::write(
        bin.join("pyvenv.cfg"),
        format!(
            "home = {}\ninclude-system-site-packages = true\n",
            pth.display()
        ),
    )
    .unwrap();
    fs::write(
        bin.join("ck-script-runner-python._pth"),
        format!("{}\nimport diskmod\n", pth.display()),
    )
    .unwrap();
    fs::write(bin.join("pybuilddir.txt"), "../pth\n").unwrap();
    let script = r#"
import os, sys
print(sys.argv[0], __file__, os.path.exists(__file__), sys.path, sys.flags.isolated, sys.flags.safe_path)
print(all(path == sys.executable for path in (sys.prefix, sys.exec_prefix, sys.base_prefix, sys.base_exec_prefix)))
for name in ("diskmod", "nativemod"):
    try:
        __import__(name)
    except ModuleNotFoundError as error:
        print(type(error).__name__, error)
"#;
    let result = run_with(
        &executable,
        &frame(script.as_bytes()),
        "valid",
        &[],
        Some(&cwd),
    );
    assert_eq!(result.err, "");
    assert_eq!(
        result.out,
        format!(
            "ck-script:main ck-script:main False [] 1 True\nTrue\nModuleNotFoundError diskmod {REFUSAL}\nModuleNotFoundError nativemod {REFUSAL}\n"
        )
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn built_in_modules_import_and_ctypes_does_not() {
    let unix = if cfg!(unix) {
        "'_posixsubprocess', 'fcntl', 'termios', "
    } else {
        ""
    };
    let script = format!(
        r#"
import importlib, sys
names = [{unix}'_json', '_socket', 'select', '_ssl', '_hashlib', 'zlib', '_sqlite3', '_datetime', '_struct', 'math',
         '_random', '_bisect', '_heapq', 'array', 'binascii', 'unicodedata', '_asyncio', '_contextvars', '_queue',
         '_pickle', '_opcode', '_blake2', '_csv', 'pyexpat', '_decimal']
for name in names:
    importlib.import_module(name)
    assert name in sys.builtin_module_names, name
import asyncio, dataclasses, hashlib, sqlite3, ssl
print('BUILTINS_OK', hashlib.blake2b(b'').hexdigest()[:8])
for name in ('_ctypes', 'ctypes'):
    try:
        importlib.import_module(name)
    except ModuleNotFoundError as error:
        print(error)
"#
    );
    let result = run(&script);
    assert_eq!(result.err, "");
    assert_eq!(
        result.out,
        format!("BUILTINS_OK 786a02f7\n_ctypes {REFUSAL}\n_ctypes {REFUSAL}\n")
    );
}

/// Modules whose import is expected to fail, by the missing module the failure names: the
/// extensions the runner leaves out on purpose.
const LEFT_OUT: &[&str] = &[
    "_ctypes",
    "_curses",
    "_curses_panel",
    "readline",
    "_dbm",
    "_gdbm",
    "_tkinter",
    "tkinter",
];

#[test]
fn every_shipped_standard_library_module_imports() {
    let stdlib = Path::new(env!("CK_TEST_STDLIB"));
    let dropped = [
        "test",
        "idlelib",
        "tkinter",
        "turtledemo",
        "ensurepip",
        "lib2to3",
        "venv",
        "pydoc_data",
    ];
    // Importing these has side effects (a browser opens, text prints) and proves nothing more.
    let noisy = ["antigravity", "this", "__hello__", "__phello__"];
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(stdlib).expect("stdlib") {
        let path = entry.expect("entry").path();
        let file = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let name = file
            .strip_suffix(".py")
            .map(str::to_string)
            .or_else(|| path.join("__init__.py").exists().then_some(file));
        if let Some(name) =
            name.filter(|name| !dropped.contains(&name.as_str()) && !noisy.contains(&name.as_str()))
        {
            names.insert(name);
        }
    }
    assert!(
        names.len() > 150,
        "found only {} modules in {}",
        names.len(),
        stdlib.display()
    );
    let script = format!(
        r#"
import sys
for name in {names:?}:
    try:
        __import__(name)
    except ImportError as error:
        print('IMPORT_FAILED', name, error.name)
    except BaseException as error:
        print('ERROR', name, type(error).__name__, error)
print('IMPORTED', len({names:?}))
"#,
        names = names.iter().collect::<Vec<_>>()
    );
    let result = run(&script);
    assert_eq!(result.code, 0, "{}", result.err);
    let unexpected: Vec<&str> = result
        .out
        .lines()
        .filter(|line| {
            line.starts_with("ERROR")
                || (line.starts_with("IMPORT_FAILED")
                    && !LEFT_OUT.contains(&line.rsplit(' ').next().unwrap_or("")))
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected import failures: {unexpected:#?}\nstderr: {}",
        result.err
    );
    assert!(result.out.contains(&format!("IMPORTED {}", names.len())));
    // Importing every module is quiet apart from the script's own prints.
    assert!(!result.err.contains("ERROR:"), "{}", result.err);
}

#[test]
fn bundle_modules_are_served_from_the_piped_bytes() {
    let dir = scratch("bundle");
    let path = dir.join("app.pyz");
    bundle(
        &path,
        &[
            (
                "__main__.py",
                b"import pkg.mod\nprint(pkg.mod.VALUE, __file__, __name__)\n",
            ),
            ("pkg/__init__.py", b""),
            ("pkg/mod.py", b"VALUE = 'BUNDLE_OK'\n"),
            ("README.txt", b"data files are allowed"),
        ],
    );
    let result = run_with(
        &runner(),
        &frame(&fs::read(&path).unwrap()),
        "valid",
        &[],
        None,
    );
    assert_eq!(
        (result.out.as_str(), result.err.as_str()),
        ("BUNDLE_OK ck-script:main __main__\n", "")
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn bundle_holding_a_native_file_is_refused_at_load() {
    let dir = scratch("native-bundle");
    let cases: [(&str, &[u8]); 3] = [
        ("pkg/fast.cpython-313-darwin.so", b"anything"),
        ("pkg/fast.pyd", b"anything"),
        ("pkg/blob.bin", b"\x7fELF\x02\x01\x01"),
    ];
    for (name, data) in cases {
        let path = dir.join("native.pyz");
        bundle(
            &path,
            &[
                ("__main__.py", b"print('EVALUATED')\n"),
                ("pkg/__init__.py", b""),
                (name, data),
            ],
        );
        let result = run_with(
            &runner(),
            &frame(&fs::read(&path).unwrap()),
            "valid",
            &[],
            None,
        );
        assert_eq!((result.code, result.out.as_str()), (1, ""), "{name}");
        assert_eq!(
            result.err,
            format!("script_bundle_refused: native file {name}\n")
        );
        let checked = check(&path);
        assert_eq!(
            (checked.code, checked.err),
            (1, format!("script_bundle_refused: native file {name}\n"))
        );
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_mode_warns_on_loads_without_running_anything() {
    let dir = scratch("check");
    let file = dir.join("script.py");
    fs::write(
        &file,
        r#"print("EVALUATED")
exec(open("other.py").read())
eval(data)
exec("print('literal code is in the script')")
import importlib.util
spec = importlib.util.spec_from_file_location("x", "x.py")
import runpy
runpy.run_path("x.py")
import ctypes
try:
    import cffi
except ImportError:
    cffi = None
"#,
    )
    .unwrap();
    let result = check(&file);
    assert_eq!(
        (result.code, result.out.as_str()),
        (0, ""),
        "{}",
        result.err
    );
    let expected = [
        "script_load_warning: exec at line 2: runs code that is not in the approved script",
        "script_load_warning: eval at line 3: runs code that is not in the approved script",
        "script_load_warning: spec_from_file_location at line 6: loads a module from a file outside the approved script",
        "script_load_warning: run_path at line 8: runs a file outside the approved script",
        "script_load_warning: ctypes at line 9: loads native code; it is not available in this runner",
        "script_load_warning: cffi at line 11: loads native code; it is not available in this runner",
        "script_import_warning: cffi at line 11: not in the approved runner; this import fails at run time",
    ];
    assert_eq!(result.err.lines().collect::<Vec<_>>(), expected);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_mode_refuses_imports_the_runner_cannot_serve() {
    let dir = scratch("check-refused");
    let file = dir.join("script.py");
    fs::write(&file, "print('EVALUATED')\nimport json, requests\nfrom yaml import safe_load\nfrom . import sibling\n").unwrap();
    let result = check(&file);
    assert_eq!(result.code, 1);
    assert_eq!(result.out, "");
    assert_eq!(
        result.err,
        "script_import_refused: requests\nscript_import_refused: yaml\n"
    );
    let _ = fs::remove_dir_all(dir);
}
