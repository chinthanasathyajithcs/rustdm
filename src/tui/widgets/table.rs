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
/// Below this many columns the ETA column is left out.
const ETA_MIN_WIDTH: u16 = 80;
/// Below this many columns the SPEED column is left out too.
const SPEED_MIN_WIDTH: u16 = 64;

/// The table's columns, left to right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    Id,
    Name,
    Status,
    Progress,
    Size,
    Speed,
    Eta,
}

impl Column {
    fn header(self) -> &'static str {
        match self {
            Column::Id => "ID",
            Column::Name => "NAME",
            Column::Status => "STATUS",
            Column::Progress => "PROGRESS",
            Column::Size => "SIZE",
            Column::Speed => "SPEED",
            Column::Eta => "ETA",
        }
    }

    fn width(self) -> Constraint {
        match self {
            Column::Id => Constraint::Length(6),
            Column::Name => Constraint::Min(12),
            Column::Status => Constraint::Length(9),
            Column::Progress => Constraint::Length(TABLE_BAR_WIDTH as u16 + 5),
            Column::Size => Constraint::Length(21),
            Column::Speed => Constraint::Length(12),
            Column::Eta => Constraint::Length(8),
        }
    }

    fn cell(self, d: &DownloadSummary) -> Cell<'static> {
        match self {
            Column::Id => Cell::from(d.id.clone()),
            Column::Name => Cell::from(d.filename.clone()),
            Column::Status => {
                Cell::from(status_label(d.status)).style(theme::status_style(d.status))
            }
            Column::Progress => Cell::from(progress_bar(fraction_done(d), TABLE_BAR_WIDTH)),
            Column::Size => Cell::from(size_text(d)),
            Column::Speed => Cell::from(format_speed(d.speed_bps)),
            Column::Eta => Cell::from(format_eta(d.eta_secs)),
        }
    }
}

/// The columns that fit a table `width` columns wide: ETA goes first, then SPEED.
pub fn table_columns(width: u16) -> Vec<Column> {
    use Column::*;
    [Id, Name, Status, Progress, Size, Speed, Eta]
        .into_iter()
        .filter(|c| match c {
            Eta => width >= ETA_MIN_WIDTH,
            Speed => width >= SPEED_MIN_WIDTH,
            _ => true,
        })
        .collect()
}

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
    let columns = table_columns(area.width);
    let rows: Vec<Row> = shown
        .iter()
        .map(|d| {
            let row = Row::new(columns.iter().map(|c| c.cell(d)));
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

    let header = Row::new(columns.iter().map(|c| c.header())).style(theme::TABLE_HEADER);
    let widths: Vec<Constraint> = columns.iter().map(|c| c.width()).collect();
    frame.render_widget(Table::new(rows, widths).header(header), inner);
    app.table_offset = offset;
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::DownloadStatus;
    use crate::tui::state::test_support::{sample_app, screen_text, summary};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| crate::tui::screens::render(f, app))
            .unwrap();
        screen_text(&terminal)
    }

    #[test]
    fn eta_drops_below_80_columns_and_speed_below_64() {
        assert!(table_columns(80).contains(&Column::Eta));
        assert!(!table_columns(79).contains(&Column::Eta));
        assert!(table_columns(79).contains(&Column::Speed));
        assert!(table_columns(64).contains(&Column::Speed));
        assert!(!table_columns(63).contains(&Column::Speed));
        assert!(table_columns(40).contains(&Column::Name));

        let mut app = sample_app();
        let header = |text: String| {
            text.lines()
                .find(|l| l.contains("NAME"))
                .unwrap()
                .to_string()
        };
        let wide = header(draw(&mut app, 120, 30));
        assert!(wide.contains("SPEED") && wide.contains("ETA"), "{wide}");
        let medium = header(draw(&mut app, 70, 30));
        assert!(
            medium.contains("SPEED") && !medium.contains("ETA"),
            "{medium}"
        );
        let narrow = header(draw(&mut app, 60, 30));
        assert!(!narrow.contains("SPEED"), "{narrow}");
    }

    #[test]
    fn long_urls_and_file_names_are_cut_not_wrapped() {
        let mut app = App::default();
        let mut long = summary("dl_1", DownloadStatus::Active);
        long.filename = "x".repeat(300);
        long.url = format!("http://example.com/{}", "y".repeat(300));
        app.set_downloads(vec![long, summary("dl_2", DownloadStatus::Queued)]);
        let text = draw(&mut app, 80, 24);
        // The table: from its header row to its bottom border.
        let table: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.contains("NAME"))
            .take_while(|l| !l.contains('└'))
            .collect();
        assert_eq!(table.len(), 3, "header and one row each:\n{text}");
        assert!(
            table[1].contains("dl_1") && table[1].contains("xxxxx"),
            "{text}"
        );
        assert!(table[2].contains("dl_2"), "{text}");
    }
}
