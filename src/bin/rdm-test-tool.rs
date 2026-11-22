//! A stand-in for yt-dlp and deno in tests: plays the answers a test scripted
//! for it (see `rustdm::testing::video`). Built only with `--features testing`.

fn main() -> std::process::ExitCode {
    let code = rustdm::testing::video::run_test_tool();
    std::process::ExitCode::from(code as u8)
}
