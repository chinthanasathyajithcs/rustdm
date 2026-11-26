//! The connection lanes: one progress bar per connection.

use crate::ipc::protocol::{PieceMap, TelemetryTick};
use crate::tui::format::format_bytes;
use crate::tui::theme;
use crate::tui::widgets::table::progress_spans;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Connection lanes listed in the details pane before "and N more".
pub const MAX_LANE_LINES: usize = 8;

/// How many lines the lanes need, title and "and N more" included; 0 for none.
pub fn lane_lines(tick: Option<&TelemetryTick>) -> u16 {
    let lanes = tick.map_or(0, |t| match &t.pieces {
        Some(map) => map.running.len(),
        None => t.lanes.len(),
    });
    match lanes {
        0 => 0,
        n => (n.min(MAX_LANE_LINES + 1) + 1) as u16,
    }
}

pub fn render_lanes(frame: &mut Frame, tick: Option<&TelemetryTick>, area: Rect) {
    if let Some(map) = tick.and_then(|t| t.pieces.as_ref()) {
        return render_pieces(frame, map, area);
    }
    let Some(t) = tick.filter(|t| !t.lanes.is_empty()) else {
        return;
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("Connections ({} active)", t.active_connections),
        theme::MUTED,
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
            theme::WORKING
        } else {
            theme::SUCCESS
        };
        let mut spans = vec![Span::styled(format!("#{:<3}", i + 1), theme::MUTED)];
        spans.extend(progress_spans(fraction, bar_width, style));
        spans.push(Span::raw(format!(
            "  {} of {}",
            format_bytes(lane.downloaded_bytes),
            format_bytes(len)
        )));
        lines.push(Line::from(spans));
    }
    lines.extend(more_line(t.lanes.len()));
    frame.render_widget(Paragraph::new(lines), area);
}

/// "… and 3 more" when `lanes` are more than the pane lists.
fn more_line(lanes: usize) -> Option<Line<'static>> {
    (lanes > MAX_LANE_LINES).then(|| {
        Line::from(Span::styled(
            format!("… and {} more", lanes - MAX_LANE_LINES),
            theme::MUTED,
        ))
    })
}

/// One line per piece being fetched: `piece 7  ████░░  50%  1.0 MiB of 2.0 MiB`.
fn render_pieces(frame: &mut Frame, map: &PieceMap, area: Rect) {
    if map.running.is_empty() {
        return;
    }
    let mut lines = vec![Line::from(Span::styled(
        format!("Downloading {} pieces at once", map.running.len()),
        theme::MUTED,
    ))];
    let bar_width = (area.width as usize).saturating_sub(44).clamp(5, 30);
    for lane in map.running.iter().take(MAX_LANE_LINES) {
        let fraction = lane
            .total_bytes
            .filter(|&len| len > 0)
            .map(|len| (lane.downloaded_bytes as f64 / len as f64).min(1.0));
        let mut spans = vec![Span::styled(
            format!("piece {:<6}", lane.index + 1),
            theme::MUTED,
        )];
        spans.extend(progress_spans(fraction, bar_width, theme::WORKING));
        let got = format_bytes(lane.downloaded_bytes);
        spans.push(Span::raw(match lane.total_bytes {
            Some(len) => format!("  {got} of {}", format_bytes(len)),
            None => format!("  {got}"),
        }));
        lines.push(Line::from(spans));
    }
    lines.extend(more_line(map.running.len()));
    frame.render_widget(Paragraph::new(lines), area);
}
