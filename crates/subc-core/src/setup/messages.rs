//! Route installer messages without writing into an active terminal UI.

use std::cell::RefCell;

thread_local! {
    static KEPT: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

pub(super) fn keep<T>(run: impl FnOnce() -> T) -> (T, Vec<String>) {
    struct Restore(Option<Vec<String>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            KEPT.replace(self.0.take());
        }
    }
    let _restore = Restore(KEPT.replace(Some(Vec::new())));
    let result = run();
    (result, KEPT.take().expect("installer output is being kept"))
}

pub(super) fn warning(line: String) {
    emit(line, |line| eprintln!("warning: {line}"));
}

pub(super) fn notice(line: String) {
    emit(line, |line| crate::cli_output::println!("{line}"));
}

fn emit(line: String, print: impl FnOnce(String)) {
    let outside_the_app = KEPT.with(|kept| {
        if let Some(messages) = kept.borrow_mut().as_mut() {
            messages.push(line);
            None
        } else {
            Some(line)
        }
    });
    if let Some(line) = outside_the_app {
        print(line);
    }
}
