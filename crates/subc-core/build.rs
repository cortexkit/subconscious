fn main() {
    println!("cargo:rustc-check-cfg=cfg(loom)");
    if std::env::var_os("CARGO_FEATURE_LOOM").is_some() {
        // Scope loom instrumentation to subc-core. A workspace-wide `RUSTFLAGS=--cfg loom`
        // also disables Tokio's net/process modules, so it cannot compile this daemon crate.
        println!("cargo:rustc-cfg=loom");
    }
}
