use crate::ipc::protocol::{DownloadStatus, DownloadSummary, TelemetryTick};
use crate::tui::app::{App, Button, HitMap, Mode, Tab};
use crate::tui::format::{
    format_bytes, format_eta, format_speed, format_speed_cap, fraction_done, status_label,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table};
use ratatui::Frame;

/// Height of the details pane when the window is tall enough.
const INSPECTOR_HEIGHT: u16 = 16;
/// Smallest table height (borders, header and two rows) kept before the details pane shrinks.
const MIN_TABLE_HEIGHT: u16 = 5;
/// Connection lanes listed in the details pane before "and N more".
const MAX_LANE_LINES: usize = 8;
/// Width of the progress bar in the table's PROGRESS column.
const TABLE_BAR_WIDTH: usize = 10;

/// One cell of the segment map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapCell {
    Empty,
    Partial,
    Done,
    /// A connection is writing inside this cell right now.
    Cursor,
}

/// Split a file of `total` bytes into `cells` equal cells and mark how much of each
/// is on disk. `completed` holds `[start, end)` ranges; `cursors` are the write
/// positions of active connections.
pub fn segment_cells(
    total: u64,
    completed: &[(u64, u64)],
    cursors: &[u64],
    cells: usize,
) -> Vec<MapCell> {
    if total == 0 || cells == 0 {
        return vec![MapCell::Empty; cells];
    }
    let bounds = |i: usize| (i as u128 * total as u128 / cells as u128) as u64;

    let mut map: Vec<MapCell> = (0..cells)
        .map(|i| {
            let (start, end) = (bounds(i), bounds(i + 1));
            let covered: u64 = completed
                .iter()
                .map(|&(s, e)| e.min(end).saturating_sub(s.max(start)))
                .sum();
            if end > start && covered >= end - start {
                MapCell::Done
            } else if covered > 0 {
                MapCell::Partial
            } else {
                MapCell::Empty
            }
        })
        .collect();

    for &cursor in cursors {
        if cursor < total {
            let i = (cursor as u128 * cells as u128 / total as u128) as usize;
            map[i.min(cells - 1)] = MapCell::Cursor;
        }
    }
    map
}

/// Positions of the toolbar buttons on `area`'s first line, left to right.
pub fn toolbar_layout(area: Rect) -> Vec<(Rect, Button)> {
    let mut x = area.x;
    let mut buttons = Vec::new();
    for button in Button::ALL {
        let width = button.label().len() as u16 + 4;
        if x + width > area.right() {
            break;
        }
        buttons.push((Rect::new(x, area.y, width, 1), button));
        x += width + 1;
    }
    buttons
}

fn tab_label(app: &App, tab: Tab) -> String {
    format!(" {} ({}) ", tab.title(), app.count(tab))
}

/// Positions of the tabs on `area`'s first line, left to right.
pub fn tabs_layout(app: &App, area: Rect) -> Vec<(Rect, Tab)> {
    let mut x = area.x;
    let mut tabs = Vec::new();
    for tab in Tab::ALL {
        let width = tab_label(app, tab).len() as u16;
        if x + width > area.right() {
            break;
        }
        tabs.push((Rect::new(x, area.y, width, 1), tab));
        x += width + 1;
    }
    tabs
}

/// Draw the whole dashboard and record what is clickable in `app.hits`.
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let inspector_height = INSPECTOR_HEIGHT.min(area.height.saturating_sub(4 + MIN_TABLE_HEIGHT));
    let [header, toolbar, tabs, table, inspector, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(MIN_TABLE_HEIGHT),
        Constraint::Length(inspector_height),
        Constraint::Length(1),
    ])
    .areas(area);

    let mut hits = HitMap::default();
    render_header(frame, app, header);
    hits.buttons = render_toolbar(frame, toolbar);
    hits.tabs = render_tabs(frame, app, tabs);
    hits.rows = render_table(frame, app, table);
    render_inspector(frame, app, inspector);
    render_footer(frame, app, footer);
    app.hits = hits;
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let status = if !app.connected {
        Span::styled("rdmd not connected", Style::new().fg(Color::Red))
    } else {
        let limits = match &app.limits {
            Some(l) if l.max_bytes_per_sec > 0 => format!(
                "{} at once · speed cap {}",
                l.max_active,
                format_speed(l.max_bytes_per_sec)
            ),
            Some(l) => format!("{} at once · no speed cap", l.max_active),
            None => String::new(),
        };
        Span::styled(
            format!("rdmd connected  {limits}"),
            Style::new().fg(Color::Gray),
        )
    };
    let total_speed: u64 = app.downloads.iter().map(|d| d.speed_bps).sum();
    let line = Line::from(vec![
        Span::styled(
            " rustdm ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        status,
        Span::raw("  "),
        Span::styled(
            format!("total {}", format_speed(total_speed)),
            Style::new().fg(Color::Cyan),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_toolbar(frame: &mut Frame, area: Rect) -> Vec<(Rect, Button)> {
    let buttons = toolbar_layout(area);
    for (rect, button) in &buttons {
        let style = match button {
            Button::Add => Style::new().fg(Color::Black).bg(Color::Green),
            Button::Quit => Style::new().fg(Color::White).bg(Color::DarkGray),
            _ => Style::new().fg(Color::Black).bg(Color::Gray),
        };
        let text = Paragraph::new(format!("[ {} ]", button.label())).style(style);
        frame.render_widget(text, *rect);
    }
    buttons
}

fn render_tabs(frame: &mut Frame, app: &App, area: Rect) -> Vec<(Rect, Tab)> {
    let tabs = tabs_layout(app, area);
    for (rect, tab) in &tabs {
        let style = if *tab == app.tab {
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Gray)
        };
        frame.render_widget(Paragraph::new(tab_label(app, *tab)).style(style), *rect);
    }
    tabs
}

fn status_style(status: DownloadStatus) -> Style {
    let color = match status {
        DownloadStatus::Active => Color::Cyan,
        DownloadStatus::Queued => Color::Yellow,
        DownloadStatus::Pausing | DownloadStatus::Paused => Color::Gray,
        DownloadStatus::Retrying => Color::Magenta,
        DownloadStatus::Completed => Color::Green,
        DownloadStatus::Failed => Color::Red,
        DownloadStatus::Cancelled => Color::DarkGray,
    };
    Style::new().fg(color)
}

/// "██████░░░░ 60%" for a known size, or blanks while unknown.
fn progress_bar(fraction: Option<f64>, width: usize) -> String {
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

fn size_text(d: &DownloadSummary) -> String {
    match d.total_bytes {
        Some(total) => format!(
            "{} / {}",
            format_bytes(d.downloaded_bytes),
            format_bytes(total)
        ),
        None => format_bytes(d.downloaded_bytes),
    }
}

fn render_table(frame: &mut Frame, app: &mut App, area: Rect) -> Vec<(Rect, String)> {
    let block = Block::new()
        .borders(Borders::ALL)
        .title(" Downloads ")
        .border_style(Style::new().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible = app.visible();
    if visible.is_empty() {
        let text = if app.downloads.is_empty() {
            "No downloads yet. Press a, click [ + Add ], or paste a URL."
        } else {
            "Nothing in this tab."
        };
        frame.render_widget(
            Paragraph::new(text).style(Style::new().fg(Color::Gray)),
            inner,
        );
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
                Cell::from(status_label(d.status)).style(status_style(d.status)),
                Cell::from(progress_bar(fraction_done(d), TABLE_BAR_WIDTH)),
                Cell::from(size_text(d)),
                Cell::from(format_speed(d.speed_bps)),
                Cell::from(format_eta(d.eta_secs)),
            ]);
            if app.selected.as_deref() == Some(d.id.as_str()) {
                row.style(
                    Style::new()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
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
    .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD));
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

fn render_inspector(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let Some(d) = app.selected_summary() else {
        let block = Block::new().borders(Borders::ALL).title(" Details ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("Select a download to see its segments, connections and speed.")
                .style(Style::new().fg(Color::Gray)),
            inner,
        );
        return;
    };

    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!(" {} ", d.filename))
        .border_style(Style::new().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let tick = app.ticks.get(&d.id);
    let lane_lines = tick.map_or(0, |t| t.lanes.len().min(MAX_LANE_LINES + 1)) as u16;
    let [info, map, lanes, speed] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Max(lane_lines.max(1) + 1),
        Constraint::Min(0),
    ])
    .areas(inner);

    render_info(frame, d, tick, info);
    render_segment_map(frame, d, tick, map);
    render_lanes(frame, tick, lanes);
    render_speed(frame, app, d, speed);
}

fn render_info(frame: &mut Frame, d: &DownloadSummary, tick: Option<&TelemetryTick>, area: Rect) {
    let label = Style::new().fg(Color::Gray);
    let mut first = vec![
        Span::styled("Status ", label),
        Span::styled(status_label(d.status), status_style(d.status)),
        Span::styled("   Size ", label),
        Span::raw(size_text(d)),
        Span::styled("   Speed ", label),
        Span::raw(format_speed(d.speed_bps)),
        Span::styled("   ETA ", label),
        Span::raw(format_eta(d.eta_secs)),
    ];
    if let Some(t) = tick {
        first.push(Span::styled("   Connections ", label));
        first.push(Span::raw(t.active_connections.to_string()));
    }
    if d.max_bytes_per_sec > 0 {
        first.push(Span::styled("   Cap ", label));
        first.push(Span::raw(format_speed_cap(d.max_bytes_per_sec)));
    }
    let last = match &d.error_message {
        Some(err) => Line::from(vec![
            Span::styled("Error ", label),
            Span::styled(err.clone(), Style::new().fg(Color::Red)),
        ]),
        None => Line::from(vec![
            Span::styled("Saved to ", label),
            Span::raw(d.destination.display().to_string()),
        ]),
    };
    let lines = vec![
        Line::from(first),
        Line::from(vec![Span::styled("URL ", label), Span::raw(d.url.clone())]),
        last,
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_segment_map(
    frame: &mut Frame,
    d: &DownloadSummary,
    tick: Option<&TelemetryTick>,
    area: Rect,
) {
    if area.height < 2 || area.width == 0 {
        return;
    }
    let rows = (area.height - 1) as usize;
    let width = area.width as usize;
    let title = Line::from(Span::styled(
        "Segments  █ done  ▒ partial  ▌ connection  ░ missing",
        Style::new().fg(Color::Gray),
    ));

    let total = d
        .total_bytes
        .or(tick.and_then(|t| t.total_bytes))
        .unwrap_or(0);
    let cells = if d.status == DownloadStatus::Completed {
        vec![MapCell::Done; width * rows]
    } else if let (Some(t), true) = (tick, total > 0) {
        let completed: Vec<(u64, u64)> = t.segments.iter().map(|s| (s.start, s.end)).collect();
        let cursors: Vec<u64> = t
            .lanes
            .iter()
            .filter(|l| l.state == "active")
            .map(|l| l.start + l.downloaded_bytes)
            .collect();
        segment_cells(total, &completed, &cursors, width * rows)
    } else {
        let note = Paragraph::new(vec![
            title,
            Line::from(Span::styled(
                "The segment map appears while the download runs.",
                Style::new().fg(Color::DarkGray),
            )),
        ]);
        frame.render_widget(note, area);
        return;
    };

    let mut lines = vec![title];
    for row in cells.chunks(width) {
        let spans: Vec<Span> = row
            .iter()
            .map(|cell| match cell {
                MapCell::Done => Span::styled("█", Style::new().fg(Color::Green)),
                MapCell::Partial => Span::styled("▒", Style::new().fg(Color::Cyan)),
                MapCell::Cursor => Span::styled("▌", Style::new().fg(Color::Yellow)),
                MapCell::Empty => Span::styled("░", Style::new().fg(Color::DarkGray)),
            })
            .collect();
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_lanes(frame: &mut Frame, tick: Option<&TelemetryTick>, area: Rect) {
    let Some(t) = tick.filter(|t| !t.lanes.is_empty()) else {
        return;
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("Connections ({} active)", t.active_connections),
        Style::new().fg(Color::Gray),
    ))];
    let bar_width = (area.width as usize).saturating_sub(40).clamp(5, 30);
    for (i, lane) in t.lanes.iter().take(MAX_LANE_LINES).enumerate() {
        let len = lane.end.saturating_sub(lane.start);
        let fraction = if len > 0 {
            Some(lane.downloaded_bytes as f64 / len as f64)
        } else {
            Some(1.0)
        };
        let style = if lane.state == "active" {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new().fg(Color::Green)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("#{:<3}", i + 1), Style::new().fg(Color::Gray)),
            Span::styled(progress_bar(fraction, bar_width), style),
            Span::raw(format!(
                "  {} of {}",
                format_bytes(lane.downloaded_bytes),
                format_bytes(len)
            )),
        ]));
    }
    if t.lanes.len() > MAX_LANE_LINES {
        lines.push(Line::from(Span::styled(
            format!("… and {} more", t.lanes.len() - MAX_LANE_LINES),
            Style::new().fg(Color::Gray),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_speed(frame: &mut Frame, app: &App, d: &DownloadSummary, area: Rect) {
    if area.height < 2 {
        return;
    }
    let history: Vec<u64> = app
        .speed_history
        .get(&d.id)
        .map(|h| h.iter().copied().collect())
        .unwrap_or_default();
    let peak = history.iter().copied().max().unwrap_or(0);
    let [label, graph] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                "Speed  {} now · {} peak",
                format_speed(d.speed_bps),
                format_speed(peak)
            ),
            Style::new().fg(Color::Gray),
        ))),
        label,
    );
    // Show the newest samples that fit, right-aligned like a scrolling chart.
    let start = history.len().saturating_sub(graph.width as usize);
    frame.render_widget(
        Sparkline::default()
            .data(&history[start..])
            .style(Style::new().fg(Color::Cyan)),
        graph,
    );
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = match (&app.mode, &app.notice) {
        (Mode::AddUrl(input), _) => {
            let prompt = "Add URL: ";
            let cursor_x = area.x + (prompt.len() + input.chars().count()) as u16;
            frame.set_cursor_position(Position::new(
                cursor_x.min(area.right().saturating_sub(1)),
                area.y,
            ));
            Line::from(vec![
                Span::styled(
                    prompt,
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::raw(input.clone()),
                Span::styled(
                    "   Enter adds · Esc cancels",
                    Style::new().fg(Color::DarkGray),
                ),
            ])
        }
        (Mode::Browse, Some(notice)) => {
            let color = if notice.is_error {
                Color::Red
            } else {
                Color::Green
            };
            Line::from(Span::styled(notice.text.clone(), Style::new().fg(color)))
        }
        (Mode::Browse, None) => Line::from(Span::styled(
            "a add · p pause · r resume · c cancel · x remove · ←/→ tabs · ↑/↓ select · ⇧↑/⇧↓ reorder · q quit",
            Style::new().fg(Color::DarkGray),
        )),
    };
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::SegmentProgress;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;

    #[test]
    fn segment_cells_mark_done_partial_empty_and_cursors() {
        // 100 bytes in 4 cells of 25: first done, second half done, rest empty.
        let cells = segment_cells(100, &[(0, 25), (25, 37)], &[80], 4);
        assert_eq!(
            cells,
            vec![
                MapCell::Done,
                MapCell::Partial,
                MapCell::Empty,
                MapCell::Cursor
            ]
        );
        assert_eq!(segment_cells(0, &[], &[], 3), vec![MapCell::Empty; 3]);
    }

    #[test]
    fn toolbar_buttons_sit_side_by_side_and_stop_at_the_edge() {
        let buttons = toolbar_layout(Rect::new(0, 1, 200, 1));
        assert_eq!(buttons.len(), Button::ALL.len());
        for pair in buttons.windows(2) {
            assert!(pair[0].0.right() < pair[1].0.x, "buttons must not touch");
        }
        let narrow = toolbar_layout(Rect::new(0, 1, 20, 1));
        assert!(narrow.iter().all(|(r, _)| r.right() <= 20));
    }

    fn sample_app() -> App {
        let mut app = App {
            connected: true,
            ..App::default()
        };
        app.set_downloads(vec![DownloadSummary {
            id: "dl_1".to_string(),
            url: "http://example.com/ubuntu.iso".to_string(),
            destination: PathBuf::from("/downloads/ubuntu.iso"),
            filename: "ubuntu.iso".to_string(),
            status: DownloadStatus::Active,
            downloaded_bytes: 40 * 1024 * 1024,
            total_bytes: Some(100 * 1024 * 1024),
            speed_bps: 2 * 1024 * 1024,
            eta_secs: Some(30),
            error_message: None,
            max_bytes_per_sec: 0,
        }]);
        app.apply_tick(TelemetryTick {
            id: "dl_1".to_string(),
            downloaded_bytes: 40 * 1024 * 1024,
            total_bytes: Some(100 * 1024 * 1024),
            speed_bps: 2 * 1024 * 1024,
            active_connections: 2,
            segments: vec![SegmentProgress {
                start: 0,
                end: 40 * 1024 * 1024,
                downloaded_bytes: 40 * 1024 * 1024,
                state: "completed".to_string(),
            }],
            lanes: vec![
                SegmentProgress {
                    start: 0,
                    end: 50 * 1024 * 1024,
                    downloaded_bytes: 40 * 1024 * 1024,
                    state: "active".to_string(),
                },
                SegmentProgress {
                    start: 50 * 1024 * 1024,
                    end: 100 * 1024 * 1024,
                    downloaded_bytes: 0,
                    state: "active".to_string(),
                },
            ],
        });
        app
    }

    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn dashboard_shows_table_details_and_records_click_targets() {
        let mut app = sample_app();
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);

        for expected in [
            "rustdm",
            "[ + Add ]",
            "[ || Pause ]",
            "Downloading (1)",
            "ubuntu.iso",
            "40.0 MiB / 100.0 MiB",
            "Segments",
            "Connections (2 active)",
            "Speed",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        assert_eq!(app.hits.buttons.len(), Button::ALL.len());
        assert_eq!(app.hits.tabs.len(), Tab::ALL.len());
        assert_eq!(app.hits.rows.len(), 1);
        assert_eq!(app.hits.rows[0].1, "dl_1");
    }

    #[test]
    fn small_windows_still_render_without_panicking() {
        let mut app = sample_app();
        for (w, h) in [(20, 5), (40, 10), (80, 12), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &mut app)).unwrap();
        }
    }

    #[test]
    fn add_mode_shows_the_url_being_typed() {
        let mut app = sample_app();
        app.mode = Mode::AddUrl("http://example.com/a".to_string());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(screen_text(&terminal).contains("Add URL: http://example.com/a"));
    }
}
