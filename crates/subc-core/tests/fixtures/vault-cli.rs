#![forbid(unsafe_code)]

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version") => println!("ck-auth 0.8.0"),
        Some("grant") => println!("already granted (no change)"),
        _ => std::process::exit(2),
    }
}
