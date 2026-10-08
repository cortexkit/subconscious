//! Person-present provider smoke check, not an authorization API.
//!
//! Run without a timer to approve/cancel or choose the password fallback. A
//! timer demonstrates withdrawal while a sheet/dialog is showing. This bypasses
//! the daemon queue and therefore prints no daemon audit line.

use std::{sync::mpsc, time::Duration};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(summary) = args.next() else {
        eprintln!("usage: ckdev-presence <summary> [withdraw-after-seconds]");
        std::process::exit(2);
    };
    if summary.is_empty() || summary.contains('\0') {
        eprintln!("summary must not be empty or contain NUL");
        std::process::exit(2);
    }
    let withdraw_after = args.next().map(|seconds| {
        seconds.parse::<u64>().unwrap_or_else(|_| {
            eprintln!("withdraw-after-seconds must be an unsigned integer");
            std::process::exit(2);
        })
    });
    if args.next().is_some() {
        eprintln!("too many arguments");
        std::process::exit(2);
    }
    let sentence = format!("ckdev-presence asks: {summary} (requested by a local program)");
    println!("reason: {sentence}");
    let (finished, completion) = mpsc::channel::<()>();
    let outcome = subc_presence::prompt(
        &sentence,
        Box::new(move |handle| {
            println!("withdraw handle ready");
            if let Some(seconds) = withdraw_after {
                std::thread::spawn(move || {
                    if completion
                        .recv_timeout(Duration::from_secs(seconds))
                        .is_err()
                    {
                        println!("withdrawing after {seconds} s");
                        handle.withdraw();
                    }
                });
            }
        }),
    );
    let _ = finished.send(());
    println!("outcome: {outcome:?}");
}
