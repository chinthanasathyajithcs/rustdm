use clap::{Parser, Subcommand};
use rustdm::daemon::launch::{daemon_exe_path, spawn_detached};
use rustdm::daemon::service::{daemon_holds_lock, DEFAULT_SETTLE_TIMEOUT};
use rustdm::engine::{build_client, download, resume, DownloadRequest, Outcome, Progress};
use rustdm::ipc::client::{connect_or_spawn, ClientError, DaemonClient};
use rustdm::ipc::protocol::{AddDownloadParams, DownloadStatus, DownloadSummary};
use rustdm::ipc::transport::IpcEndpoint;
use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long `rdm` waits for a daemon it started to begin listening.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);
/// How often `rdm daemon stop` checks whether the daemon has exited.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Time `rdm daemon stop` allows on top of the daemon's own settle timeout.
const STOP_GRACE: Duration = Duration::from_secs(5);

type CliResult = Result<(), Box<dyn Error>>;

#[derive(Parser)]
#[command(
    name = "rdm",
    version = rustdm::VERSION,
    about = "Windows 11 resilient download manager"
)]
struct Cli {
    /// Daemon endpoint: a socket path on Unix, a pipe name on Windows
    #[arg(long, global = true)]
    endpoint: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Download a file in the foreground
    Download {
        url: String,
        #[arg(long, default_value = "{downloads}/{name}")]
        out: String,
        #[arg(long, default_value_t = 8)]
        connections: u32,
    },
    /// Resume a paused download: a daemon id such as dl_3, or a .part.rdm state file
    Resume {
        target: String,
        /// Replacement URL, when resuming from a state file
        #[arg(long)]
        url: Option<String>,
    },
    /// Add a download to the background daemon, starting the daemon if needed
    Add {
        url: String,
        /// Folder to save into (default: your Downloads folder)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// File name to save as (default: taken from the server)
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value_t = 8)]
        connections: usize,
    },
    /// List the background daemon's downloads
    List,
    /// Pause a background download
    Pause { id: String },
    /// Stop a background download for good
    Cancel { id: String },
    /// Start, stop, or check the background daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon if it is not already running
    Start,
    /// Show whether the daemon is running
    Status,
    /// Pause all downloads and stop the daemon
    Stop,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> CliResult {
    let endpoint = cli
        .endpoint
        .as_deref()
        .map(IpcEndpoint::from_arg)
        .unwrap_or_else(IpcEndpoint::default_endpoint);

    match cli.command {
        Commands::Download {
            url,
            out,
            connections,
        } => download_foreground(&url, out, connections).await,
        Commands::Resume { target, url } => {
            if is_daemon_id(&target) && !Path::new(&target).exists() {
                if url.is_some() {
                    return Err("--url only applies when resuming from a state file".into());
                }
                let summary = DaemonClient::connect(&endpoint)
                    .await?
                    .resume(&target)
                    .await?;
                print_status(&summary);
                Ok(())
            } else {
                resume_foreground(Path::new(&target), url).await
            }
        }
        Commands::Add {
            url,
            dir,
            name,
            connections,
        } => add(&endpoint, url, dir, name, connections).await,
        Commands::List => list(&endpoint).await,
        Commands::Pause { id } => {
            let summary = DaemonClient::connect(&endpoint).await?.pause(&id).await?;
            print_status(&summary);
            Ok(())
        }
        Commands::Cancel { id } => {
            let summary = DaemonClient::connect(&endpoint).await?.cancel(&id).await?;
            print_status(&summary);
            Ok(())
        }
        Commands::Daemon { action } => match action {
            DaemonAction::Start => daemon_start(&endpoint).await,
            DaemonAction::Status => daemon_status(&endpoint).await,
            DaemonAction::Stop => daemon_stop(&endpoint).await,
        },
    }
}

fn default_downloads_dir() -> PathBuf {
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        PathBuf::from(userprofile).join("Downloads")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("Downloads")
    } else {
        PathBuf::from("Downloads")
    }
}

async fn download_foreground(url: &str, out: String, connections: u32) -> CliResult {
    let parsed_url = url::Url::parse(url)?;
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = cancel_tx.send(true);
    });

    let progress = Arc::new(Progress::new());
    let progress_printer = spawn_progress_printer(Arc::clone(&progress));

    let req = DownloadRequest {
        url: parsed_url,
        template: out,
        downloads_dir: default_downloads_dir(),
        connections,
        progress,
        cancel: Some(cancel_rx),
    };

    let result = download(&build_client(), req).await;
    progress_printer.abort();
    eprintln!();
    report_outcome("Download", result?);
    Ok(())
}

async fn resume_foreground(state: &Path, url: Option<String>) -> CliResult {
    let url_override = url.map(|u| url::Url::parse(&u)).transpose()?;
    report_outcome(
        "Resume",
        resume(&build_client(), state, url_override).await?,
    );
    Ok(())
}

fn spawn_progress_printer(progress: Arc<Progress>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_bytes = 0u64;
        let mut last_time = Instant::now();
        let mut interval = tokio::time::interval(Duration::from_millis(500));

        loop {
            interval.tick().await;
            let downloaded = progress.downloaded.load(Ordering::SeqCst);
            let elapsed = last_time.elapsed().as_secs_f64();
            let bytes_diff = downloaded.saturating_sub(last_bytes);
            let speed_mb = if elapsed > 0.0 {
                (bytes_diff as f64 / (1024.0 * 1024.0)) / elapsed
            } else {
                0.0
            };

            let mb_done = downloaded as f64 / (1024.0 * 1024.0);
            eprint!(
                "\r[rdm] Downloaded: {:.2} MB | Speed: {:.2} MB/s",
                mb_done, speed_mb
            );
            let _ = std::io::stderr().flush();

            last_bytes = downloaded;
            last_time = Instant::now();
        }
    })
}

fn report_outcome(verb: &str, outcome: Outcome) {
    match outcome {
        Outcome::Completed { path, steals } => {
            eprintln!("{verb} complete: {} (steals: {steals})", path.display());
        }
        Outcome::Paused { reason, state } => {
            eprintln!(
                "Download paused ({reason:?}): state saved to {}",
                state.display()
            );
        }
    }
}

async fn start_daemon(endpoint: &IpcEndpoint) -> Result<DaemonClient, ClientError> {
    connect_or_spawn(
        endpoint,
        || spawn_detached(&daemon_exe_path()?, endpoint),
        SPAWN_TIMEOUT,
    )
    .await
}

async fn add(
    endpoint: &IpcEndpoint,
    url: String,
    dir: Option<PathBuf>,
    name: Option<String>,
    connections: usize,
) -> CliResult {
    url::Url::parse(&url)?;
    // The daemon runs in another directory, so send it an absolute path.
    let dir = std::path::absolute(dir.unwrap_or_else(default_downloads_dir))?;

    let summary = start_daemon(endpoint)
        .await?
        .add(AddDownloadParams {
            url,
            destination: Some(dir.clone()),
            filename: name,
            connections: Some(connections),
        })
        .await?;
    println!("Added {} (saving to {})", summary.id, dir.display());
    Ok(())
}

async fn list(endpoint: &IpcEndpoint) -> CliResult {
    let mut client = match DaemonClient::connect(endpoint).await {
        Err(ClientError::NotRunning) => {
            println!("rdmd is not running, so there are no background downloads.");
            return Ok(());
        }
        other => other?,
    };

    let mut downloads = client.list().await?;
    if downloads.is_empty() {
        println!("No downloads.");
        return Ok(());
    }

    downloads.sort_by_key(|d| id_number(&d.id));
    println!(
        "{:<8} {:<10} {:<28} {:>12}  NAME",
        "ID", "STATUS", "PROGRESS", "SPEED"
    );
    for summary in &downloads {
        println!("{}", format_row(summary));
    }
    Ok(())
}

async fn daemon_start(endpoint: &IpcEndpoint) -> CliResult {
    let ping = start_daemon(endpoint).await?.ping().await?;
    println!("rdmd {} is running on {}", ping.version, endpoint);
    Ok(())
}

async fn daemon_status(endpoint: &IpcEndpoint) -> CliResult {
    let mut client = match DaemonClient::connect(endpoint).await {
        Err(ClientError::NotRunning) => {
            println!("rdmd is not running ({endpoint})");
            return Ok(());
        }
        other => other?,
    };

    let ping = client.ping().await?;
    let count = client.list().await?.len();
    println!(
        "rdmd {} is running on {} (up {}, {} downloads)",
        ping.version,
        endpoint,
        format_uptime(ping.uptime_secs),
        count
    );
    Ok(())
}

async fn daemon_stop(endpoint: &IpcEndpoint) -> CliResult {
    match DaemonClient::connect(endpoint).await {
        Err(ClientError::NotRunning) => {
            println!("rdmd is not running");
            return Ok(());
        }
        other => other?.shutdown().await?,
    }

    // The daemon holds its lock until downloads have saved state and it exits.
    let timeout = DEFAULT_SETTLE_TIMEOUT + STOP_GRACE;
    let deadline = Instant::now() + timeout;
    while daemon_holds_lock(endpoint) {
        if Instant::now() >= deadline {
            return Err(format!("rdmd did not exit within {timeout:?}").into());
        }
        tokio::time::sleep(STOP_POLL_INTERVAL).await;
    }
    println!("rdmd stopped");
    Ok(())
}

fn print_status(summary: &DownloadSummary) {
    println!("{}: {}", summary.id, status_label(summary.status));
}

fn is_daemon_id(target: &str) -> bool {
    target
        .strip_prefix("dl_")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn id_number(id: &str) -> u64 {
    id.strip_prefix("dl_")
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX)
}

fn status_label(status: DownloadStatus) -> &'static str {
    match status {
        DownloadStatus::Queued => "queued",
        DownloadStatus::Active => "active",
        DownloadStatus::Pausing => "pausing",
        DownloadStatus::Paused => "paused",
        DownloadStatus::Retrying => "retrying",
        DownloadStatus::Completed => "completed",
        DownloadStatus::Failed => "failed",
        DownloadStatus::Cancelled => "cancelled",
    }
}

fn format_row(d: &DownloadSummary) -> String {
    let progress = match d.total_bytes {
        Some(total) if total > 0 => format!(
            "{} / {} ({:.0}%)",
            format_bytes(d.downloaded_bytes),
            format_bytes(total),
            d.downloaded_bytes as f64 * 100.0 / total as f64
        ),
        _ => format_bytes(d.downloaded_bytes),
    };
    let speed = if d.speed_bps > 0 {
        format!("{}/s", format_bytes(d.speed_bps))
    } else {
        "-".to_string()
    };

    let mut row = format!(
        "{:<8} {:<10} {:<28} {:>12}  {}",
        d.id,
        status_label(d.status),
        progress,
        speed,
        d.filename
    );
    if let Some(err) = &d.error_message {
        row.push_str(&format!("  ({err})"));
    }
    row
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_uptime(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_ids_are_dl_followed_by_digits() {
        assert!(is_daemon_id("dl_1"));
        assert!(is_daemon_id("dl_42"));
        assert!(!is_daemon_id("dl_"));
        assert!(!is_daemon_id("dl_4x"));
        assert!(!is_daemon_id("demo.bin.part.rdm"));
    }

    #[test]
    fn bytes_use_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(64 * 1024 * 1024), "64.0 MiB");
    }

    #[test]
    fn uptime_drops_empty_leading_units() {
        assert_eq!(format_uptime(9), "9s");
        assert_eq!(format_uptime(125), "2m 05s");
        assert_eq!(format_uptime(3723), "1h 02m 03s");
    }

    #[test]
    fn list_rows_show_percent_once_the_size_is_known() {
        let summary = DownloadSummary {
            id: "dl_3".to_string(),
            url: "http://example.com/a.iso".to_string(),
            destination: PathBuf::from("/downloads"),
            filename: "a.iso".to_string(),
            status: DownloadStatus::Active,
            downloaded_bytes: 25 * 1024 * 1024,
            total_bytes: Some(100 * 1024 * 1024),
            speed_bps: 2 * 1024 * 1024,
            eta_secs: Some(37),
            error_message: None,
        };
        let row = format_row(&summary);
        assert!(row.contains("25.0 MiB / 100.0 MiB (25%)"), "{row}");
        assert!(row.contains("2.0 MiB/s"), "{row}");
        assert!(row.ends_with("a.iso"), "{row}");
    }

    #[test]
    fn ids_sort_numerically() {
        let mut ids = vec!["dl_10", "dl_2", "dl_1"];
        ids.sort_by_key(|id| id_number(id));
        assert_eq!(ids, vec!["dl_1", "dl_2", "dl_10"]);
    }
}
