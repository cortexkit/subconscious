#[cfg(unix)]
fn main() {
    use std::os::unix::process::CommandExt;

    rustix::process::umask(rustix::fs::Mode::empty());
    let mut args = std::env::args_os().skip(1);
    let Some(program) = args.next() else {
        eprintln!("mode-umask-helper needs a child program");
        std::process::exit(2);
    };
    let mut child = std::process::Command::new(program);
    child.args(args).envs(std::env::vars_os());
    let error = child.exec();
    eprintln!("mode-umask-helper could not run its child: {error}");
    std::process::exit(127);
}

#[cfg(not(unix))]
fn main() {}
