use clap::Parser;
use rustdm::daemon::service::{run_daemon, DEFAULT_SETTLE_TIMEOUT};
use rustdm::ipc::transport::IpcEndpoint;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "rdmd",
    version = rustdm::VERSION,
    about = "rustdm background download daemon"
)]
struct Args {
    /// IPC endpoint: a socket path on Unix, a pipe name on Windows
    #[arg(long)]
    endpoint: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let endpoint = args
        .endpoint
        .as_deref()
        .map(IpcEndpoint::from_arg)
        .unwrap_or_else(IpcEndpoint::default_endpoint);

    match run_daemon(&endpoint, shutdown_signal(), DEFAULT_SETTLE_TIMEOUT).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rdmd: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Resolves on Ctrl+C. If the handler cannot be registered, logs it and never
/// resolves, so the daemon keeps running. The other signal sources below do the same.
async fn ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::warn!("cannot listen for Ctrl+C: {e}");
        std::future::pending::<()>().await;
    }
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    // Registering a SIGHUP handler replaces the default action, so closing the
    // terminal that started rdmd does not kill it.
    let _hangup = signal(SignalKind::hangup());

    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(e) => {
                tracing::warn!("cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        _ = ctrl_c() => {}
        _ = terminate => {}
    }
}

#[cfg(windows)]
async fn shutdown_signal() {
    use tokio::signal::windows;

    let ctrl_break = async {
        match windows::ctrl_break() {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(e) => {
                tracing::warn!("cannot listen for Ctrl+Break: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    let ctrl_close = async {
        match windows::ctrl_close() {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(e) => {
                tracing::warn!("cannot listen for console close: {e}");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        _ = ctrl_c() => {}
        _ = ctrl_break => {}
        _ = ctrl_close => {}
    }
}
