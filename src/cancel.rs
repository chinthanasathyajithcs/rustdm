use crate::error::Error;
use std::future::Future;
use std::time::Duration;
use tokio::sync::watch;

/// Run `fut`, but stop with `Error::Cancelled` as soon as `cancel` turns true,
/// so a pause is not held up by a silent connection or a long retry wait.
pub async fn or_cancel<F: Future>(
    fut: F,
    cancel: Option<&watch::Receiver<bool>>,
) -> Result<F::Output, Error> {
    let Some(cancel) = cancel else {
        return Ok(fut.await);
    };
    let mut cancel = cancel.clone();
    if *cancel.borrow_and_update() {
        return Err(Error::Cancelled);
    }
    tokio::select! {
        out = fut => Ok(out),
        _ = cancelled(&mut cancel) => Err(Error::Cancelled),
    }
}

/// Sleep for `delay` unless `cancel` fires first.
pub async fn sleep_or_cancel(
    delay: Duration,
    cancel: Option<&watch::Receiver<bool>>,
) -> Result<(), Error> {
    or_cancel(tokio::time::sleep(delay), cancel).await
}

/// Resolves once the flag is true; never resolves if the sender is gone.
async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        if *cancel.borrow() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn without_a_cancel_flag_the_future_just_runs() {
        assert_eq!(or_cancel(async { 7 }, None).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn an_already_set_flag_cancels_at_once() {
        let (_tx, rx) = watch::channel(true);
        let result = or_cancel(std::future::pending::<()>(), Some(&rx)).await;
        assert!(matches!(result, Err(Error::Cancelled)));
    }

    #[tokio::test]
    async fn setting_the_flag_interrupts_a_long_wait() {
        let (tx, rx) = watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tx.send(true).unwrap();
        });
        let start = std::time::Instant::now();
        let result = sleep_or_cancel(Duration::from_secs(60), Some(&rx)).await;
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_dropped_sender_never_cancels() {
        let (tx, rx) = watch::channel(false);
        drop(tx);
        let result = or_cancel(async { "done" }, Some(&rx)).await;
        assert_eq!(result.unwrap(), "done");
    }
}
