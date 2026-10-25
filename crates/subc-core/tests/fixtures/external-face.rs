use std::io::Write;

fn main() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--ck-domain")
    {
        println!("example face");
        return;
    }
    std::io::stdout()
        .write_all(b"\x1b[31mface\x1b[0m\n{\"schema\":\"face/v9\",\"ok\":true}\n")
        .unwrap();
    std::io::stderr().write_all(b"face error\n").unwrap();
    std::process::exit(7);
}
