use std::io::Read;

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    subc_os::privacy_identity::trampoline_main_for_test(&args);
    match args.first().and_then(|s| s.to_str()) {
        #[cfg(target_os = "macos")]
        Some("responsibility") => {
            let (responsible, parent_responsible, parent, _) =
                subc_os::privacy_identity::privacy_observation_for_test().unwrap();
            println!(
                "{} {responsible} {parent_responsible} {parent}",
                std::process::id()
            );
        }
        Some("io") => {
            assert_eq!(
                &args[1..],
                &["first argument", "--second", "third"].map(std::ffi::OsString::from)
            );
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).unwrap();
            println!("stdin={input}");
            println!("env={}", std::env::var("SUBC_COMMAND_VALUE").unwrap());
            // Canonical on both sides: Windows spells a canonical path with a `\\?\`
            // prefix and macOS resolves /var to /private/var, while current_dir
            // reports the plain form.
            println!(
                "cwd={}",
                std::env::current_dir()
                    .unwrap()
                    .canonicalize()
                    .unwrap()
                    .display()
            );
            eprintln!("child stderr");
        }
        Some("environment") => {
            let mut vars: Vec<_> = std::env::vars().collect();
            vars.sort();
            for (key, value) in vars {
                println!("{key}={value}");
            }
        }
        Some("exit") => std::process::exit(args[1].to_str().unwrap().parse().unwrap()),
        _ => std::process::exit(2),
    }
}
