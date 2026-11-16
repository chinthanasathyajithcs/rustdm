use clap::{Parser, Subcommand, ValueEnum};
use rustdm::daemon::launch::{daemon_exe_path, spawn_detached};
use rustdm::daemon::service::{daemon_holds_lock, DEFAULT_SETTLE_TIMEOUT};
use rustdm::daemon::speed::SpeedMeter;
use rustdm::dirs::default_downloads_dir;
use rustdm::engine::{
    build_client, download, resume_with_options, DownloadRequest, Outcome, PauseReason, Progress,
};
use rustdm::ipc::client::{connect_or_spawn, ClientError, DaemonClient};
use rustdm::ipc::protocol::{
    AddDownloadParams, DaemonLimits, DownloadDefaults, DownloadSummary, QueuePosition,
    SetDefaultsParams, SetLimitsParams,
};
use rustdm::ipc::transport::IpcEndpoint;
use rustdm::tui::format::{
    format_bytes, format_eta, format_row, format_speed, format_speed_cap, status_label,
};
use rustdm::tui::{run_interactive, run_plain, PLAIN_INTERVAL};
use rustdm::units::parse_speed;
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
        /// Folder to save into (default: the folder `rdm defaults` shows)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// File name to save as (default: taken from the server)
        #[arg(long)]
        name: Option<String>,
        /// Connections for this download (default: the number `rdm defaults` shows)
        #[arg(long)]
        connections: Option<usize>,
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
    /// Show or change the folder, connections and speed cap new downloads start with
    Defaults {
        /// Folder new downloads save into
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Connections each new download opens, 1 to 16
        #[arg(long)]
        connections: Option<u32>,
        /// Speed cap for each new download such as 500K or 2M; 0 or "off" removes it
        #[arg(long, value_parser = parse_speed)]
        speed: Option<u64>,
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
                connections,
                max_bytes_per_sec: max_speed,
                context: None,
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
        Commands::Defaults {
            dir,
            connections,
            speed,
        } => defaults(&endpoint, dir, connections, speed).await,
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
    run_interactive(endpoint).await
}

async fn download_foreground(url: &str, out: String, connections: u32) -> CliResult {
    let parsed_url = url::Url::parse(url)?;
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest {
        url: parsed_url,
        template: out,
        downloads_dir: default_downloads_dir(),
        connections,
        progress: Arc::clone(&progress),
        cancel: Some(pause_on_ctrl_c()),
        file_name: None,
    };

    let printer = spawn_progress_printer(progress);
    let result = download(&build_client(), req).await;
    printer.abort();
    eprintln!();
    report_outcome("Download", result?);
    Ok(())
}

async fn resume_foreground(state: &Path, url: Option<String>) -> CliResult {
    let url_override = url.map(|u| url::Url::parse(&u)).transpose()?;
    let progress = Arc::new(Progress::new());
    let printer = spawn_progress_printer(Arc::clone(&progress));
    let result = resume_with_options(
        &build_client(),
        state,
        url_override,
        Some(progress),
        Some(pause_on_ctrl_c()),
    )
    .await;
    printer.abort();
    eprintln!();
    report_outcome("Resume", result?);
    Ok(())
}

/// A cancel flag that Ctrl+C sets, so a foreground download pauses and saves
/// its state instead of being killed.
fn pause_on_ctrl_c() -> tokio::sync::watch::Receiver<bool> {
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = cancel_tx.send(true);
    });
    cancel_rx
}

fn spawn_progress_printer(progress: Arc<Progress>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut meter = SpeedMeter::default();
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        // Pad with spaces rather than an erase escape, which older Windows
        // consoles print as text.
        let mut last_len = 0;
        loop {
            interval.tick().await;
            let downloaded = progress.downloaded.load(Ordering::SeqCst);
            let speed = meter.record(Instant::now(), downloaded);
            let total = Some(progress.total.load(Ordering::SeqCst)).filter(|&t| t > 0);
            let line = progress_line(downloaded, total, speed);
            eprint!("\r{line:<width$}", width = last_len);
            last_len = line.len();
            let _ = std::io::stderr().flush();
        }
    })
}

/// "[rdm] 25.0 MiB / 100.0 MiB (25%)  2.0 MiB/s  ETA 37s", shortened when the
/// size is unknown.
fn progress_line(downloaded: u64, total: Option<u64>, speed: u64) -> String {
    let speed_text = format_speed(speed);
    match total {
        Some(total) => {
            let eta = (speed > 0).then(|| total.saturating_sub(downloaded) / speed);
            format!(
                "[rdm] {} / {} ({:.0}%)  {}  ETA {}",
                format_bytes(downloaded),
                format_bytes(total),
                downloaded as f64 * 100.0 / total as f64,
                speed_text,
                format_eta(eta)
            )
        }
        None => format!("[rdm] {}  {}", format_bytes(downloaded), speed_text),
    }
}

fn report_outcome(verb: &str, outcome: Outcome) {
    match outcome {
        Outcome::Completed { path, steals } => {
            eprintln!("{verb} complete: {} (steals: {steals})", path.display());
        }
        Outcome::Paused {
            reason: PauseReason::LinkExpired { status },
            state,
        } => {
            eprintln!(
                "Download paused ({}): state saved to {}",
                rustdm::error::link_status_message(status),
                state.display()
            );
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
    // The daemon runs in another directory, so send it an absolute path. With
    // no --dir the daemon uses its default folder.
    let destination = params
        .destination
        .clone()
        .map(std::path::absolute)
        .transpose()?;

    let summary = start_daemon(endpoint)
        .await?
        .add(AddDownloadParams {
            destination,
            ..params
        })
        .await?;
    println!(
        "Added {} ({}, saving to {})",
        summary.id,
        status_label(summary.status),
        summary.destination.display()
    );
    Ok(())
}

async fn defaults(
    endpoint: &IpcEndpoint,
    dir: Option<PathBuf>,
    connections: Option<u32>,
    speed: Option<u64>,
) -> CliResult {
    let mut client = DaemonClient::connect(endpoint).await?;
    let defaults = if dir.is_none() && connections.is_none() && speed.is_none() {
        client.get_defaults().await?
    } else {
        client
            .set_defaults(SetDefaultsParams {
                folder: dir.map(std::path::absolute).transpose()?,
                connections,
                max_bytes_per_sec: speed,
            })
            .await?
    };
    println!("{}", format_defaults(&defaults));
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

fn format_defaults(defaults: &DownloadDefaults) -> String {
    format!(
        "Folder: {}\nConnections: {}\nSpeed cap per download: {}",
        defaults.folder.display(),
        defaults.connections,
        format_speed_cap(defaults.max_bytes_per_sec)
    )
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

    #[test]
    fn the_progress_line_uses_binary_units_and_shows_percent_and_eta() {
        const MIB: u64 = 1024 * 1024;
        assert_eq!(
            progress_line(25 * MIB, Some(100 * MIB), 2 * MIB),
            "[rdm] 25.0 MiB / 100.0 MiB (25%)  2.0 MiB/s  ETA 37s"
        );
        assert_eq!(
            progress_line(25 * MIB, Some(100 * MIB), 0),
            "[rdm] 25.0 MiB / 100.0 MiB (25%)  -  ETA -"
        );
        assert_eq!(
            progress_line(3 * MIB, None, MIB),
            "[rdm] 3.0 MiB  1.0 MiB/s"
        );
    }

    #[test]
    fn defaults_print_folder_connections_and_speed_in_words() {
        assert_eq!(
            format_defaults(&DownloadDefaults {
                folder: "/dl".into(),
                connections: 8,
                max_bytes_per_sec: 0
            }),
            "Folder: /dl\nConnections: 8\nSpeed cap per download: unlimited"
        );
    }
}
