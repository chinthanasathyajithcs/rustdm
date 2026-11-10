//! The connection lanes: one progress bar per connection.

use crate::ipc::protocol::TelemetryTick;
use crate::tui::format::format_bytes;
use crate::tui::theme;
use crate::tui::widgets::table::progress_bar;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Connection lanes listed in the details pane before "and N more".
pub const MAX_LANE_LINES: usize = 8;

pub fn render_lanes(frame: &mut Frame, tick: Option<&TelemetryTick>, area: Rect) {
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
        lines.push(Line::from(vec![
            Span::styled(format!("#{:<3}", i + 1), theme::MUTED),
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
            theme::MUTED,
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}
