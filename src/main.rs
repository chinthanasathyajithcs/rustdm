use clap::{Parser, Subcommand};
use rustdm::engine::{build_client, download, resume, DownloadRequest, Outcome, Progress};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "rdm",
    version = rustdm::VERSION,
    about = "Windows 11 resilient download manager"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Download {
        url: String,
        #[arg(long, default_value = "{downloads}/{name}")]
        out: String,
        #[arg(long, default_value_t = 8)]
        connections: u32,
    },
    Resume {
        state: PathBuf,
        #[arg(long)]
        url: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let client = build_client();

    let default_downloads_dir = if let Ok(userprofile) = std::env::var("USERPROFILE") {
        PathBuf::from(userprofile).join("Downloads")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("Downloads")
    } else {
        PathBuf::from("Downloads")
    };

    match cli.command {
        Commands::Download {
            url,
            out,
            connections,
        } => {
            let parsed_url = url::Url::parse(&url)?;
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                let _ = cancel_tx.send(true);
            });

            let progress = Arc::new(Progress::new());
            let req = DownloadRequest {
                url: parsed_url,
                template: out,
                downloads_dir: default_downloads_dir,
                connections,
                progress,
                cancel: Some(cancel_rx),
            };

            match download(&client, req).await? {
                Outcome::Completed { path, steals } => {
                    eprintln!("Download complete: {} (steals: {steals})", path.display());
                }
                Outcome::Paused { reason, state } => {
                    eprintln!(
                        "Download paused ({reason:?}): state saved to {}",
                        state.display()
                    );
                }
            }
        }
        Commands::Resume { state, url } => {
            let url_override = url.map(|u| url::Url::parse(&u)).transpose()?;
            match resume(&client, &state, url_override).await? {
                Outcome::Completed { path, steals } => {
                    eprintln!("Resume complete: {} (steals: {steals})", path.display());
                }
                Outcome::Paused { reason, state } => {
                    eprintln!(
                        "Download paused ({reason:?}): state saved to {}",
                        state.display()
                    );
                }
            }
        }
    }

    Ok(())
}
