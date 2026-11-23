//! The downloads table, and the progress bar and size text it shares.

use crate::ipc::protocol::DownloadSummary;
use crate::tui::format::{format_bytes, format_eta, format_speed, fraction_done, status_span};
use crate::tui::state::App;
use crate::tui::theme;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

/// Width of the progress bar in the table's PROGRESS column.
pub const TABLE_BAR_WIDTH: usize = 10;
/// The shorter bar used when a full one would push SPEED out.
const NARROW_BAR_WIDTH: usize = 6;
/// Fewest cells the NAME column gets before another column is dropped.
const NAME_MIN_WIDTH: u16 = 12;
/// Space ratatui leaves between columns.
const COLUMN_GAP: u16 = 1;

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

    /// Cells this column needs to show its widest value whole; NAME takes
    /// whatever is left.
    fn width(self, bar_width: usize) -> u16 {
        match self {
            Column::Id => 6,
            Column::Name => NAME_MIN_WIDTH,
            Column::Status => 9,
            // The bar, a space and "100%".
            Column::Progress => bar_width as u16 + 5,
            // "1023.9 MiB / 1023.9 MiB"
            Column::Size => 23,
            // "1023.9 KiB/s"
            Column::Speed => 12,
            Column::Eta => 8,
        }
    }

    fn cell(self, d: &DownloadSummary, bar_width: usize, stalled: bool) -> Cell<'static> {
        match self {
            Column::Id => Cell::from(d.id.clone()),
            Column::Name => Cell::from(d.filename.clone()),
            Column::Status => Cell::from(status_span(d.status, stalled)),
            Column::Progress => Cell::from(Line::from(progress_spans(
                fraction_done(d),
                bar_width,
                theme::BAR,
            ))),
            Column::Size => Cell::from(size_text(d)),
            Column::Speed => Cell::from(format_speed(d.speed_bps)),
            Column::Eta => Cell::from(format_eta(d.eta_secs)),
        }
    }
}

/// Which columns the table shows, and how wide its progress bars are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableLayout {
    pub columns: Vec<Column>,
    pub bar_width: usize,
}

impl TableLayout {
    fn new(columns: &[Column], bar_width: usize) -> Self {
        Self {
            columns: columns.to_vec(),
            bar_width,
        }
    }

    /// Cells needed to show every column whole.
    fn needed(&self) -> u16 {
        let cells: u16 = self.columns.iter().map(|c| c.width(self.bar_width)).sum();
        cells + COLUMN_GAP * (self.columns.len() as u16 - 1)
    }

    fn constraints(&self) -> Vec<Constraint> {
        self.columns
            .iter()
            .map(|c| match c {
                Column::Name => Constraint::Min(NAME_MIN_WIDTH),
                other => Constraint::Length(other.width(self.bar_width)),
            })
            .collect()
    }
}

/// The fullest layout that fits `width` cells without cutting a value short:
/// ETA goes first, then SIZE (the details pane shows both), then the bar
/// shortens, then SPEED goes. Narrower than the last, NAME is squeezed.
pub fn table_layout(width: u16) -> TableLayout {
    use Column::*;
    let candidates = [
        TableLayout::new(
            &[Id, Name, Status, Progress, Size, Speed, Eta],
            TABLE_BAR_WIDTH,
        ),
        TableLayout::new(&[Id, Name, Status, Progress, Size, Speed], TABLE_BAR_WIDTH),
        TableLayout::new(&[Id, Name, Status, Progress, Speed], TABLE_BAR_WIDTH),
        TableLayout::new(&[Id, Name, Status, Progress, Speed], NARROW_BAR_WIDTH),
        TableLayout::new(&[Id, Name, Status, Progress], NARROW_BAR_WIDTH),
    ];
    let narrowest = candidates[candidates.len() - 1].clone();
    candidates
        .into_iter()
        .find(|layout| layout.needed() <= width)
        .unwrap_or(narrowest)
}

/// "██████░░░░ 60%" for a known size, or blanks while unknown.
pub fn progress_bar(fraction: Option<f64>, width: usize) -> String {
    progress_spans(fraction, width, Style::new())
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

/// `progress_bar` in colour: the filled part in `fill`, the rest as a faint track.
pub fn progress_spans(fraction: Option<f64>, width: usize, fill: Style) -> Vec<Span<'static>> {
    let (filled, label) = match fraction {
        Some(f) => (
            ((f * width as f64).round() as usize).min(width),
            format!(" {:>3.0}%", f * 100.0),
        ),
        None => (0, " ?".to_string()),
    };
    vec![
        Span::styled("█".repeat(filled), fill),
        Span::styled("░".repeat(width - filled), theme::BAR_TRACK),
        Span::raw(label),
    ]
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
        .border_style(theme::FRAME);
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
    let layout = table_layout(inner.width);
    let rows: Vec<Row> = shown
        .iter()
        .map(|d| {
            let stalled = app.stall_of(d).is_some();
            let row = Row::new(
                layout
                    .columns
                    .iter()
                    .map(|c| c.cell(d, layout.bar_width, stalled)),
            );
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

    let header = Row::new(layout.columns.iter().map(|c| c.header())).style(theme::TABLE_HEADER);
    frame.render_widget(
        Table::new(rows, layout.constraints())
            .header(header)
            .column_spacing(COLUMN_GAP),
        inner,
    );
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
    fn columns_drop_eta_first_then_size_then_the_bar_shortens_then_speed_goes() {
        let has = |width: u16, column: Column| table_layout(width).columns.contains(&column);
        assert!(has(91, Column::Eta));
        assert!(!has(90, Column::Eta));
        assert!(has(82, Column::Size));
        assert!(!has(81, Column::Size));
        assert_eq!(table_layout(58).bar_width, TABLE_BAR_WIDTH);
        assert!(table_layout(57).bar_width < TABLE_BAR_WIDTH);
        assert!(has(54, Column::Speed));
        assert!(!has(53, Column::Speed));
        assert!(has(20, Column::Name) && has(20, Column::Progress));
    }

    #[test]
    fn an_80_column_terminal_keeps_the_full_bar_and_gives_name_the_room() {
        // 80 columns leave 78 inside the border.
        let layout = table_layout(78);
        assert_eq!(layout.bar_width, TABLE_BAR_WIDTH);
        assert!(layout.columns.contains(&Column::Speed));
        assert!(
            !layout.columns.contains(&Column::Size),
            "the details pane shows the size"
        );
        let name_cells = 78 - layout.needed() + NAME_MIN_WIDTH;
        assert!(name_cells >= 30, "NAME gets {name_cells} cells");
    }

    #[test]
    fn a_choosing_row_shows_its_status_whole() {
        for width in [120, 80] {
            let mut app = sample_app();
            app.set_downloads(vec![summary("dl_1", DownloadStatus::Choosing)]);
            let text = draw(&mut app, width, 30);
            assert!(text.contains("choose "), "{width}: {text}");
        }
    }

    #[test]
    fn every_cell_shows_whole_at_any_width() {
        for width in [140, 100, 92, 91, 85, 80, 70, 66, 60, 50] {
            let mut app = sample_app();
            let text = draw(&mut app, width, 30);
            let header = text.lines().find(|l| l.contains("NAME")).unwrap();
            let row = text.lines().find(|l| l.contains("dl_1")).unwrap();
            let mut wanted = vec!["ubuntu.iso", "active", " 40%"];
            if header.contains("SIZE") {
                wanted.push("40.0 MiB / 100.0 MiB");
            }
            if header.contains("SPEED") {
                wanted.push("2.0 MiB/s");
            }
            if header.contains("ETA") {
                wanted.push("30s");
            }
            for piece in wanted {
                assert!(row.contains(piece), "{piece:?} cut at {width}:\n{text}");
            }
        }
    }

    #[test]
    fn progress_bars_draw_the_filled_part_in_the_accent_and_the_rest_as_a_track() {
        let mut app = sample_app();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| crate::tui::screens::render(f, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let y = (0..buffer.area.height)
            .find(|&y| buffer[(1, y)].symbol() == "d" && buffer[(4, y)].symbol() == "1")
            .unwrap();
        let cells: Vec<_> = (0..buffer.area.width).map(|x| &buffer[(x, y)]).collect();
        let filled = cells.iter().find(|c| c.symbol() == "█").unwrap();
        let empty = cells.iter().find(|c| c.symbol() == "░").unwrap();
        assert_eq!(filled.fg, theme::palette::AMBER);
        assert_eq!(empty.fg, theme::palette::TRACK);
    }

    #[test]
    fn a_stalled_download_says_so_in_the_status_column() {
        let mut app = sample_app();
        app.ticks.get_mut("dl_1").unwrap().stall = Some("no data for 42s".to_string());
        let text = draw(&mut app, 80, 24);
        let row = text.lines().find(|l| l.contains("dl_1")).unwrap();
        assert!(row.contains("stalled"), "{text}");
        assert!(!row.contains("active"), "{text}");
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
