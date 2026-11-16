//! Native messaging host for the rustdm browser extension. The browser starts
//! it and talks to it over stdin and stdout; stdout carries frames only.

use rustdm::ipc::transport::IpcEndpoint;
use rustdm::nmhost::origin::check_caller;
use rustdm::nmhost::relay::run_host;
use std::process::ExitCode;
use std::time::Duration;

/// Overrides the daemon endpoint. Read only by builds with the `testing`
/// feature; a release build always uses the per-user default.
#[cfg(feature = "testing")]
const ENDPOINT_ENV: &str = "RUSTDM_ENDPOINT";
/// How long to wait for a blocked stdin read before exiting anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(200);

#[cfg(feature = "testing")]
fn daemon_endpoint() -> Result<IpcEndpoint, String> {
    match std::env::var(ENDPOINT_ENV) {
        Ok(raw) if !raw.is_empty() => rustdm::nmhost::relay::endpoint_from_override(&raw),
        _ => Ok(IpcEndpoint::default_endpoint()),
    }
}

#[cfg(not(feature = "testing"))]
fn daemon_endpoint() -> Result<IpcEndpoint, String> {
    Ok(IpcEndpoint::default_endpoint())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = check_caller(&args) {
        eprintln!("rdm-nmhost: {e}");
        return ExitCode::from(2);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    let endpoint = match daemon_endpoint() {
        Ok(endpoint) => endpoint,
        Err(e) => {
            eprintln!("rdm-nmhost: {e}");
            return ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("rdm-nmhost: cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run_host(
        &mut tokio::io::stdin(),
        &mut tokio::io::stdout(),
        &endpoint,
    ));
    // A stdin read can still be pending in a helper thread; do not wait for it.
    runtime.shutdown_timeout(SHUTDOWN_GRACE);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rdm-nmhost: {e}");
            ExitCode::FAILURE
        }
    }
}
