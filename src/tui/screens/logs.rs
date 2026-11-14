//! The Logs screen: the end of rdmd's log file, read straight from disk,
//! and below it the notices the footer has shown.

use crate::tui::action::Action;
use crate::tui::state::{App, Notice};
use crate::tui::theme;
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// Only this much of the end of the log is read, however large it grows.
pub const LOG_TAIL_BYTES: u64 = 64 * 1024;
/// Most notices shown under the log.
const NOTICE_ROWS: usize = 6;
/// Lines the mouse wheel scrolls.
const WHEEL_LINES: usize = 3;

/// What the Logs screen shows. `scroll` counts lines up from the newest.
#[derive(Clone, Debug, Default)]
pub struct LogsState {
    pub lines: Vec<String>,
    pub scroll: usize,
    pub error: Option<String>,
    /// Log lines the last frame had room for: one page.
    pub page: usize,
}

/// The last `max_bytes` of the file at `path` as lines, without the first
/// line when the cut fell inside it. Bytes that aren't UTF-8 are replaced.
pub fn read_tail(path: &Path, max_bytes: u64) -> io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut bytes = Vec::new();
    if len > max_bytes {
        // Read one byte before the cut, to see whether a line starts at it.
        file.seek(SeekFrom::Start(len - max_bytes - 1))?;
        file.take(max_bytes + 1).read_to_end(&mut bytes)?;
        let first_break = bytes
            .iter()
            .position(|b| *b == b'\n')
            .unwrap_or(bytes.len());
        bytes.drain(..(first_break + 1).min(bytes.len()));
    } else {
        file.read_to_end(&mut bytes)?;
    }
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::to_string)
        .collect())
}

/// Read the log again; called on opening the screen and on every refresh while it shows.
pub fn reload(app: &mut App) {
    let Some(path) = app.log_path.clone() else {
        app.logs.lines.clear();
        app.logs.error = Some("This rdmd doesn't report where its log is".into());
        return;
    };
    match read_tail(&path, LOG_TAIL_BYTES) {
        Ok(lines) => {
            // Keep the same lines in view while new ones arrive below.
            if app.logs.scroll > 0 {
                app.logs.scroll += lines.len().saturating_sub(app.logs.lines.len());
            }
            app.logs.lines = lines;
            app.logs.error = None;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            app.logs.lines.clear();
            app.logs.error = Some(format!("No log yet at {}", path.display()));
        }
        Err(e) => {
            app.logs.lines.clear();
            app.logs.error = Some(format!("Can't read {}: {e}", path.display()));
        }
    }
    app.logs.scroll = app.logs.scroll.min(max_scroll(&app.logs));
}

fn max_scroll(logs: &LogsState) -> usize {
    logs.lines.len().saturating_sub(logs.page.max(1))
}

pub fn handle_event(app: &mut App, event: &Event) -> Option<Action> {
    let page = app.logs.page.max(1);
    let up = |logs: &mut LogsState, n: usize| logs.scroll = (logs.scroll + n).min(max_scroll(logs));
    let logs = &mut app.logs;
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Up | KeyCode::Char('k') => up(logs, 1),
            KeyCode::Down | KeyCode::Char('j') => logs.scroll = logs.scroll.saturating_sub(1),
            KeyCode::PageUp => up(logs, page),
            KeyCode::PageDown => logs.scroll = logs.scroll.saturating_sub(page),
            KeyCode::Char('G') | KeyCode::End => logs.scroll = 0,
            KeyCode::Home => logs.scroll = max_scroll(logs),
            _ => {}
        },
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => up(logs, WHEEL_LINES),
            MouseEventKind::ScrollDown => logs.scroll = logs.scroll.saturating_sub(WHEEL_LINES),
            _ => {}
        },
        _ => {}
    }
    None
}

pub fn render(frame: &mut Frame, app: &mut App, area: Rect) {
    let notice_rows = app.history.len().clamp(1, NOTICE_ROWS) as u16;
    let [log_area, notice_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(notice_rows + 2)]).areas(area);
    render_log(frame, app, log_area);
    render_notices(frame, app, notice_area);
}

fn render_log(frame: &mut Frame, app: &mut App, area: Rect) {
    let mut title = match &app.log_path {
        Some(path) => format!(" Log: {} ", path.display()),
        None => " Log ".to_string(),
    };
    if app.logs.scroll > 0 {
        title.push_str(&format!("· {} lines up · G newest ", app.logs.scroll));
    }
    let block = Block::new().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.logs.page = inner.height as usize;
    app.logs.scroll = app.logs.scroll.min(max_scroll(&app.logs));

    if let Some(err) = &app.logs.error {
        frame.render_widget(Paragraph::new(err.as_str()).style(theme::MUTED), inner);
        return;
    }
    let end = app.logs.lines.len() - app.logs.scroll;
    let start = end.saturating_sub(inner.height as usize);
    let lines: Vec<Line> = app.logs.lines[start..end]
        .iter()
        .map(|l| Line::raw(l.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_notices(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::new()
        .borders(Borders::ALL)
        .title(" Notices ")
        .border_style(theme::FRAME);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.history.is_empty() {
        let none = Paragraph::new("Nothing yet").style(theme::DIM);
        frame.render_widget(none, inner);
        return;
    }
    let skip = app.history.len().saturating_sub(inner.height as usize);
    let lines: Vec<Line> = app.history.iter().skip(skip).map(notice_line).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn notice_line(notice: &Notice) -> Line<'_> {
    let style = if notice.is_error {
        theme::ERROR
    } else {
        theme::MUTED
    };
    Line::styled(notice.text.as_str(), style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::screens;
    use crate::tui::state::test_support::screen_text;
    use crate::tui::state::{Screen, NOTICE_HISTORY};
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use std::io::Write;
    use std::path::Path;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| screens::render(f, app)).unwrap();
        screen_text(&terminal)
    }

    fn logs_app(path: &Path) -> App {
        App {
            connected: true,
            screen: Screen::Logs,
            log_path: Some(path.to_path_buf()),
            ..App::default()
        }
    }

    fn numbered_log(path: &Path, lines: usize) {
        let mut file = std::fs::File::create(path).unwrap();
        for i in 1..=lines {
            writeln!(file, "line {i:07} of the daemon log").unwrap();
        }
    }

    #[test]
    fn read_tail_returns_whole_small_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdmd.log");
        std::fs::write(&path, "first\r\nsecond\nthird\n").unwrap();
        assert_eq!(
            read_tail(&path, LOG_TAIL_BYTES).unwrap(),
            vec!["first", "second", "third"]
        );
    }

    #[test]
    fn only_the_last_64_kib_of_a_large_log_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdmd.log");
        numbered_log(&path, 160_000);
        assert!(std::fs::metadata(&path).unwrap().len() > 4_000_000);

        let lines = read_tail(&path, LOG_TAIL_BYTES).unwrap();
        let bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
        assert!(bytes as u64 <= LOG_TAIL_BYTES, "{bytes} bytes read");
        assert_eq!(
            lines.last().map(String::as_str),
            Some("line 0160000 of the daemon log")
        );
        assert!(
            lines[0].starts_with("line ") && lines[0].ends_with(" of the daemon log"),
            "starts mid-line: {:?}",
            lines[0]
        );
    }

    #[test]
    fn a_tail_that_starts_on_a_line_boundary_keeps_that_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdmd.log");
        std::fs::write(&path, "aaaa\nbbbb\ncccc\n").unwrap();
        assert_eq!(read_tail(&path, 10).unwrap(), vec!["bbbb", "cccc"]);
    }

    #[test]
    fn invalid_utf8_is_shown_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdmd.log");
        std::fs::write(&path, b"ok\nbad \xff\xfe byte\n").unwrap();
        let lines = read_tail(&path, LOG_TAIL_BYTES).unwrap();
        assert_eq!(lines[0], "ok");
        assert!(lines[1].starts_with("bad ") && lines[1].ends_with(" byte"));
    }

    #[test]
    fn a_missing_log_says_where_it_will_be() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdmd.log");
        let mut app = logs_app(&path);
        reload(&mut app);
        let want = format!("No log yet at {}", path.display());
        assert_eq!(app.logs.error.as_deref(), Some(want.as_str()));
        assert!(draw(&mut app, 200, 30).contains(&want));
    }

    #[test]
    fn an_rdmd_without_a_log_path_says_so() {
        let mut app = App {
            screen: Screen::Logs,
            ..App::default()
        };
        reload(&mut app);
        assert!(draw(&mut app, 100, 30).contains("This rdmd doesn't report where its log is"));
    }

    #[test]
    fn the_screen_shows_the_newest_log_lines_and_the_notices() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdmd.log");
        numbered_log(&path, 100);
        let mut app = logs_app(&path);
        app.notify("Added dl_1 (queued)", false);
        app.notify("rdmd is not running", true);
        reload(&mut app);
        let text = draw(&mut app, 100, 30);
        assert!(text.contains("line 0000100 of the daemon log"), "{text}");
        assert!(
            !text.contains("line 0000001 "),
            "only the newest fit:\n{text}"
        );
        let added = text.find("Added dl_1 (queued)").expect("first notice");
        let failed = text.find("rdmd is not running").expect("second notice");
        assert!(added < failed, "newest notice last:\n{text}");
    }

    #[test]
    fn g_jumps_back_to_the_newest_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdmd.log");
        numbered_log(&path, 100);
        let mut app = logs_app(&path);
        reload(&mut app);
        draw(&mut app, 100, 30);

        screens::handle_event(&mut app, &key(KeyCode::Up));
        assert_eq!(app.logs.scroll, 1);
        screens::handle_event(&mut app, &key(KeyCode::PageUp));
        assert!(app.logs.scroll > 1, "PgUp scrolls a page");
        let text = draw(&mut app, 100, 30);
        assert!(!text.contains("line 0000100 "), "{text}");

        screens::handle_event(&mut app, &key(KeyCode::Char('G')));
        assert_eq!(app.logs.scroll, 0);
        assert!(draw(&mut app, 100, 30).contains("line 0000100 "));
    }

    #[test]
    fn scrolling_stops_at_the_oldest_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdmd.log");
        numbered_log(&path, 40);
        let mut app = logs_app(&path);
        reload(&mut app);
        draw(&mut app, 100, 30);
        for _ in 0..10 {
            screens::handle_event(&mut app, &key(KeyCode::PageUp));
        }
        assert!(draw(&mut app, 100, 30).contains("line 0000001 "));
        screens::handle_event(&mut app, &key(KeyCode::Down));
        assert!(app.logs.scroll < 40, "Down works right after the top");
    }

    #[test]
    fn notices_are_kept_in_history_up_to_the_cap() {
        let mut app = App::default();
        for i in 0..NOTICE_HISTORY + 5 {
            app.notify(format!("notice {i}"), false);
        }
        assert_eq!(app.history.len(), NOTICE_HISTORY);
        assert_eq!(app.history.front().unwrap().text, "notice 5");
        assert_eq!(
            app.history.back().unwrap().text,
            format!("notice {}", NOTICE_HISTORY + 4)
        );
    }
}
