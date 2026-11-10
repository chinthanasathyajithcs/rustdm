//! The inspector pane under the table: details of the selected download.

use crate::ipc::protocol::{DownloadSummary, TelemetryTick};
use crate::tui::format::{format_eta, format_speed, format_speed_cap, status_label};
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
        .border_style(theme::ACCENT);
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
    let label = theme::MUTED;
    let mut first = vec![
        Span::styled("Status ", label),
        Span::styled(status_label(d.status), theme::status_style(d.status)),
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
            Span::styled(err.clone(), theme::ERROR),
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
