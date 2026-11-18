//! Whether `rdm` owns its console window, which is the case when it was opened
//! by double-clicking it or a shortcut rather than typed into a terminal. Then
//! the window closes the moment rdm exits, so a startup error would vanish
//! before it can be read; `main` waits for Enter instead.

use std::io::{self, BufRead, Write};

/// Only the program that created a console is attached to it when it was
/// double-clicked; a shell that ran rdm is attached too. Zero means no console.
pub fn owns_window(processes_on_console: u32) -> bool {
    processes_on_console == 1
}

/// True when rdm was started on its own console window (Windows only).
pub fn started_on_its_own_window() -> bool {
    owns_window(processes_on_console())
}

#[cfg(windows)]
fn processes_on_console() -> u32 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    }
    let mut ids = [0u32; 4];
    // SAFETY: `ids` holds `ids.len()` entries; the call writes at most that many
    // and returns the total count (0 when there is no console).
    unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) }
}

#[cfg(not(windows))]
fn processes_on_console() -> u32 {
    0
}

/// Ask for Enter and wait for it (or for the input to end).
pub fn wait_for_enter(input: &mut impl BufRead, output: &mut impl Write) {
    let _ = write!(output, "Press Enter to close this window.");
    let _ = output.flush();
    let _ = input.read_line(&mut String::new());
}

/// `wait_for_enter` on the real console.
pub fn pause_before_closing() {
    wait_for_enter(&mut io::stdin().lock(), &mut io::stderr());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_console_rdm_has_to_itself_is_its_own_window() {
        assert!(owns_window(1));
        assert!(!owns_window(2), "a terminal shell shares the console");
        assert!(!owns_window(0), "no console at all");
    }

    #[test]
    fn waiting_asks_for_enter_and_returns_when_it_is_pressed() {
        let mut input = io::Cursor::new(b"\n".to_vec());
        let mut output = Vec::new();
        wait_for_enter(&mut input, &mut output);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Press Enter to close this window."
        );
        assert_eq!(input.position(), 1);
    }

    #[test]
    fn waiting_ends_when_the_input_is_closed() {
        let mut output = Vec::new();
        wait_for_enter(&mut io::Cursor::new(Vec::new()), &mut output);
        assert!(!output.is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn other_systems_never_wait() {
        assert!(!started_on_its_own_window());
    }
}
