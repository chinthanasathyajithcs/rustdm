//! The downloads table, and the progress bar and size text it shares.

use crate::ipc::protocol::DownloadSummary;
use crate::tui::format::{format_bytes, format_eta, format_speed, fraction_done, status_label};
use crate::tui::state::App;
use crate::tui::theme;
use ratatui::layout::{Constraint, Rect};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

/// Width of the progress bar in the table's PROGRESS column.
pub const TABLE_BAR_WIDTH: usize = 10;

/// "██████░░░░ 60%" for a known size, or blanks while unknown.
pub fn progress_bar(fraction: Option<f64>, width: usize) -> String {
    match fraction {
        Some(f) => {
            let filled = ((f * width as f64).round() as usize).min(width);
            format!(
                "{}{} {:>3.0}%",
                "█".repeat(filled),
                "░".repeat(width - filled),
                f * 100.0
            )
        }
        None => format!("{} ?", "░".repeat(width)),
    }
}

pub fn size_text(d: &DownloadSummary) -> String {
    match d.total_bytes {
        Some(total) => format!(
            "{} / {}",
            format_bytes(d.downloaded_bytes),
            format_bytes(total)
        ),
        None => format_bytes(d.downloaded_bytes),
    }
}

pub fn render_table(frame: &mut Frame, app: &mut App, area: Rect) -> Vec<(Rect, String)> {
    let block = Block::new()
        .borders(Borders::ALL)
        .title(" Downloads ")
        .border_style(theme::DIM);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible = app.visible();
    if visible.is_empty() {
        let text = if app.downloads.is_empty() {
            "No downloads yet. Press a, click [ + Add ], or paste a URL."
        } else {
            "Nothing in this tab."
        };
        frame.render_widget(Paragraph::new(text).style(theme::MUTED), inner);
        return Vec::new();
    }

    // Scroll so the selected row stays on screen.
    let body_rows = inner.height.saturating_sub(1) as usize;
    let selected = app.selected_index().unwrap_or(0);
    let mut offset = app.table_offset.min(visible.len().saturating_sub(1));
    if selected < offset {
        offset = selected;
    } else if body_rows > 0 && selected >= offset + body_rows {
        offset = selected + 1 - body_rows;
    }

    let shown: Vec<&DownloadSummary> = visible
        .iter()
        .skip(offset)
        .take(body_rows)
        .copied()
        .collect();
    let rows: Vec<Row> = shown
        .iter()
        .map(|d| {
            let row = Row::new(vec![
                Cell::from(d.id.clone()),
                Cell::from(d.filename.clone()),
                Cell::from(status_label(d.status)).style(theme::status_style(d.status)),
                Cell::from(progress_bar(fraction_done(d), TABLE_BAR_WIDTH)),
                Cell::from(size_text(d)),
                Cell::from(format_speed(d.speed_bps)),
                Cell::from(format_eta(d.eta_secs)),
            ]);
            if app.selected.as_deref() == Some(d.id.as_str()) {
                row.style(theme::SELECTED_ROW)
            } else {
                row
            }
        })
        .collect();
    let hits = shown
        .iter()
        .enumerate()
        .map(|(i, d)| {
            (
                Rect::new(inner.x, inner.y + 1 + i as u16, inner.width, 1),
                d.id.clone(),
            )
        })
        .collect();

    let header = Row::new(vec![
        "ID", "NAME", "STATUS", "PROGRESS", "SIZE", "SPEED", "ETA",
    ])
    .style(theme::TABLE_HEADER);
    let widths = [
        Constraint::Length(6),
        Constraint::Min(12),
        Constraint::Length(9),
        Constraint::Length(TABLE_BAR_WIDTH as u16 + 5),
        Constraint::Length(21),
        Constraint::Length(12),
        Constraint::Length(8),
    ];
    frame.render_widget(Table::new(rows, widths).header(header), inner);
    app.table_offset = offset;
    hits
}
