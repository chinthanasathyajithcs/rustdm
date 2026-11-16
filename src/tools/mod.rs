//! Fetches, verifies, updates and rolls back the helper programs rustdm needs
//! for video: yt-dlp, ffmpeg and deno. Nothing happens until something asks
//! for a tool.

pub mod checksum;
pub mod error;
pub mod openpgp;
pub mod version;

pub use error::ToolError;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tool {
    YtDlp,
    Ffmpeg,
    Deno,
}

impl Tool {
    pub const ALL: [Tool; 3] = [Tool::YtDlp, Tool::Ffmpeg, Tool::Deno];

    /// The name people know it by; also its folder name under `tools/`.
    pub fn name(self) -> &'static str {
        match self {
            Tool::YtDlp => "yt-dlp",
            Tool::Ffmpeg => "ffmpeg",
            Tool::Deno => "deno",
        }
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_serialize_in_kebab_case() {
        assert_eq!(serde_json::to_string(&Tool::YtDlp).unwrap(), "\"yt-dlp\"");
        assert_eq!(serde_json::to_string(&Tool::Deno).unwrap(), "\"deno\"");
        assert_eq!(Tool::Ffmpeg.to_string(), "ffmpeg");
    }
}
