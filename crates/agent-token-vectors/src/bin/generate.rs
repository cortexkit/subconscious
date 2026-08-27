use std::{env, fs, path::PathBuf};

fn main() {
    let output = output_path(env::args().skip(1).collect()).unwrap_or_else(|message| {
        eprintln!("{message}");
        std::process::exit(2);
    });
    let bytes = agent_token_vectors::render_corpus();
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).expect("create corpus directory");
    }
    fs::write(&output, bytes).expect("write corpus");
    println!("wrote {}", output.display());
}

fn output_path(args: Vec<String>) -> Result<PathBuf, String> {
    match args.as_slice() {
        [] => Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("vectors/agent_token_vectors_v1.json")),
        [flag, path] if flag == "--output" => Ok(PathBuf::from(path)),
        _ => Err("usage: generate [--output PATH]".into()),
    }
}
