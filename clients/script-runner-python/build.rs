//! Links CPython into ck-script-runner-python from an extracted python-build-standalone archive.
//!
//! Everything CPython-specific comes from the archive's `PYTHON.json`: the core object files, the
//! object files and libraries of each native module, and the system libraries they need. Another
//! source of CPython (a source build for a platform without usable archives) can feed this same
//! step by providing the same layout and `PYTHON.json`.
//!
//! Inputs, both required (scripts/release/build-script-runner-python.sh sets them):
//! - `CK_PYTHON_DIST`: the archive's extracted `python/` directory;
//! - `CK_PYTHON_HOST`: a CPython of the same minor version that runs on the build machine, used to
//!   compile the standard library to bytecode.

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// CPython's own bootstrap modules (Modules/Setup.bootstrap): the interpreter can't start
/// or import its standard library without them.
const INTERPRETER_MODULES: &[&str] = &[
    "_abc", "_codecs", "_collections", "_functools", "_io", "_locale", "_operator", "_signal",
    "_sre", "_stat", "_suggestions", "_symtable", "_sysconfig", "_thread", "_tracemalloc",
    "_typing", "_weakref", "atexit", "errno", "faulthandler", "itertools", "posix", "pwd", "time",
];

/// Standard-library extensions linked on every platform. The rule for this set: an extension is
/// linked when it is self-contained (its native dependencies are static in the archive), loads
/// no file, and needs no macOS entitlement. That leaves out `_ctypes` (it loads native libraries
/// chosen at run time), curses, readline, dbm and gdbm, tkinter, and the `_test*`/`xx*` modules.
const STDLIB_MODULES: &[&str] = &[
    "_asyncio", "_bisect", "_blake2", "_bz2", "_codecs_cn", "_codecs_hk", "_codecs_iso2022",
    "_codecs_jp", "_codecs_kr", "_codecs_tw", "_contextvars", "_csv", "_datetime", "_decimal",
    "_hashlib", "_heapq", "_json", "_lsprof", "_lzma", "_md5", "_multibytecodec", "_opcode",
    "_pickle", "_queue", "_random", "_sha1", "_sha2", "_sha3", "_socket", "_sqlite3", "_ssl",
    "_statistics", "_struct", "_zoneinfo", "array", "binascii", "cmath", "math", "mmap",
    "pyexpat", "select", "unicodedata", "zlib",
];
const UNIX_MODULES: &[&str] =
    &["_posixshmem", "_posixsubprocess", "fcntl", "grp", "resource", "syslog", "termios"];
/// macOS only: urllib's proxy lookup, through the SystemConfiguration system framework.
const MACOS_MODULES: &[&str] = &["_scproxy"];
const WINDOWS_MODULES: &[&str] = &["_overlapped", "_winapi"];
/// Linked when the archive has them; not every platform's build provides them.
const OPTIONAL_MODULES: &[&str] = &["_uuid"];

/// Never linked, even if a future archive marks it as required.
const REFUSED_MODULES: &[&str] = &["_ctypes"];

fn required_env(name: &str) -> PathBuf {
    println!("cargo:rerun-if-env-changed={name}");
    match env::var_os(name) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => panic!(
            "{name} is not set: build with scripts/release/build-script-runner-python.sh, \
             which extracts the pinned CPython archive and names it"
        ),
    }
}

fn text<'a>(value: &'a Value, what: &str) -> &'a str {
    value.as_str().unwrap_or_else(|| panic!("PYTHON.json: {what} is not a string"))
}

/// Link inputs collected from PYTHON.json, in the order they were named.
#[derive(Default)]
struct LinkPlan {
    objects: Vec<PathBuf>,
    static_libraries: Vec<PathBuf>,
    system_libraries: Vec<String>,
    frameworks: Vec<String>,
}

impl LinkPlan {
    fn add_links(&mut self, dist: &Path, links: &Value, owner: &str) {
        for link in links.as_array().into_iter().flatten() {
            let name = text(&link["name"], "link name");
            if let Some(path) = link["path_static"].as_str() {
                let path = dist.join(path);
                if !self.static_libraries.contains(&path) {
                    self.static_libraries.push(path);
                }
            } else if link["framework"].as_bool() == Some(true) {
                if !self.frameworks.iter().any(|known| known == name) {
                    self.frameworks.push(name.to_string());
                }
            } else if link["system"].as_bool() == Some(true) {
                if !self.system_libraries.iter().any(|known| known == name) {
                    self.system_libraries.push(name.to_string());
                }
            } else {
                panic!("{owner} needs the dynamic library {name}; the runner must be one self-contained executable");
            }
        }
    }
}

fn main() {
    let dist = required_env("CK_PYTHON_DIST");
    let host_python = required_env("CK_PYTHON_HOST");
    let manifest = dist.join("PYTHON.json");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed=src/embed.c");
    println!("cargo:rerun-if-changed=tools/pack_stdlib.py");
    let info: Value = serde_json::from_slice(
        &fs::read(&manifest).unwrap_or_else(|error| panic!("cannot read {}: {error}", manifest.display())),
    )
    .unwrap_or_else(|error| panic!("{} is not valid JSON: {error}", manifest.display()));
    let minor = text(&info["python_major_minor_version"], "python_major_minor_version");
    assert_eq!(minor, "3.13", "the runner embeds CPython 3.13, but {} is {minor}", manifest.display());
    let build = &info["build_info"];
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");

    let mut plan = LinkPlan::default();
    // The archive's own init table names every module it built, _ctypes included; the runner
    // generates its own table below instead.
    let archive_inittab = build["inittab_object"].as_str();
    for object in build["core"]["objs"].as_array().expect("PYTHON.json: build_info.core.objs") {
        let object = text(object, "core object");
        if Some(object) != archive_inittab {
            plan.objects.push(dist.join(object));
        }
    }
    plan.add_links(&dist, &build["core"]["links"], "the CPython core");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let mut wanted: Vec<&str> = INTERPRETER_MODULES.iter().chain(STDLIB_MODULES).copied().collect();
    match target_os.as_str() {
        "windows" => wanted.extend(WINDOWS_MODULES),
        "macos" => wanted.extend(UNIX_MODULES.iter().chain(MACOS_MODULES)),
        _ => wanted.extend(UNIX_MODULES),
    }
    let extensions = build["extensions"].as_object().expect("PYTHON.json: build_info.extensions");
    for name in &wanted {
        assert!(extensions.contains_key(*name), "the archive has no {name} module");
    }
    wanted.extend(OPTIONAL_MODULES.iter().filter(|name| extensions.contains_key(**name)));
    let mut inittab = BTreeMap::new();
    for (name, variants) in extensions {
        let variants = variants.as_array().expect("PYTHON.json: extension variants");
        let variant = variants
            .iter()
            .find(|variant| variant["variant"].as_str() == Some("default"))
            .or_else(|| variants.first())
            .unwrap_or_else(|| panic!("PYTHON.json: {name} has no variant"));
        let in_core = variant["in_core"].as_bool() == Some(true);
        let required = variant["required"].as_bool() == Some(true);
        if REFUSED_MODULES.contains(&name.as_str()) || !(in_core || required || wanted.contains(&name.as_str())) {
            continue;
        }
        if !in_core {
            for object in variant["objs"].as_array().into_iter().flatten() {
                plan.objects.push(dist.join(text(object, "extension object")));
            }
            plan.add_links(&dist, &variant["links"], name);
        }
        if let Some(init) = variant["init_fn"].as_str() {
            inittab.insert(name.clone(), init.to_string());
        }
    }

    // The interpreter's init table: every built-in module and its init function. CPython reads
    // the `_PyImport_Inittab` symbol, normally defined by the archive's Modules/config.c.
    let mut source = String::from("#include \"Python.h\"\n\n");
    for init in inittab.values().filter(|init| *init != "NULL") {
        writeln!(source, "extern PyObject *{init}(void);").unwrap();
    }
    source.push_str("\nstruct _inittab _PyImport_Inittab[] = {\n");
    for (name, init) in &inittab {
        writeln!(source, "    {{\"{name}\", {init}}},").unwrap();
    }
    source.push_str("    {0, 0}\n};\n");
    let inittab_c = out.join("inittab.c");
    fs::write(&inittab_c, source).expect("write inittab.c");

    let include = dist.join(text(&info["python_paths"]["include"], "python_paths.include"));
    let mut embed = cc::Build::new();
    embed.file("src/embed.c").file(&inittab_c).include(&include).warnings(true).cargo_metadata(false);
    embed.compile("ck_embed");
    let embed_library = out.join(if windows { "ck_embed.lib" } else { "libck_embed.a" });

    // Everything CPython goes to the runner binary only, never to test binaries.
    let link = |argument: &dyn AsRef<std::ffi::OsStr>| {
        println!("cargo:rustc-link-arg-bins={}", Path::new(argument.as_ref()).display());
    };
    link(&embed_library);
    for object in &plan.objects {
        link(object);
    }
    for library in &plan.static_libraries {
        link(library);
    }
    for library in &plan.system_libraries {
        link(&format!("-l{library}"));
    }
    for framework in &plan.frameworks {
        link(&"-framework");
        link(framework);
    }

    // The archive's objects may be LLVM bitcode, linked by an LLVM lld of the same major
    // version. rustc ships one (rust-lld); use it when the archive's LLVM matches rustc's.
    if let Some(format) = build["object_file_format"].as_str().and_then(|format| format.strip_prefix("llvm-bitcode:")) {
        let archive_major = format.split('.').next().unwrap_or_default();
        let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
        let version = Command::new(&rustc).arg("-vV").output().expect("run rustc -vV");
        let version = String::from_utf8_lossy(&version.stdout);
        let rustc_llvm = version.lines().find_map(|line| line.strip_prefix("LLVM version: ")).unwrap_or_default();
        assert_eq!(
            rustc_llvm.split('.').next().unwrap_or_default(),
            archive_major,
            "the archive's objects are LLVM {format} bitcode, but rustc's LLVM is {rustc_llvm}: \
             use a Rust toolchain whose rust-lld has the same LLVM major version"
        );
        let sysroot = Command::new(&rustc).args(["--print", "sysroot"]).output().expect("run rustc --print sysroot");
        let sysroot = String::from_utf8_lossy(&sysroot.stdout).trim().to_string();
        let host = env::var("HOST").expect("HOST");
        let gcc_ld = Path::new(&sysroot).join("lib/rustlib").join(host).join("bin/gcc-ld");
        assert!(gcc_ld.is_dir(), "rust-lld is missing from {}", gcc_ld.display());
        link(&"-fuse-ld=lld");
        link(&format!("-B{}", gcc_ld.display()));
    }

    let table = out.join("stdlib.bin");
    let status = Command::new(&host_python)
        .arg("-I")
        .arg("tools/pack_stdlib.py")
        .arg(&dist)
        .arg(&table)
        .status()
        .unwrap_or_else(|error| panic!("cannot run CK_PYTHON_HOST {}: {error}", host_python.display()));
    assert!(status.success(), "compiling the standard library failed");

    println!("cargo:rustc-env=CK_PYTHON_VERSION={}", text(&info["python_version"], "python_version"));
    // Lets the tests drive the runner with the shared pipe harness without a system Python, and
    // list the standard library the runner ships.
    println!("cargo:rustc-env=CK_TEST_PYTHON={}", host_python.display());
    let stdlib = dist.join(text(&info["python_paths"]["stdlib"], "python_paths.stdlib"));
    println!("cargo:rustc-env=CK_TEST_STDLIB={}", stdlib.display());
}
