//! A one-line text field: cursor movement, editing and paste, measured in
//! characters so any text is safe to edit.

use crate::tui::theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextField {
    text: String,
    /// Position of the cursor, in characters from the start.
    cursor: usize,
}

impl TextField {
    /// A field holding `text`, with the cursor at the end.
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            cursor: text.chars().count(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Edit the text for `key`. Returns false for keys a field doesn't use,
    /// such as Enter, Tab and Esc, so the dialog holding it can act on them.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        // Windows reports AltGr as Ctrl+Alt; those keys are characters, not shortcuts.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
        let len = self.text.chars().count();
        match key.code {
            KeyCode::Char('u') if ctrl => {
                self.text.clear();
                self.cursor = 0;
            }
            KeyCode::Char(_) if ctrl => return false,
            KeyCode::Char(c) => self.insert(&c.to_string()),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.remove_at(self.cursor);
            }
            KeyCode::Delete if self.cursor < len => self.remove_at(self.cursor),
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    /// Insert pasted text at the cursor, without line breaks or other control characters.
    pub fn paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        self.insert(&clean);
    }

    /// Draw the text on `area`'s first row, scrolled so the cursor shows, and
    /// place the terminal cursor there when `focused`.
    pub fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let chars: Vec<char> = self.text.chars().collect();
        // Keep one cell free for the cursor after the last character.
        let room = area.width as usize - 1;
        let mut start = self.cursor;
        while start > 0 && width_of(&chars[start - 1..self.cursor]) <= room {
            start -= 1;
        }
        let mut end = start;
        while end < chars.len() && width_of(&chars[start..=end]) <= area.width as usize {
            end += 1;
        }
        let shown: String = chars[start..end].iter().collect();
        let style = if focused {
            theme::FIELD_FOCUSED
        } else {
            theme::FIELD
        };
        let row = Rect::new(area.x, area.y, area.width, 1);
        frame.render_widget(Paragraph::new(shown).style(style), row);
        if focused {
            let x = area.x + width_of(&chars[start..self.cursor]) as u16;
            frame.set_cursor_position(Position::new(x.min(area.right() - 1), area.y));
        }
    }

    fn byte_index(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(i, _)| i)
    }

    fn insert(&mut self, text: &str) {
        let at = self.byte_index(self.cursor);
        self.text.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    fn remove_at(&mut self, chars: usize) {
        let at = self.byte_index(chars);
        self.text.remove(at);
    }
}

/// Terminal columns `chars` take up; wide characters such as 日 take two.
fn width_of(chars: &[char]) -> usize {
    Span::raw(chars.iter().collect::<String>()).width()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    #[test]
    fn altgr_characters_are_typed() {
        // Windows reports AltGr as Ctrl+Alt, which is how `\`, `@` and `~` are typed on many layouts.
        let mut field = TextField::new("C:");
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        for c in ['\\', '@', '~'] {
            assert!(
                field.handle_key(&KeyEvent::new(KeyCode::Char(c), altgr)),
                "{c}"
            );
        }
        assert_eq!(field.text(), "C:\\@~");
        assert!(!field.handle_key(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    fn press(field: &mut TextField, code: KeyCode) -> bool {
        field.handle_key(&KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(field: &mut TextField, text: &str) {
        for c in text.chars() {
            press(field, KeyCode::Char(c));
        }
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut field = TextField::new("ac");
        press(&mut field, KeyCode::Left);
        type_text(&mut field, "b");
        assert_eq!(field.text(), "abc");
        type_text(&mut field, "X");
        assert_eq!(field.text(), "abXc", "shifted letters type too");
    }

    #[test]
    fn home_end_and_arrows_move_the_cursor() {
        let mut field = TextField::new("abc");
        press(&mut field, KeyCode::Home);
        type_text(&mut field, "x");
        press(&mut field, KeyCode::End);
        type_text(&mut field, "y");
        assert_eq!(field.text(), "xabcy");
        press(&mut field, KeyCode::Left);
        press(&mut field, KeyCode::Left);
        press(&mut field, KeyCode::Right);
        type_text(&mut field, "z");
        assert_eq!(field.text(), "xabczy");
        for _ in 0..10 {
            press(&mut field, KeyCode::Right);
        }
        type_text(&mut field, "!");
        assert_eq!(field.text(), "xabczy!", "the cursor stops at the end");
    }

    #[test]
    fn backspace_and_delete_remove_around_the_cursor() {
        let mut field = TextField::new("abcd");
        press(&mut field, KeyCode::Left);
        press(&mut field, KeyCode::Left);
        press(&mut field, KeyCode::Backspace);
        assert_eq!(field.text(), "acd");
        press(&mut field, KeyCode::Delete);
        assert_eq!(field.text(), "ad");
        press(&mut field, KeyCode::Home);
        press(&mut field, KeyCode::Backspace);
        press(&mut field, KeyCode::End);
        press(&mut field, KeyCode::Delete);
        assert_eq!(field.text(), "ad", "nothing to remove at the ends");
    }

    #[test]
    fn ctrl_u_clears() {
        let mut field = TextField::new("https://example.com/a");
        let used = field.handle_key(&KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(used);
        assert_eq!(field.text(), "");
        type_text(&mut field, "x");
        assert_eq!(field.text(), "x");
    }

    #[test]
    fn paste_inserts_at_the_cursor() {
        let mut field = TextField::new("ad");
        press(&mut field, KeyCode::Left);
        field.paste("b\r\nc");
        assert_eq!(field.text(), "abcd", "line breaks are dropped");
        type_text(&mut field, "!");
        assert_eq!(field.text(), "abc!d");
    }

    #[test]
    fn cursor_moves_by_character_in_non_ascii_text() {
        let mut field = TextField::new("日本é🎉");
        press(&mut field, KeyCode::End);
        press(&mut field, KeyCode::Backspace);
        assert_eq!(field.text(), "日本é");
        press(&mut field, KeyCode::Home);
        press(&mut field, KeyCode::Delete);
        assert_eq!(field.text(), "本é");
        press(&mut field, KeyCode::Right);
        type_text(&mut field, "🎉");
        assert_eq!(field.text(), "本🎉é");
    }

    #[test]
    fn keys_a_dialog_needs_are_left_alone() {
        let mut field = TextField::new("abc");
        for code in [KeyCode::Enter, KeyCode::Tab, KeyCode::BackTab, KeyCode::Esc] {
            assert!(!press(&mut field, code), "{code:?}");
        }
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!field.handle_key(&ctrl_c));
        assert_eq!(field.text(), "abc");
    }

    fn rendered(field: &TextField, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|f| field.render(f, Rect::new(0, 0, width, 1), true))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect()
    }

    #[test]
    fn long_text_scrolls_to_keep_the_cursor_visible() {
        let mut field = TextField::new("abcdefghijklmnopqrstuvwxyz");
        let text = rendered(&field, 10);
        assert!(text.contains("xyz"), "{text:?}");
        assert!(!text.contains('a'), "{text:?}");

        press(&mut field, KeyCode::Home);
        let text = rendered(&field, 10);
        assert!(text.starts_with("abcdefghij"), "{text:?}");
    }

    #[test]
    fn wide_characters_scroll_without_panicking() {
        let field = TextField::new(&"日本".repeat(20));
        for width in [1, 2, 3, 10] {
            rendered(&field, width);
        }
    }
}
