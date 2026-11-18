//! The three tool rows of the Settings screen: what each shows after its name,
//! and the notes under it.

use super::{put_line, INDENT, LABEL_WIDTH};
use crate::ipc::protocol::{ToolName, ToolPhase};
use crate::tools::display::{age_text, state_text};
use crate::tools::release::now_unix;
use crate::tui::state::App;
use crate::tui::theme;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::Frame;

/// Under a tool's row: why its last attempt failed, and, when it is the
/// selected row, which keys act on it.
pub(super) fn render_tool_notes(
    frame: &mut Frame,
    app: &App,
    tool: ToolName,
    selected: bool,
    inner: Rect,
    y: &mut u16,
) {
    let indent = || Span::raw(" ".repeat((INDENT + LABEL_WIDTH) as usize));
    let failure = app
        .tools
        .iter()
        .find(|i| i.tool == tool && i.phase == ToolPhase::Idle)
        .and_then(|i| i.error.clone());
    if let Some(error) = failure {
        let line = Line::from(vec![indent(), Span::styled(error, theme::ERROR)]);
        put_line(frame, inner, y, line);
    }
    if selected {
        let hint = Span::styled("u Update · b Go back one version", theme::DIM);
        put_line(frame, inner, y, Line::from(vec![indent(), hint]));
    }
}

/// What a tool row shows after its name: version, last check, previous
/// version, or the progress of what it is doing now.
pub(super) fn tool_value(app: &App, tool: ToolName, style: Style) -> Line<'static> {
    let Some(info) = app.tools.iter().find(|i| i.tool == tool) else {
        return Line::from(Span::styled("—", style));
    };
    if info.phase != ToolPhase::Idle {
        return Line::from(Span::styled(state_text(info), style));
    }
    let Some(version) = &info.installed else {
        return Line::from(Span::styled("not installed", style));
    };
    let mut spans = vec![Span::styled(version.clone(), style)];
    let mut details = format!("  checked {}", age_text(info.last_checked_unix, now_unix()));
    if let Some(previous) = &info.previous {
        details.push_str(&format!("  previous {previous}"));
    }
    spans.push(Span::styled(details, theme::DIM));
    Line::from(spans)
}
