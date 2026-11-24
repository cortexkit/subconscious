use std::time::Duration;

/// Reserve time for the daemon to queue and deliver an operator prompt and
/// recover its provider. Even older daemons and log mode get the same budget:
/// the wire does not advertise whether a privileged request will prompt.
pub(super) fn operator() -> Duration {
    #[cfg(feature = "test-support")]
    if let Some(millis) = std::env::var_os("CK_TEST_OPERATOR_RESPONSE_BUDGET_MS")
        .and_then(|value| value.to_str()?.parse::<u64>().ok())
    {
        return Duration::from_millis(millis);
    }
    super::OPERATOR_RESPONSE_BUDGET
}
