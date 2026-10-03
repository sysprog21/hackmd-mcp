//! Confirming writes that `HackMD` applies asynchronously.

use std::time::Duration;

use super::HackmdError;

/// How long a write is given to become visible, and the first pause between
/// reads. The pause doubles so the window is covered in a handful of requests
/// rather than ten: `HackMD` allows 100 requests per five minutes, and a
/// foldered note creation spends several of them before ever polling.
const READBACK_WINDOW: Duration = Duration::from_secs(2);
const READBACK_FIRST_DELAY: Duration = Duration::from_millis(100);
/// Each read-back downloads the whole note again, so a large body gets more
/// time: one more second per MiB written, up to this ceiling.
const READBACK_MAX_WINDOW: Duration = Duration::from_secs(30);

fn readback_window(written_bytes: usize) -> Duration {
    let mib = u32::try_from(written_bytes / (1024 * 1024)).unwrap_or(u32::MAX);
    READBACK_WINDOW
        .saturating_add(Duration::from_secs(1).saturating_mul(mib))
        .min(READBACK_MAX_WINDOW)
}

/// What a read-back saw, and whether it satisfied the caller.
pub(crate) struct Readback<T> {
    pub(crate) value: T,
    pub(crate) confirmed: bool,
}

/// Polls with the window sized to `written_bytes` and the standard first
/// delay.
pub(super) async fn poll_readback_sized<T, Fut>(
    written_bytes: usize,
    fetch: impl FnMut() -> Fut,
    accepted: impl Fn(&T) -> bool,
) -> Result<Readback<T>, HackmdError>
where
    Fut: std::future::Future<Output = Result<T, HackmdError>>,
{
    poll_readback_with_policy(
        fetch,
        accepted,
        readback_window(written_bytes),
        READBACK_FIRST_DELAY,
    )
    .await
}

/// Re-reads a just-written resource until `accepted` holds.
///
/// `HackMD` applies some writes asynchronously, so the first read after a write
/// can still answer with the previous value. When the window expires the last
/// observation is still returned, with `confirmed: false`: a caller that treats
/// that as failure has its error, and one that wants to report the current
/// state has it without paying for another request. Callers go through
/// `HackmdClient::poll_readback`, which sizes the window.
async fn poll_readback_with_policy<T, Fut>(
    mut fetch: impl FnMut() -> Fut,
    accepted: impl Fn(&T) -> bool,
    window: Duration,
    first_delay: Duration,
) -> Result<Readback<T>, HackmdError>
where
    Fut: std::future::Future<Output = Result<T, HackmdError>>,
{
    let started = tokio::time::Instant::now();
    let deadline = started + window;
    let mut delay = first_delay;
    let mut attempts = 0_u8;
    let outcome = loop {
        attempts = attempts.saturating_add(1);
        let Ok(result) = tokio::time::timeout_at(deadline, fetch()).await else {
            break Err(HackmdError::ReadbackTimeout);
        };
        let value = match result {
            Ok(value) => value,
            Err(error) => break Err(error),
        };
        if accepted(&value) {
            break Ok(Readback {
                value,
                confirmed: true,
            });
        }
        if tokio::time::Instant::now() + delay >= deadline {
            break Ok(Readback {
                value,
                confirmed: false,
            });
        }
        tokio::time::sleep(delay).await;
        delay = delay.saturating_mul(2);
    };
    crate::retry::record_readback(attempts, started.elapsed());
    outcome
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Value;

    use super::HackmdError;

    #[tokio::test]
    async fn readback_policy_bounds_the_fetch_itself() {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            super::poll_readback_with_policy(
                std::future::pending::<Result<Value, HackmdError>>,
                |_| true,
                Duration::from_millis(10),
                Duration::from_millis(1),
            ),
        )
        .await
        .expect("the readback policy must bound a stuck fetch");

        assert!(matches!(result, Err(HackmdError::ReadbackTimeout)));
    }

    #[tokio::test]
    async fn readback_policy_returns_the_last_bounded_observation() {
        let result = super::poll_readback_with_policy(
            || std::future::ready(Ok::<_, HackmdError>("old")),
            |value| *value == "new",
            Duration::from_millis(10),
            Duration::from_millis(20),
        )
        .await
        .expect("a completed fetch should remain observable");

        assert_eq!(result.value, "old");
        assert!(!result.confirmed);
    }

    #[test]
    fn the_readback_window_grows_with_the_body_written() {
        assert_eq!(super::readback_window(0), Duration::from_secs(2));
        assert_eq!(
            super::readback_window(10 * 1024 * 1024),
            Duration::from_secs(12)
        );
        assert_eq!(
            super::readback_window(50 * 1024 * 1024),
            Duration::from_secs(30)
        );
    }
}
