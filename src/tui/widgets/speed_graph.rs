//! The speed graph of the selected download.

use crate::ipc::protocol::DownloadSummary;
use crate::tui::format::format_speed;
use crate::tui::state::App;
use crate::tui::theme;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Sparkline};
use ratatui::Frame;

pub fn render_speed(frame: &mut Frame, app: &App, d: &DownloadSummary, area: Rect) {
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
            theme::MUTED,
        ))),
        label,
    );
    // Show the newest samples that fit, right-aligned like a scrolling chart.
    let start = history.len().saturating_sub(graph.width as usize);
    frame.render_widget(
        Sparkline::default()
            .data(&history[start..])
            .style(theme::GRAPH),
        graph,
    );
}
