//! The segment map: which parts of the file are on disk, drawn as cells.

use crate::ipc::protocol::{DownloadStatus, DownloadSummary, PieceMap, TelemetryTick};
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

/// Split a video's `map.total` pieces over `cells` cells: a cell is done when
/// all its pieces are on disk, a cursor when one of them is being fetched.
pub fn piece_cells(map: &PieceMap, cells: usize) -> Vec<MapCell> {
    let total = map.total as usize;
    if total == 0 || cells == 0 {
        return vec![MapCell::Empty; cells];
    }
    let running: Vec<u64> = map.running.iter().map(|lane| lane.index).collect();
    let kept = |i: u64| i < map.written || map.waiting.contains(&i);
    (0..cells)
        .map(|cell| {
            let first = cell * total / cells;
            let end = ((cell + 1) * total / cells).max(first + 1);
            let pieces = first as u64..end as u64;
            let done = pieces.clone().filter(|&i| kept(i)).count();
            if pieces.clone().any(|i| running.contains(&i)) {
                MapCell::Cursor
            } else if done == pieces.count() {
                MapCell::Done
            } else if done > 0 {
                MapCell::Partial
            } else {
                MapCell::Empty
            }
        })
        .collect()
}

/// "Segments  █ done  ▒ partial  ▌ connection  ░ missing", each mark in its
/// map colour; for a video in pieces, "Pieces  █ done  ▌ downloading  ░ missing".
fn legend(pieces: bool) -> Line<'static> {
    let (title, marks) = if pieces {
        ("Pieces", PIECE_MARKS.as_slice())
    } else {
        ("Segments", SEGMENT_MARKS.as_slice())
    };
    let mut spans = vec![Span::styled(title, theme::MUTED)];
    for &(mark, style, name) in marks {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(mark, style));
        spans.push(Span::styled(format!(" {name}"), theme::MUTED));
    }
    Line::from(spans)
}

type Mark = (&'static str, ratatui::style::Style, &'static str);

const SEGMENT_MARKS: [Mark; 4] = [
    ("█", theme::SEGMENT_DONE, "done"),
    ("▒", theme::SEGMENT_PARTIAL, "partial"),
    ("▌", theme::SEGMENT_CURSOR, "connection"),
    // The missing mark is drawn faint on the map; the legend needs it visible.
    ("░", theme::DIM, "missing"),
];

const PIECE_MARKS: [Mark; 3] = [
    ("█", theme::SEGMENT_DONE, "done"),
    ("▌", theme::SEGMENT_CURSOR, "downloading"),
    ("░", theme::DIM, "missing"),
];

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
    let pieces = tick.and_then(|t| t.pieces.as_ref());
    let title = legend(pieces.is_some());

    let total = d
        .total_bytes
        .or(tick.and_then(|t| t.total_bytes))
        .unwrap_or(0);
    let cells = if d.status == DownloadStatus::Completed {
        vec![MapCell::Done; width * rows]
    } else if let Some(map) = pieces {
        piece_cells(map, width * rows)
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
                MapCell::Done => Span::styled("█", theme::SEGMENT_DONE),
                MapCell::Partial => Span::styled("▒", theme::SEGMENT_PARTIAL),
                MapCell::Cursor => Span::styled("▌", theme::SEGMENT_CURSOR),
                MapCell::Empty => Span::styled("░", theme::SEGMENT_MISSING),
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

    use crate::ipc::protocol::PieceLane;

    fn lane(index: u64) -> PieceLane {
        PieceLane {
            index,
            downloaded_bytes: 1,
            total_bytes: None,
        }
    }

    #[test]
    fn piece_cells_show_each_piece_when_there_is_room() {
        let map = PieceMap {
            total: 4,
            written: 1,
            waiting: vec![3],
            running: vec![lane(1)],
        };

        // Two cells a piece.
        assert_eq!(
            piece_cells(&map, 8),
            vec![
                MapCell::Done,
                MapCell::Done,
                MapCell::Cursor,
                MapCell::Cursor,
                MapCell::Empty,
                MapCell::Empty,
                MapCell::Done,
                MapCell::Done,
            ]
        );
    }

    #[test]
    fn piece_cells_share_a_cell_when_pieces_are_many() {
        // Ten pieces in three cells: 0-2, 3-5, 6-9.
        let map = PieceMap {
            total: 10,
            written: 3,
            waiting: vec![4],
            running: vec![lane(8)],
        };

        assert_eq!(
            piece_cells(&map, 3),
            vec![MapCell::Done, MapCell::Partial, MapCell::Cursor]
        );
        assert_eq!(
            piece_cells(&PieceMap::default(), 2),
            vec![MapCell::Empty; 2]
        );
    }
}
