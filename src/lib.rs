#![deny(clippy::print_stdout, clippy::print_stderr)]

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod cancel;
pub mod daemon;
pub mod dirs;
pub mod disk;
pub mod engine;
pub mod error;
pub mod ipc;
pub mod limiter;
pub mod probe;
pub mod ranges;
pub mod retry;
pub mod scheduler;
pub mod state;
pub mod template;
#[cfg(feature = "testing")]
pub mod testing;
pub mod tui;
pub mod units;
pub mod worker;
pub mod writer;
