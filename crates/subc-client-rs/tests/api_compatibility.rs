use std::{path::PathBuf, time::Duration};
use subc_client_rs::{ConsumerError, ConsumerOptions, RetryBackoff};

#[test]
fn original_consumer_options_accept_full_literal() {
    let options = ConsumerOptions {
        handshake_timeout: Duration::from_secs(2),
        call_timeout: Duration::from_secs(1),
        reconnect_backoff: RetryBackoff {
            base: Duration::from_millis(5),
            cap: Duration::from_millis(10),
            max_attempts: 4,
        },
        restored_debounce: Duration::ZERO,
        liveness_probe_window: Duration::from_secs(2),
    };
    assert_eq!(options.call_timeout, Duration::from_secs(1));
}

fn original_error_name(error: ConsumerError) -> &'static str {
    match error {
        ConsumerError::Discovery { .. } => "discovery",
        ConsumerError::ConnectionFile { .. } => "connection_file",
        ConsumerError::NoEndpoint { .. } => "no_endpoint",
        ConsumerError::Connect { .. } => "connect",
        ConsumerError::Auth { .. } => "auth",
        ConsumerError::Closed => "closed",
    }
}

#[test]
fn original_consumer_error_remains_exhaustively_matchable() {
    let error = ConsumerError::NoEndpoint {
        path: PathBuf::from("connection.json"),
    };
    assert_eq!(original_error_name(error), "no_endpoint");
}
