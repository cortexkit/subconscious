fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    #[cfg(target_os = "macos")]
    if args.first().is_some_and(|arg| arg == "__disclaim-exec")
        && !args.iter().any(|arg| arg == "--probe")
    {
        if let Some(path) = std::env::var_os("SUBC_TEST_PRIVACY_EXEC_BARRIER") {
            use std::io::{Read, Write};
            let mut barrier = std::net::TcpStream::connect(path.to_str().unwrap()).unwrap();
            barrier
                .set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .unwrap();
            // The fixture is now running the trampoline image. Keep its exec
            // acknowledgement open until the test has sampled the roster.
            barrier.write_all(b"R").unwrap();
            let mut release = [0];
            barrier.read_exact(&mut release).unwrap();
            assert_eq!(&release, b"X");
        }
    }
    subc_os::privacy_identity::trampoline_main_for_test(&args);
    std::process::exit(2);
}
