#![deny(clippy::print_stdout, clippy::print_stderr)]

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod engine;
pub mod error;
pub mod probe;
pub mod ranges;
pub mod retry;
pub mod template;
#[cfg(feature = "testing")]
pub mod testing;
pub mod worker;
pub mod writer;
