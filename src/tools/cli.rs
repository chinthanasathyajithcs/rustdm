//! `rdm tools`: show the helper tools, update them, or roll one back. The
//! work is in the daemon; this talks to it and prints what comes back.

use super::display::{state_text, table, tool_label};
use super::release::now_unix;
use crate::ipc::client::DaemonClient;
use crate::ipc::protocol::{ToolInfo, ToolName, ToolPhase};
use std::error::Error;
use std::io::Write;
use std::time::Duration;

/// How often `rdm tools update` asks how far the daemon has got.
pub const UPDATE_POLL_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolsCommand {
    /// Show every tool's version and state.
    Status,
    /// Check one tool (installing it if missing), or every installed tool.
    Update(Option<ToolName>),
    /// Go back to a tool's previous version.
    Rollback(ToolName),
}

type CliResult = Result<(), Box<dyn Error + Send + Sync>>;

pub async fn run(
    client: &mut DaemonClient,
    command: ToolsCommand,
    out: &mut dyn Write,
    poll: Duration,
) -> CliResult {
    match command {
        ToolsCommand::Status => {
            let status = client.tools_status().await?;
            writeln!(out, "{}", table(&status, now_unix()))?;
            Ok(())
        }
        ToolsCommand::Rollback(tool) => {
            let before = client.tools_status().await?;
            let was = before
                .iter()
                .find(|i| i.tool == tool)
                .and_then(|i| i.installed.clone());
            let now = client.tools_rollback(tool).await?;
            writeln!(
                out,
                "{}: now {} (was {})",
                tool_label(tool),
                now.installed.as_deref().unwrap_or("-"),
                was.as_deref().unwrap_or("-")
            )?;
            Ok(())
        }
        ToolsCommand::Update(tool) => update(client, tool, out, poll).await,
    }
}

/// Start the update, then wait for it, printing each new step. Fails when a
/// tool that was asked for ends with a problem.
async fn update(
    client: &mut DaemonClient,
    tool: Option<ToolName>,
    out: &mut dyn Write,
    poll: Duration,
) -> CliResult {
    let started = client.tools_update(tool).await?;
    let targets: Vec<ToolName> = match tool {
        Some(tool) => vec![tool],
        None => started
            .iter()
            .filter(|i| i.installed.is_some() || i.phase != ToolPhase::Idle)
            .map(|i| i.tool)
            .collect(),
    };
    if targets.is_empty() {
        writeln!(
            out,
            "No tools are installed yet; they are downloaded when first needed."
        )?;
        return Ok(());
    }

    let mut last_line = String::new();
    let status = loop {
        let status = client.tools_status().await?;
        let working: Vec<&ToolInfo> = status
            .iter()
            .filter(|i| targets.contains(&i.tool) && i.phase != ToolPhase::Idle)
            .collect();
        let Some(first) = working.first() else {
            break status;
        };
        let line = state_text(first);
        if line != last_line {
            writeln!(out, "{line}")?;
            last_line = line;
        }
        tokio::time::sleep(poll).await;
    };

    writeln!(out, "{}", table(&status, now_unix()))?;
    let failed: Vec<&str> = status
        .iter()
        .filter(|i| targets.contains(&i.tool) && i.error.is_some())
        .map(|i| tool_label(i.tool))
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("could not update {}", failed.join(", ")).into())
    }
}
