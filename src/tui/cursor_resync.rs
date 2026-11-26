//! Keep every cell in its column when the terminal draws some text wider than
//! ratatui measured it.
//!
//! ratatui gives a Sinhala letter with a vowel sign (such as "පි") one column,
//! but the Windows console draws it two columns wide. The backend only moves
//! the cursor when a cell is not right after the last one it drew, so the rest
//! of the row lands too far right, and later updates, which do move the
//! cursor, write over that shifted text and leave mixed-up digits behind.
//! This wrapper moves the cursor again after every non-ASCII cell, so a
//! wrongly measured character can only spill into the next cell, which is
//! then drawn over at its own column.

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// Wraps a backend and starts a new draw run after each non-ASCII cell.
pub struct CursorResync<B> {
    inner: B,
}

impl<B> CursorResync<B> {
    pub fn new(inner: B) -> Self {
        Self { inner }
    }
}

impl<B: Backend> Backend for CursorResync<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut run: Vec<(u16, u16, &'a Cell)> = Vec::new();
        for item in content {
            let ends_run = !item.2.symbol().is_ascii();
            run.push(item);
            if ends_run {
                // A new draw call starts with a cursor move to its first cell.
                self.inner.draw(std::mem::take(&mut run).into_iter())?;
            }
        }
        if run.is_empty() {
            return Ok(());
        }
        self.inner.draw(run.into_iter())
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::CrosstermBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    /// The start of the YouTube title that showed the bug.
    const SINHALA_NAME: &str = "\u{1F534}\u{0DB4}\u{0DD2}\u{0DA7} \u{0DB4}\u{0DD2}\u{0DA7} robbery";

    /// The bytes a full draw of `text` on one row sends to the terminal.
    fn drawn_bytes(text: &str) -> String {
        let mut screen = Buffer::empty(Rect::new(0, 0, 30, 1));
        screen.set_string(0, 0, text, Style::default());
        // The old screen differs in every cell, so the whole row is redrawn.
        let mut old = Buffer::empty(screen.area);
        old.set_string(0, 0, "x".repeat(30), Style::default());
        let mut bytes = Vec::new();
        let mut backend = CursorResync::new(CrosstermBackend::new(&mut bytes));
        backend.draw(old.diff(&screen).into_iter()).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    /// Crossterm's cursor move to 0-based column `x` on the first row.
    fn move_to(x: u16) -> String {
        format!("\x1b[1;{}H", x + 1)
    }

    #[test]
    fn moves_the_cursor_after_a_letter_with_a_vowel_sign() {
        let bytes = drawn_bytes(SINHALA_NAME);
        // "පි" is cell 2, so "ට" is drawn after a move to its own cell 3.
        let letter = format!("{}\u{0DA7}", move_to(3));
        assert!(bytes.contains(&letter), "no move after පි in {bytes:?}");
    }

    #[test]
    fn text_after_the_name_starts_at_its_own_column() {
        let bytes = drawn_bytes(SINHALA_NAME);
        // The space after the second "ට" (cell 6) is cell 7, whatever width
        // the console gave the letters.
        let word = format!("{} robbery", move_to(7));
        assert!(bytes.contains(&word), "no move before robbery in {bytes:?}");
    }

    #[test]
    fn plain_text_still_goes_out_in_one_run() {
        let bytes = drawn_bytes("Rust_in_100_Seconds.mp4");
        assert_eq!(
            bytes.matches("\x1b[1;").count(),
            1,
            "extra cursor moves in {bytes:?}"
        );
    }
}
