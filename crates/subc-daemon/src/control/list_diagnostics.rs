use std::{future::Future, time::Duration};

tokio::task_local! {
    static DEADLINE: tokio::time::Instant;
}

/// All optional diagnostics in one list share a deadline, rather than spending
/// another per-module timeout for every module in a large roster.
pub(crate) async fn scope<T>(read: impl Future<Output = T>) -> T {
    DEADLINE
        .scope(
            tokio::time::Instant::now() + Duration::from_millis(100),
            read,
        )
        .await
}

pub(crate) fn deadline() -> Option<tokio::time::Instant> {
    DEADLINE.try_with(|deadline| *deadline).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn list_diagnostics_share_one_budget_and_skip_unready_readings() {
        scope(async {
            let deadline = deadline().unwrap();
            assert_eq!(
                deadline.duration_since(tokio::time::Instant::now()),
                Duration::from_millis(100)
            );
            let reading = tokio::time::timeout(
                Duration::from_secs(1),
                tokio::time::timeout_at(deadline, std::future::pending::<()>()),
            )
            .await
            .expect("a diagnostic that never returns must not hold up the list");
            assert!(reading.is_err());
            assert_eq!(
                super::deadline(),
                Some(deadline),
                "later diagnostics must not start a new budget"
            );
            assert!(tokio::time::Instant::now() >= deadline);
        })
        .await;
    }
}
