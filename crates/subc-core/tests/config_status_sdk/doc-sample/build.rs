use std::{env, fs, path::PathBuf};

fn main() {
    let source = PathBuf::from("../../../../subc-client-rs/src/config.rs");
    println!("cargo:rerun-if-changed={}", source.display());
    let source = fs::read_to_string(source).unwrap();
    let sample = source
        .split_once("//! ```no_run\n")
        .unwrap()
        .1
        .split_once("//! ```")
        .unwrap()
        .0;
    let lines: Vec<_> = sample
        .lines()
        .map(|line| line.strip_prefix("//! ").unwrap_or(""))
        .filter(|line| !line.starts_with("# "))
        .collect();
    let output = format!(
        "{}\nconst ADOPTION_LINES: usize = {};\n",
        lines.join("\n"),
        lines.len()
    );
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("sample.rs"),
        output,
    )
    .unwrap();
}
