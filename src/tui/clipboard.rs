//! Reading text from the Windows clipboard, for terminals (the old console)
//! that hand Ctrl+V to the program instead of pasting.

use std::ffi::c_void;
use std::time::Duration;

/// Text as UTF-16, ending in a zero.
const CF_UNICODETEXT: u32 = 13;
/// Another program can hold the clipboard for a moment, right after a copy.
const OPEN_ATTEMPTS: u32 = 5;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(10);

#[link(name = "user32")]
extern "system" {
    fn IsClipboardFormatAvailable(format: u32) -> i32;
    fn OpenClipboard(owner: *mut c_void) -> i32;
    fn GetClipboardData(format: u32) -> *mut c_void;
    fn CloseClipboard() -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GlobalLock(memory: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(memory: *mut c_void) -> i32;
    fn GlobalSize(memory: *mut c_void) -> usize;
}

/// The clipboard's text, or `None` when it holds no text or stays busy.
pub fn read_text() -> Option<String> {
    // SAFETY: no pointers are passed; the result is a plain flag.
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
        return None;
    }
    // SAFETY: a null owner ties the clipboard to this thread until it is closed.
    let opened = (0..OPEN_ATTEMPTS).any(|attempt| {
        if attempt > 0 {
            std::thread::sleep(OPEN_RETRY_DELAY);
        }
        let opened = unsafe { OpenClipboard(std::ptr::null_mut()) };
        opened != 0
    });
    if !opened {
        return None;
    }
    let text = text_while_open();
    // SAFETY: this thread opened the clipboard above.
    unsafe { CloseClipboard() };
    text
}

/// Copy the clipboard's text out. Only called while this thread has the
/// clipboard open, so the data can't change or go away underneath.
fn text_while_open() -> Option<String> {
    // SAFETY: the clipboard is open; the handle it returns stays valid until
    // it is closed, and it is only read between GlobalLock and GlobalUnlock.
    // The read is bounded by the block's own size, so text without its
    // closing zero can't be read past the end.
    unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT);
        if handle.is_null() {
            return None;
        }
        let data = GlobalLock(handle) as *const u16;
        if data.is_null() {
            return None;
        }
        let units = std::slice::from_raw_parts(data, GlobalSize(handle) / 2);
        let len = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        let text = String::from_utf16_lossy(&units[..len]);
        GlobalUnlock(handle);
        Some(text)
    }
}
