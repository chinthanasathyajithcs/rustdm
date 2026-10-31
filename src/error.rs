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
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
