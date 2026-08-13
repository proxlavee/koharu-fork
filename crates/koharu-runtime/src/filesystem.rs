use std::{io, path::Path};

#[cfg(any(windows, test))]
use std::{future::Future, time::Duration};

#[cfg(any(windows, test))]
const WINDOWS_PERMISSION_RETRIES: u32 = 10;
#[cfg(any(windows, test))]
const WINDOWS_PERMISSION_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Renames an installation artifact, tolerating temporary Windows scanner locks.
pub(crate) async fn rename(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        retry_permission_denied(
            || std::fs::rename(from, to),
            tokio::time::sleep,
            |error, delay| {
                tracing::warn!(
                    source = %from.display(),
                    destination = %to.display(),
                    ?delay,
                    %error,
                    "retrying a runtime artifact move after a temporary Windows access denial"
                );
            },
        )
        .await
    }
    #[cfg(not(windows))]
    {
        tokio::fs::rename(from, to).await
    }
}

#[cfg(any(windows, test))]
async fn retry_permission_denied<Operation, Sleep, Pending, Notify>(
    mut operation: Operation,
    mut sleep: Sleep,
    mut notify: Notify,
) -> io::Result<()>
where
    Operation: FnMut() -> io::Result<()>,
    Sleep: FnMut(Duration) -> Pending,
    Pending: Future<Output = ()>,
    Notify: FnMut(&io::Error, Duration),
{
    let mut retries = 0;
    loop {
        match operation() {
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && retries < WINDOWS_PERMISSION_RETRIES =>
            {
                let delay = WINDOWS_PERMISSION_RETRY_DELAY.saturating_mul(1 << retries);
                retries += 1;
                notify(&error, delay);
                sleep(delay).await;
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[tokio::test]
    async fn retries_temporary_permission_denials() {
        let attempts = Cell::new(0);
        let slept = Cell::new(Duration::ZERO);

        retry_permission_denied(
            || {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                if attempt < 3 {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                } else {
                    Ok(())
                }
            },
            |delay| {
                slept.set(slept.get() + delay);
                std::future::ready(())
            },
            |_, _| {},
        )
        .await
        .unwrap();

        assert_eq!(attempts.get(), 3);
        assert_eq!(slept.get(), Duration::from_millis(30));
    }

    #[tokio::test]
    async fn does_not_retry_permanent_or_unrelated_errors() {
        let attempts = Cell::new(0);
        let error = retry_permission_denied(
            || {
                attempts.set(attempts.get() + 1);
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid runtime artifact",
                ))
            },
            |_| std::future::ready(()),
            |_, _| {},
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(attempts.get(), 1);
    }
}
