fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    subc_os::privacy_identity::trampoline_main_for_test(&args);
    std::process::exit(2);
}
