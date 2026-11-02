use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("download cancelled")]
    Cancelled,
    #[error("link expired or removed (status {status})")]
    LinkExpired { status: u16 },
    #[error("server file changed between sessions")]
    FileChanged,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("state error: {0}")]
    State(String),
    #[error("insufficient disk space on {target}: required {required} bytes, available {available} bytes")]
    InsufficientDiskSpace {
        target: String,
        required: u64,
        available: u64,
    },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
