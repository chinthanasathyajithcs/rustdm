//! The segment map: which parts of the file are on disk, drawn as cells.

use crate::ipc::protocol::{DownloadStatus, DownloadSummary, TelemetryTick};
use crate::tui::theme;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

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

pub fn render_segment_map(
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
        theme::MUTED,
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
                theme::DIM,
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
                MapCell::Done => Span::styled("█", theme::SUCCESS),
                MapCell::Partial => Span::styled("▒", theme::ACCENT),
                MapCell::Cursor => Span::styled("▌", theme::WORKING),
                MapCell::Empty => Span::styled("░", theme::DIM),
            })
            .collect();
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
