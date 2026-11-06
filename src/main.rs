use clap::{Parser, Subcommand, ValueEnum};
use rustdm::daemon::launch::{daemon_exe_path, spawn_detached};
use rustdm::daemon::service::{daemon_holds_lock, DEFAULT_SETTLE_TIMEOUT};
use rustdm::engine::{build_client, download, resume, DownloadRequest, Outcome, Progress};
use rustdm::ipc::client::{connect_or_spawn, ClientError, DaemonClient};
use rustdm::ipc::protocol::{
    AddDownloadParams, DaemonLimits, DownloadSummary, QueuePosition, SetLimitsParams,
};
use rustdm::ipc::transport::IpcEndpoint;
use rustdm::tui::format::{format_row, format_speed_cap, status_label};
use rustdm::tui::{default_downloads_dir, run_interactive, run_plain, PLAIN_INTERVAL};
use std::error::Error;
use std::io::{IsTerminal, Write};
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

type CliResult = Result<(), Box<dyn Error + Send + Sync>>;

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
    /// With no command, rdm opens the dashboard
    #[command(subcommand)]
    command: Option<Commands>,
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
        /// Speed cap for this download alone, such as 500K or 2M
        #[arg(long, value_parser = parse_speed)]
        max_speed: Option<u64>,
    },
    /// List the background daemon's downloads in queue order
    List,
    /// Pause a background download (a queued one stays out of the queue)
    Pause { id: String },
    /// Stop a background download for good
    Cancel {
        id: String,
        /// Also delete the partial .part and .part.rdm files
        #[arg(long)]
        delete: bool,
    },
    /// Remove a finished, paused, or cancelled download from the list (files are kept)
    Remove { id: String },
    /// Move a download in the queue; queued downloads start in list order
    Move {
        id: String,
        #[arg(value_enum)]
        position: MoveTo,
    },
    /// Cap one download's speed, such as 500K or 2M; 0 or "off" removes the cap
    Speed {
        id: String,
        #[arg(value_parser = parse_speed)]
        max_speed: u64,
    },
    /// Show or change how many downloads run at once and the total speed cap
    Limits {
        /// Downloads that run at the same time; the rest wait in the queue
        #[arg(long)]
        max_active: Option<usize>,
        /// Combined speed cap such as 500K, 2M or 1.5MiB (K = 1024 bytes); 0 or "off" removes it
        #[arg(long, value_parser = parse_speed)]
        max_speed: Option<u64>,
    },
    /// Open the interactive dashboard (what plain `rdm` does)
    Tui {
        /// Print changing rows as plain text instead (automatic when output is not a terminal)
        #[arg(long)]
        plain: bool,
    },
    /// Start, stop, or check the background daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum MoveTo {
    Top,
    Up,
    Down,
    Bottom,
}

impl From<MoveTo> for QueuePosition {
    fn from(to: MoveTo) -> Self {
        match to {
            MoveTo::Top => QueuePosition::Top,
            MoveTo::Up => QueuePosition::Up,
            MoveTo::Down => QueuePosition::Down,
            MoveTo::Bottom => QueuePosition::Bottom,
        }
    }
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

    match cli.command.unwrap_or(Commands::Tui { plain: false }) {
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
            max_speed,
        } => {
            let params = AddDownloadParams {
                url,
                destination: dir,
                filename: name,
                connections: Some(connections),
                max_bytes_per_sec: max_speed,
            };
            add(&endpoint, params).await
        }
        Commands::List => list(&endpoint).await,
        Commands::Pause { id } => {
            let summary = DaemonClient::connect(&endpoint).await?.pause(&id).await?;
            print_status(&summary);
            Ok(())
        }
        Commands::Cancel { id, delete } => {
            let summary = DaemonClient::connect(&endpoint)
                .await?
                .cancel(&id, delete)
                .await?;
            print_status(&summary);
            Ok(())
        }
        Commands::Remove { id } => {
            let summary = DaemonClient::connect(&endpoint).await?.remove(&id).await?;
            println!("Removed {} ({})", summary.id, summary.filename);
            Ok(())
        }
        Commands::Move { id, position } => {
            DaemonClient::connect(&endpoint)
                .await?
                .move_download(&id, position.into())
                .await?;
            list(&endpoint).await
        }
        Commands::Speed { id, max_speed } => {
            let summary = DaemonClient::connect(&endpoint)
                .await?
                .set_download_speed(&id, max_speed)
                .await?;
            println!("{}: speed cap {}", summary.id, format_speed_cap(max_speed));
            Ok(())
        }
        Commands::Limits {
            max_active,
            max_speed,
        } => limits(&endpoint, max_active, max_speed).await,
        Commands::Tui { plain } => tui(&endpoint, plain).await,
        Commands::Daemon { action } => match action {
            DaemonAction::Start => daemon_start(&endpoint).await,
            DaemonAction::Status => daemon_status(&endpoint).await,
            DaemonAction::Stop => daemon_stop(&endpoint).await,
        },
    }
}

async fn tui(endpoint: &IpcEndpoint, plain: bool) -> CliResult {
    if plain || !std::io::stdout().is_terminal() {
        let mut client = match DaemonClient::connect(endpoint).await {
            Err(ClientError::NotRunning) => {
                println!("rdmd is not running, so there are no background downloads.");
                return Ok(());
            }
            other => other?,
        };
        return run_plain(&mut client, &mut std::io::stdout(), PLAIN_INTERVAL).await;
    }
    // Start the daemon before taking over the screen, so startup errors print normally.
    start_daemon(endpoint).await?;
    run_interactive(endpoint, &default_downloads_dir()).await
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

async fn add(endpoint: &IpcEndpoint, params: AddDownloadParams) -> CliResult {
    url::Url::parse(&params.url)?;
    // The daemon runs in another directory, so send it an absolute path.
    let dir = std::path::absolute(
        params
            .destination
            .clone()
            .unwrap_or_else(default_downloads_dir),
    )?;

    let summary = start_daemon(endpoint)
        .await?
        .add(AddDownloadParams {
            destination: Some(dir.clone()),
            ..params
        })
        .await?;
    println!(
        "Added {} ({}, saving to {})",
        summary.id,
        status_label(summary.status),
        dir.display()
    );
    Ok(())
}

async fn limits(
    endpoint: &IpcEndpoint,
    max_active: Option<usize>,
    max_speed: Option<u64>,
) -> CliResult {
    let mut client = DaemonClient::connect(endpoint).await?;
    let limits = if max_active.is_none() && max_speed.is_none() {
        client.get_limits().await?
    } else {
        client
            .set_limits(SetLimitsParams {
                max_active,
                max_bytes_per_sec: max_speed,
            })
            .await?
    };
    println!("{}", format_limits(&limits));
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

    let downloads = client.list().await?;
    if downloads.is_empty() {
        println!("No downloads.");
        return Ok(());
    }

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

fn format_limits(limits: &DaemonLimits) -> String {
    format!(
        "Downloads at once: {}\nSpeed cap: {}",
        limits.max_active,
        format_speed_cap(limits.max_bytes_per_sec)
    )
}

/// Parse a speed such as `500K`, `2M`, `1.5MiB/s` or `0`/`off` (unlimited).
/// Units are binary: K = 1024 bytes.
fn parse_speed(raw: &str) -> Result<u64, String> {
    let lower = raw.trim().to_ascii_lowercase();
    if matches!(lower.as_str(), "0" | "off" | "unlimited") {
        return Ok(0);
    }

    let text = lower.strip_suffix("/s").unwrap_or(&lower);
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: f64 = number
        .parse()
        .map_err(|_| format!("invalid speed '{raw}'; try 500K or 2M"))?;
    let multiplier: u64 = match unit.trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        other => return Err(format!("unknown speed unit '{other}'; use K, M or G")),
    };
    Ok((value * multiplier as f64) as u64)
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
    fn uptime_drops_empty_leading_units() {
        assert_eq!(format_uptime(9), "9s");
        assert_eq!(format_uptime(125), "2m 05s");
        assert_eq!(format_uptime(3723), "1h 02m 03s");
    }

    #[test]
    fn speeds_accept_binary_units_and_off() {
        assert_eq!(parse_speed("500K"), Ok(500 * 1024));
        assert_eq!(parse_speed("2M"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_speed("1.5MiB/s"), Ok(1536 * 1024));
        assert_eq!(parse_speed("4096"), Ok(4096));
        assert_eq!(parse_speed("off"), Ok(0));
        assert!(parse_speed("fast").is_err());
        assert!(parse_speed("10X").is_err());
    }

    #[test]
    fn limits_show_unlimited_speed_in_words() {
        let limits = DaemonLimits {
            max_active: 3,
            max_bytes_per_sec: 0,
        };
        assert_eq!(
            format_limits(&limits),
            "Downloads at once: 3\nSpeed cap: unlimited"
        );
    }
}
