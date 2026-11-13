//! Native launch behaviors for configuration-gate tests. Built only with test-support.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

fn sleep_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(600));
    }
}

fn wait_for(path: &str) {
    while !Path::new(path).exists() {
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn log(path: &str, value: &str) {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(value.as_bytes())
        .unwrap();
}

fn exec_sleep() -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        panic!(
            "exec sleep failed: {}",
            std::process::Command::new("/bin/sleep").arg("600").exec()
        );
    }
    #[cfg(not(unix))]
    panic!("exec-sleep is a Unix-only fixture mode");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).expect("fixture mode") {
        "sleep" => sleep_forever(),
        "exit23" => std::process::exit(23),
        "exit17" => std::process::exit(17),
        "exec-sleep" => exec_sleep(),
        "wait-exec" => {
            wait_for(&args[1]);
            exec_sleep();
        }
        "mark-first-exit23" => {
            log(&args[2], "started\n");
            if !Path::new(&args[1]).exists() {
                File::create(&args[1]).unwrap();
                std::process::exit(23);
            }
            sleep_forever();
        }
        "fifo-exit23" => {
            log(&args[2], "started\n");
            let mut release = String::new();
            BufReader::new(File::open(&args[1]).unwrap())
                .read_line(&mut release)
                .unwrap();
            std::process::exit(23);
        }
        "mark-wait-exit0" => {
            log(&args[2], "started\n");
            wait_for(&args[1]);
        }
        "mark-sleep" => {
            log(&args[2], "started\n");
            sleep_forever();
        }
        "pid-sleep" => {
            log(&args[2], &format!("{}\n", std::process::id()));
            sleep_forever();
        }
        mode => panic!("unknown fixture mode: {mode}"),
    }
}
