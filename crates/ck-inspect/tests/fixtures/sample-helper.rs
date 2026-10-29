fn main() {
    if let Some(path) = std::env::var_os("INSPECT_FIXTURE_PID_PATH") {
        std::fs::write(path, std::process::id().to_string()).unwrap();
    }
    let _worker = std::thread::Builder::new()
        .name("profile-worker".into())
        .spawn(|| loop {
            std::hint::black_box((0..100_000u64).fold(0u64, u64::wrapping_add));
        })
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(60));
}
