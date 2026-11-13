//! Spotting a paste in a burst of key presses. Windows terminals send no paste
//! events: pasted text arrives as one key press per character, with Enter for
//! each line break, all at once.

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Characters that must arrive together before they count as a paste. Even
/// a fast typist's keys reach the reader a few at a time.
const PASTE_MIN_CHARS: usize = 8;

/// `batch` (events that were waiting together) with each burst of at least
/// `PASTE_MIN_CHARS` typed characters and line breaks turned into one paste.
pub fn coalesce_paste(batch: Vec<Event>) -> Vec<Event> {
    let mut out = Vec::with_capacity(batch.len());
    let mut run = Run::default();
    for event in batch {
        match typed(&event) {
            Typed::Char(c) => run.push(event, Some(c)),
            Typed::Release => run.push(event, None),
            Typed::No => {
                run.flush_into(&mut out);
                out.push(event);
            }
        }
    }
    run.flush_into(&mut out);
    out
}

/// Whether `event` could be part of a paste. Anything else (a shortcut, the
/// mouse, a resize) ends the batch being gathered, so it is never held back.
pub fn continues_burst(event: &Event) -> bool {
    !matches!(typed(event), Typed::No)
}

/// Ctrl+V or Shift+Insert, pressed. A terminal that pastes by itself never
/// passes these on; the old Windows console does, so rdm reads the clipboard.
pub fn is_paste_shortcut(event: &Event) -> bool {
    let Event::Key(KeyEvent {
        code,
        modifiers,
        kind,
        ..
    }) = event
    else {
        return false;
    };
    if *kind == KeyEventKind::Release {
        return false;
    }
    let held = *modifiers - KeyModifiers::SHIFT;
    match code {
        // Not Ctrl+Alt: Windows reports AltGr that way, and AltGr+V types a character.
        KeyCode::Char('v' | 'V') => held == KeyModifiers::CONTROL,
        KeyCode::Insert => *modifiers == KeyModifiers::SHIFT,
        _ => false,
    }
}

/// `event`, or a paste of the clipboard's text when `event` is the paste
/// shortcut and the clipboard holds some.
pub fn expand_paste_shortcut(event: Event, clipboard: impl FnOnce() -> Option<String>) -> Event {
    if !is_paste_shortcut(&event) {
        return event;
    }
    match clipboard() {
        Some(text) if !text.is_empty() => Event::Paste(text),
        _ => event,
    }
}

/// Typed keys seen in a row, kept as they came in case they aren't a paste.
#[derive(Default)]
struct Run {
    events: Vec<Event>,
    text: String,
    chars: usize,
}

impl Run {
    fn push(&mut self, event: Event, c: Option<char>) {
        self.events.push(event);
        if let Some(c) = c {
            self.text.push(c);
            self.chars += 1;
        }
    }

    fn flush_into(&mut self, out: &mut Vec<Event>) {
        let run = std::mem::take(self);
        if run.chars >= PASTE_MIN_CHARS {
            out.push(Event::Paste(run.text));
        } else {
            out.extend(run.events);
        }
    }
}

enum Typed {
    Char(char),
    /// Letting go of a typing key: part of a burst, but adds nothing.
    Release,
    No,
}

/// Whether `event` is a key that types text: a character (shifted, or with
/// AltGr, which Windows reports as Ctrl+Alt) or Enter.
fn typed(event: &Event) -> Typed {
    let Event::Key(KeyEvent {
        code,
        modifiers,
        kind,
        ..
    }) = event
    else {
        return Typed::No;
    };
    let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
    let plain = (*modifiers - KeyModifiers::SHIFT).is_empty()
        || (*modifiers - KeyModifiers::SHIFT) == altgr;
    let c = match code {
        KeyCode::Char(c) if plain => *c,
        KeyCode::Enter if modifiers.is_empty() => '\n',
        _ => return Typed::No,
    };
    match kind {
        KeyEventKind::Release => Typed::Release,
        _ => Typed::Char(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn release(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ))
    }

    /// Presses (and, like Windows, releases) for every character of `text`,
    /// with Enter for each line break.
    fn keys_for(text: &str) -> Vec<Event> {
        text.chars()
            .map(|c| match c {
                '\n' => KeyCode::Enter,
                c => KeyCode::Char(c),
            })
            .flat_map(|code| [press(code), release(code)])
            .collect()
    }

    #[test]
    fn a_burst_of_typed_keys_with_line_breaks_becomes_one_paste() {
        let batch = keys_for("https://a/1\nhttps://a/2\n");
        assert_eq!(
            coalesce_paste(batch),
            vec![Event::Paste("https://a/1\nhttps://a/2\n".into())]
        );
    }

    #[test]
    fn upper_case_and_altgr_characters_stay_part_of_the_paste() {
        let mut batch = keys_for("https://");
        batch.push(Event::Key(KeyEvent::new(
            KeyCode::Char('A'),
            KeyModifiers::SHIFT,
        )));
        // Windows reports AltGr as Ctrl+Alt.
        batch.push(Event::Key(KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        )));
        assert_eq!(
            coalesce_paste(batch),
            vec![Event::Paste("https://A@".into())]
        );
    }

    #[test]
    fn only_typed_keys_keep_a_burst_going() {
        assert!(continues_burst(&press(KeyCode::Char('a'))));
        assert!(continues_burst(&release(KeyCode::Char('a'))));
        assert!(continues_burst(&press(KeyCode::Enter)));
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!continues_burst(&ctrl_c), "Ctrl+C is handled at once");
        assert!(!continues_burst(&press(KeyCode::Esc)));
        assert!(!continues_burst(&Event::Resize(80, 24)));
        let moved = Event::Mouse(ratatui::crossterm::event::MouseEvent {
            kind: ratatui::crossterm::event::MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            !continues_burst(&moved),
            "mouse motion can't hold keys back"
        );
    }

    fn ctrl_v() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL))
    }

    #[test]
    fn ctrl_v_and_shift_insert_are_the_paste_shortcut() {
        assert!(is_paste_shortcut(&ctrl_v()));
        let upper = Event::Key(KeyEvent::new(
            KeyCode::Char('V'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert!(is_paste_shortcut(&upper));
        let shift_insert = Event::Key(KeyEvent::new(KeyCode::Insert, KeyModifiers::SHIFT));
        assert!(is_paste_shortcut(&shift_insert));

        assert!(
            !is_paste_shortcut(&press(KeyCode::Char('v'))),
            "plain v types v"
        );
        let let_go = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        ));
        assert!(!is_paste_shortcut(&let_go), "only the press pastes");
        // Windows reports AltGr as Ctrl+Alt: AltGr+V is a character, not paste.
        let altgr_v = Event::Key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert!(!is_paste_shortcut(&altgr_v));
    }

    #[test]
    fn the_paste_shortcut_becomes_a_paste_of_the_clipboard_text() {
        let links = "https://a/1\r\nhttps://a/2";
        assert_eq!(
            expand_paste_shortcut(ctrl_v(), || Some(links.to_string())),
            Event::Paste(links.into())
        );
    }

    #[test]
    fn with_no_text_on_the_clipboard_the_shortcut_stays_a_key() {
        assert_eq!(expand_paste_shortcut(ctrl_v(), || None), ctrl_v());
        assert_eq!(
            expand_paste_shortcut(ctrl_v(), || Some(String::new())),
            ctrl_v()
        );
    }

    #[test]
    fn other_events_never_read_the_clipboard() {
        let typed = press(KeyCode::Char('a'));
        let result = expand_paste_shortcut(typed.clone(), || panic!("read the clipboard"));
        assert_eq!(result, typed);
    }

    #[test]
    fn a_few_keys_at_once_are_still_typing() {
        let batch = keys_for("ab\n");
        assert_eq!(coalesce_paste(batch.clone()), batch);
    }

    #[test]
    fn shortcuts_and_other_events_around_a_paste_are_kept_in_order() {
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let mut batch = vec![press(KeyCode::Tab)];
        batch.extend(keys_for("https://a/1"));
        batch.push(ctrl_c.clone());
        batch.push(Event::Resize(80, 24));
        assert_eq!(
            coalesce_paste(batch),
            vec![
                press(KeyCode::Tab),
                Event::Paste("https://a/1".into()),
                ctrl_c,
                Event::Resize(80, 24),
            ]
        );
    }
}
