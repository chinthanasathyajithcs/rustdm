//! The inspector pane under the table: details of the selected download.

use crate::ipc::protocol::{DownloadSummary, TelemetryTick};
use crate::tui::format::{format_eta, format_speed, format_speed_cap, status_span};
use crate::tui::state::App;
use crate::tui::theme;
use crate::tui::widgets::lanes::{render_lanes, MAX_LANE_LINES};
use crate::tui::widgets::segment_map::render_segment_map;
use crate::tui::widgets::speed_graph::render_speed;
use crate::tui::widgets::table::size_text;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

pub fn render_inspector(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let Some(d) = app.selected_summary() else {
        let block = Block::new().borders(Borders::ALL).title(" Details ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("Select a download to see its segments, connections and speed.")
                .style(theme::MUTED),
            inner,
        );
        return;
    };

    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!(" {} ", d.filename))
        .title_style(theme::PANEL_TITLE)
        .border_style(theme::PANEL);
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

    render_info(frame, d, tick, app.stall_of(d), info);
    render_segment_map(frame, d, tick, map);
    render_lanes(frame, tick, lanes);
    render_speed(frame, app, d, speed);
}

/// Space between the status line's items.
const ITEM_GAP: &str = "   ";

fn render_info(
    frame: &mut Frame,
    d: &DownloadSummary,
    tick: Option<&TelemetryTick>,
    stall: Option<&str>,
    area: Rect,
) {
    let label = theme::MUTED;
    let first = status_line(d, tick, stall.is_some(), area.width as usize);
    let last = match (stall, &d.error_message) {
        (Some(reason), _) => Line::from(vec![
            Span::styled("Waiting ", label),
            Span::styled(reason.to_string(), theme::STALLED),
        ]),
        (None, Some(err)) => Line::from(vec![
            Span::styled("Error ", label),
            Span::styled(err.clone(), theme::ERROR),
        ]),
        (None, None) => Line::from(vec![
            Span::styled("Saved to ", label),
            Span::raw(d.destination.display().to_string()),
        ]),
    };
    let lines = vec![
        first,
        Line::from(vec![Span::styled("URL ", label), Span::raw(d.url.clone())]),
        last,
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

/// "Status active   Size …   Speed …   ETA …   Cap …   Connections …", most
/// useful first, leaving out whole items that don't fit in `width` columns.
fn status_line(
    d: &DownloadSummary,
    tick: Option<&TelemetryTick>,
    stalled: bool,
    width: usize,
) -> Line<'static> {
    let mut items: Vec<(&str, Span<'static>)> = vec![
        ("Status", status_span(d.status, stalled)),
        ("Size", Span::raw(size_text(d))),
        ("Speed", Span::raw(format_speed(d.speed_bps))),
        ("ETA", Span::raw(format_eta(d.eta_secs))),
    ];
    if d.max_bytes_per_sec > 0 {
        items.push(("Cap", Span::raw(format_speed_cap(d.max_bytes_per_sec))));
    }
    if let Some(t) = tick {
        items.push(("Connections", Span::raw(t.active_connections.to_string())));
    }

    let mut spans = Vec::new();
    let mut used = 0;
    for (name, value) in items {
        let gap = if spans.is_empty() { "" } else { ITEM_GAP };
        let name = format!("{gap}{name} ");
        let needed = name.chars().count() + value.content.chars().count();
        if used + needed > width {
            continue;
        }
        used += needed;
        spans.push(Span::styled(name, theme::MUTED));
        spans.push(value);
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use crate::tui::state::test_support::{sample_app, screen_text};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The inspector's first line, drawn on a terminal `width` columns wide.
    fn status_line(width: u16) -> String {
        let mut app = sample_app();
        let mut capped = app.downloads[0].clone();
        capped.max_bytes_per_sec = 512 * 1024;
        app.set_downloads(vec![capped]);
        let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
        terminal
            .draw(|f| crate::tui::screens::render(f, &mut app))
            .unwrap();
        let text = screen_text(&terminal);
        text.lines()
            .find(|l| l.contains("Status "))
            .unwrap()
            .to_string()
    }

    #[test]
    fn a_stalled_download_shows_why_in_the_details() {
        let mut app = sample_app();
        let reason = "no data for 42s: server busy (HTTP 503), retrying in 8s";
        app.ticks.get_mut("dl_1").unwrap().stall = Some(reason.to_string());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| crate::tui::screens::render(f, &mut app))
            .unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains(&format!("Waiting {reason}")), "{text}");
        assert!(text.contains("Status stalled"), "{text}");
    }

    #[test]
    fn the_status_line_drops_whole_items_that_do_not_fit() {
        let wide = status_line(140);
        assert!(wide.contains("Connections 2"), "{wide}");
        assert!(wide.contains("Cap 512.0 KiB/s"), "{wide}");

        let medium = status_line(100);
        assert!(medium.contains("Cap 512.0 KiB/s"), "{medium}");
        assert!(
            !medium.contains("Connec"),
            "the cap outranks connections: {medium}"
        );

        // At 80 the table has no ETA column, so ETA must stay here.
        let narrow = status_line(80);
        assert!(narrow.contains("ETA 30s"), "{narrow}");
        assert!(
            !narrow.contains("Connec"),
            "nothing is cut halfway: {narrow}"
        );
    }
}
