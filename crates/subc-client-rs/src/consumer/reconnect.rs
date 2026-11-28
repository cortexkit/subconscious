use super::*;

impl Shared {
    pub(super) async fn reconnect_with_retry(
        self: &Arc<Self>,
        reconnect_generation: u64,
    ) -> Result<(), ConsumerError> {
        self.reconnect_with(reconnect_generation, || {
            open_connection(
                &self.connection_file,
                self.opts.handshake_timeout,
                self.transport,
                self.operator_app_candidates.as_deref(),
            )
        })
        .await
    }

    async fn reconnect_with<F: Future<Output = Result<OpenedConnection, ConsumerError>>>(
        self: &Arc<Self>,
        reconnect_generation: u64,
        mut open: impl FnMut() -> F,
    ) -> Result<(), ConsumerError> {
        let mut last_error: Option<ConsumerError> = None;
        for attempt in 1..=self.opts.reconnect_backoff.max_attempts {
            if self.close_token.is_cancelled() {
                return Err(ConsumerError::Closed);
            }
            if !self.reconnect_attempt_is_current(reconnect_generation) {
                return Ok(());
            }
            crate::registration::check_consumer_dial()?;

            match open().await {
                Ok(opened) => {
                    // A newer drop may have installed its own attempt while this
                    // connection was opening. Do not let the stale attempt replace it.
                    if !self.reconnect_attempt_is_current(reconnect_generation) {
                        return Ok(());
                    }
                    let (generation, epoch) = self.install_reconnected(opened)?;
                    self.schedule_restored(generation, epoch);
                    return Ok(());
                }
                Err(err) => {
                    let transient = is_reconnect_transient(&err);
                    last_error = Some(err);
                    if !transient || attempt >= self.opts.reconnect_backoff.max_attempts {
                        break;
                    }
                    // Jittered like the route.open retries, which share this backoff.
                    let delay =
                        self.jittered(self.opts.reconnect_backoff.delay_after_attempt(attempt));
                    tokio::select! {
                        () = self.close_token.cancelled() => return Err(ConsumerError::Closed),
                        () = crate::registration::consumer_dial_stopped() => return Err(ConsumerError::Closed),
                        () = sleep(delay) => {}
                    }
                }
            }
        }
        Err(last_error.unwrap_or(ConsumerError::Closed))
    }
}

pub(super) fn is_reconnect_transient(err: &ConsumerError) -> bool {
    if let ConsumerError::Connect { source, .. } = err {
        if let Some(claim) = source
            .get_ref()
            .and_then(|source| source.downcast_ref::<crate::ClaimError>())
        {
            return matches!(claim, crate::ClaimError::Refused { code } if error_codes::is_retryable_claim(code));
        }
    }
    match err {
        ConsumerError::Connect { source, .. } => matches!(
            source.kind(),
            io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::TimedOut
                | io::ErrorKind::NotConnected
                | io::ErrorKind::AddrNotAvailable
        ),
        ConsumerError::ConnectionFile { source, .. } => match source {
            ConnectionFileError::Io { source, .. } => source.kind() == io::ErrorKind::NotFound,
            _ => false,
        },
        // Auth failure is transient during reconnect: the daemon rotates its key
        // on every restart, and with a fixed port a client racing the restart can
        // read the pre-rotation connection file yet still connect — the proof
        // mismatch then means "stale key mid-rotation", not "impostor". Each
        // retry re-reads the connection file to pick up the rotated key. The
        // server still has to prove it knows that key before the client sends
        // its own proof. First-connect auth failures stay permanent: connect()
        // surfaces them directly without entering the reconnect classifier.
        ConsumerError::Auth { .. } => true,
        ConsumerError::Discovery { .. }
        | ConsumerError::NoEndpoint { .. }
        | ConsumerError::Closed => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reconnect_stops_after_terminal_claim_refusal() {
        let shared = reconnecting_consumer();
        let mut attempts = 0;
        let result = shared
            .reconnect_with(1, || {
                attempts += 1;
                std::future::ready(Err(ConsumerError::claim(crate::ClaimError::Refused {
                    code: error_codes::CLAIM_MINT_NOT_ALLOWED.into(),
                })))
            })
            .await;
        assert!(
            matches!(result, Err(ConsumerError::Connect { source, .. }) if matches!(
                source.get_ref().and_then(|error| error.downcast_ref::<crate::ClaimError>()),
                Some(crate::ClaimError::Refused { code }) if code == error_codes::CLAIM_MINT_NOT_ALLOWED
            ))
        );
        assert_eq!(attempts, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_retries_an_ordinary_connection_refused() {
        let shared = reconnecting_consumer();
        let mut attempts = 0;
        let result = shared
            .reconnect_with(1, || {
                attempts += 1;
                std::future::ready(Err(if attempts == 1 {
                    ConsumerError::Connect {
                        path: PathBuf::new(),
                        endpoint: "test listener".into(),
                        source: io::ErrorKind::ConnectionRefused.into(),
                    }
                } else {
                    ConsumerError::NoEndpoint {
                        path: "second attempt".into(),
                    }
                }))
            })
            .await;
        assert!(
            matches!(result, Err(ConsumerError::NoEndpoint { path }) if path == Path::new("second attempt"))
        );
        assert_eq!(attempts, 2);
    }

    fn reconnecting_consumer() -> Arc<Shared> {
        let shared = Arc::new(Shared::with_transport(
            PathBuf::new(),
            ConsumerOptions::default(),
            ConsumerTransport::Tcp,
        ));
        shared.lock_inner().reconnect = ReconnectState::Inline { generation: 1 };
        shared
    }
}
