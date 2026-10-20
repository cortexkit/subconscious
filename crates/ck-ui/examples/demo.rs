//! Runs the app on the built-in fixture with time moving, connected to
//! nothing: `cargo run -p ck-ui --example demo`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ck_ui::{RunOptions, Snapshot};
use tokio::sync::watch;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    // The clock starts at the fixture's moment and runs in real time, so
    // uptimes and ages advance from what the fixture shows.
    let start = Snapshot::fixture_now();
    let began = Instant::now();
    let clock = move || start.add_millis(began.elapsed().as_millis() as i64);

    let (tx, rx) = watch::channel(Snapshot::fixture());
    tokio::spawn(async move {
        let mut seconds = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            seconds += 1;
            let now = clock();
            // Pretend the sources keep reporting: data arrives every second
            // and every module is health-checked every five.
            tx.send_modify(|s| {
                s.modules.received_at = now;
                if seconds.is_multiple_of(5) {
                    for m in &mut s.modules.data {
                        m.health_checked_at = now;
                    }
                }
            });
            if tx.is_closed() {
                break;
            }
        }
    });

    ck_ui::run_with(
        rx,
        RunOptions {
            clock: Arc::new(clock),
        },
    )
    .await
}
