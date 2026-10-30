#![deny(clippy::print_stdout, clippy::print_stderr)]

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod ranges;
