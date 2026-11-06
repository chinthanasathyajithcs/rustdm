use crate::daemon::queue_store::QueueStore;
use crate::daemon::{DaemonCore, DaemonServer};
use crate::ipc::transport::{IpcEndpoint, IpcTransportError};
use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long shutdown waits for downloads to pause and save their state.
pub const DEFAULT_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("rdmd is already running (lock held on {})", .0.display())]
    AlreadyRunning(PathBuf),
    #[error("cannot use lock file {}: {source}", .path.display())]
    Lock { path: PathBuf, source: io::Error },
    #[error("cannot read download queue {}: {source}", .path.display())]
    Queue { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Transport(#[from] IpcTransportError),
}

/// Exclusive OS file lock that keeps a second daemon off the same endpoint.
/// The OS releases it when the process exits, including after a crash.
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    pub fn acquire(path: &Path) -> Result<Self, ServiceError> {
        let lock_err = |source| ServiceError::Lock {
            path: path.to_path_buf(),
            source,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(lock_err)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(lock_err)?;

        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(ServiceError::AlreadyRunning(path.to_path_buf())),
            Err(TryLockError::Error(e)) => Err(lock_err(e)),
        }
    }
}

/// True while a daemon process holds the lock for `endpoint`.
pub fn daemon_holds_lock(endpoint: &IpcEndpoint) -> bool {
    matches!(
        InstanceLock::acquire(&endpoint.lock_path()),
        Err(ServiceError::AlreadyRunning(_))
    )
}

/// Run the daemon on `endpoint` until a `daemon.shutdown` request or `shutdown_signal`
/// resolves. Active downloads are paused and get up to `settle_timeout` to save state.
/// With a `data_dir`, the queue is restored from and saved to `<data_dir>/queue.json`.
pub async fn run_daemon<S>(
    endpoint: &IpcEndpoint,
    data_dir: Option<&Path>,
    shutdown_signal: S,
    settle_timeout: Duration,
) -> Result<(), ServiceError>
where
    S: Future<Output = ()> + Send + 'static,
{
    let _lock = InstanceLock::acquire(&endpoint.lock_path())?;
    let core = match data_dir {
        Some(dir) => {
            let store = QueueStore::in_dir(dir);
            let path = store.path().to_path_buf();
            DaemonCore::with_store(store).map_err(|source| ServiceError::Queue { path, source })?
        }
        None => DaemonCore::new(),
    };
    let server = DaemonServer::bind_with_core(endpoint, core)
        .await
        .map_err(|e| match e {
            IpcTransportError::AddressInUse(_) => {
                ServiceError::AlreadyRunning(endpoint.lock_path())
            }
            other => other.into(),
        })?;
    tracing::info!("rdmd {} listening on {}", crate::VERSION, endpoint);

    let core = server.core.clone();
    let signal_core = core.clone();
    let signal_task = tokio::spawn(async move {
        shutdown_signal.await;
        tracing::info!("shutdown signal received");
        signal_core.shutdown();
    });

    let result = server.run().await;
    signal_task.abort();

    if !core.wait_until_settled(settle_timeout).await {
        tracing::warn!(
            "some downloads were still saving state after {:?}",
            settle_timeout
        );
    }
    tracing::info!("rdmd stopped");
    result.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_on_the_same_path_reports_already_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdmd.lock");

        let first = InstanceLock::acquire(&path).expect("first lock");
        assert!(matches!(
            InstanceLock::acquire(&path),
            Err(ServiceError::AlreadyRunning(_))
        ));

        drop(first);
        InstanceLock::acquire(&path).expect("lock is free again after release");
    }
}
