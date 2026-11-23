//! A stand-in for yt-dlp and deno in tests: plays the answers a test scripted
//! for it (see `rustdm::testing::video`). Built only with `--features testing`.
//!
//! It includes `tool_program.rs` directly instead of linking the rustdm
//! library, so the program stays small: every video test installs copies of it.

#[path = "../testing/tool_program.rs"]
#[allow(dead_code)]
mod tool_program;

fn main() -> std::process::ExitCode {
    let code = tool_program::run_test_tool();
    std::process::ExitCode::from(code.clamp(0, 255) as u8)
}
