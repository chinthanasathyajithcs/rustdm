use crate::ipc::protocol::{
    decode_message, encode_message, parse_rpc_message, RpcError, RpcMessage,
};
use serde::Serialize;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{
    split, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf, ReadHalf,
    WriteHalf,
};

#[derive(Debug, thiserror::Error)]
pub enum IpcTransportError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Protocol error: {0}")]
    Protocol(#[from] RpcError),
    #[error("Address already in use: {0}")]
    AddressInUse(String),
    #[error("Connection closed")]
    ConnectionClosed,
    #[error("Connection timed out")]
    Timeout,
}

/// IPC Endpoint specification for Unix sockets or Windows Named Pipes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcEndpoint {
    UnixSocket(PathBuf),
    NamedPipe(String),
}

impl IpcEndpoint {
    /// Return the standard platform-specific default endpoint.
    pub fn default_endpoint() -> Self {
        #[cfg(unix)]
        {
            if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
                Self::UnixSocket(PathBuf::from(runtime_dir).join("rustdm.sock"))
            } else {
                Self::UnixSocket(PathBuf::from("/tmp/rustdm.sock"))
            }
        }
        #[cfg(windows)]
        {
            Self::NamedPipe(r"\\.\pipe\rustdm".to_string())
        }
    }

    /// Return an isolated platform-specific endpoint suitable for parallel test runs.
    pub fn test_endpoint(suffix: &str) -> Self {
        #[cfg(unix)]
        {
            let tmp = std::env::temp_dir();
            Self::UnixSocket(tmp.join(format!(
                "rustdm_test_{}_{}_{}.sock",
                std::process::id(),
                fastrand::u32(..),
                suffix
            )))
        }
        #[cfg(windows)]
        {
            Self::NamedPipe(format!(
                r"\\.\pipe\rustdm_test_{}_{}_{}",
                std::process::id(),
                fastrand::u32(..),
                suffix
            ))
        }
    }

    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::UnixSocket(p) => Some(p),
            Self::NamedPipe(_) => None,
        }
    }

    pub fn as_pipe_name(&self) -> Option<&str> {
        match self {
            Self::UnixSocket(_) => None,
            Self::NamedPipe(s) => Some(s),
        }
    }
}

/// Internal wrapper abstracting over platform-specific async stream types.
#[derive(Debug)]
pub enum InnerStream {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    NamedPipeServer(tokio::net::windows::named_pipe::NamedPipeServer),
    #[cfg(windows)]
    NamedPipeClient(tokio::net::windows::named_pipe::NamedPipeClient),
}

impl AsyncRead for InnerStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            #[cfg(unix)]
            InnerStream::Unix(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            InnerStream::NamedPipeServer(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            InnerStream::NamedPipeClient(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for InnerStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            #[cfg(unix)]
            InnerStream::Unix(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            InnerStream::NamedPipeServer(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            InnerStream::NamedPipeClient(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            #[cfg(unix)]
            InnerStream::Unix(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            InnerStream::NamedPipeServer(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            InnerStream::NamedPipeClient(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            #[cfg(unix)]
            InnerStream::Unix(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            InnerStream::NamedPipeServer(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            InnerStream::NamedPipeClient(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Write-half of a split IPC connection.
pub struct IpcSender {
    writer: WriteHalf<InnerStream>,
}

impl IpcSender {
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), IpcTransportError> {
        let wire = encode_message(msg)?;
        self.writer.write_all(wire.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

/// Read-half of a split IPC connection.
pub struct IpcReceiver {
    reader: BufReader<ReadHalf<InnerStream>>,
}

impl IpcReceiver {
    pub async fn recv(&mut self) -> Result<Option<RpcMessage>, IpcTransportError> {
        let mut line = String::new();
        let bytes_read = self.reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            return Ok(None);
        }
        let msg = parse_rpc_message(&line)?;
        Ok(Some(msg))
    }

    pub async fn recv_raw_line(&mut self) -> Result<Option<String>, IpcTransportError> {
        let mut line = String::new();
        let bytes_read = self.reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            return Ok(None);
        }
        Ok(Some(line))
    }

    pub async fn recv_typed<T: serde::de::DeserializeOwned>(
        &mut self,
    ) -> Result<Option<T>, IpcTransportError> {
        let mut line = String::new();
        let bytes_read = self.reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            return Ok(None);
        }
        let val = decode_message(&line)?;
        Ok(Some(val))
    }
}

/// Bidirectional IPC Connection for sending and receiving messages.
pub struct IpcConnection {
    sender: IpcSender,
    receiver: IpcReceiver,
}

impl IpcConnection {
    pub fn from_stream(stream: InnerStream) -> Self {
        let (reader, writer) = split(stream);
        Self {
            sender: IpcSender { writer },
            receiver: IpcReceiver {
                reader: BufReader::new(reader),
            },
        }
    }

    /// Connect to a running IPC endpoint.
    pub async fn connect(endpoint: &IpcEndpoint) -> Result<Self, IpcTransportError> {
        match endpoint {
            #[cfg(unix)]
            IpcEndpoint::UnixSocket(path) => {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok(Self::from_stream(InnerStream::Unix(stream)))
            }
            #[cfg(not(unix))]
            IpcEndpoint::UnixSocket(_) => Err(IpcTransportError::Io(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unix domain sockets are not supported on this platform",
            ))),
            #[cfg(windows)]
            IpcEndpoint::NamedPipe(pipe_name) => {
                let start = std::time::Instant::now();
                let client = loop {
                    match tokio::net::windows::named_pipe::ClientOptions::new().open(pipe_name) {
                        Ok(client) => break client,
                        Err(e) if e.raw_os_error() == Some(231) => {
                            // ERROR_PIPE_BUSY (Win32 231): retry while server accepts and creates next instance
                            if start.elapsed() > std::time::Duration::from_secs(5) {
                                return Err(IpcTransportError::Io(e));
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                        Err(e) => return Err(IpcTransportError::Io(e)),
                    }
                };
                Ok(Self::from_stream(InnerStream::NamedPipeClient(client)))
            }
            #[cfg(not(windows))]
            IpcEndpoint::NamedPipe(_) => Err(IpcTransportError::Io(io::Error::new(
                io::ErrorKind::Unsupported,
                "Named pipes are not supported on this platform",
            ))),
        }
    }

    /// Split the connection into independent sender and receiver halves.
    pub fn into_split(self) -> (IpcSender, IpcReceiver) {
        (self.sender, self.receiver)
    }

    /// Send a typed JSON-RPC message.
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), IpcTransportError> {
        self.sender.send(msg).await
    }

    /// Receive the next JSON-RPC message. Returns Ok(None) on EOF.
    pub async fn recv(&mut self) -> Result<Option<RpcMessage>, IpcTransportError> {
        self.receiver.recv().await
    }

    /// Receive raw line string. Returns Ok(None) on EOF.
    pub async fn recv_raw_line(&mut self) -> Result<Option<String>, IpcTransportError> {
        self.receiver.recv_raw_line().await
    }

    /// Receive directly into a deserializable type.
    pub async fn recv_typed<T: serde::de::DeserializeOwned>(
        &mut self,
    ) -> Result<Option<T>, IpcTransportError> {
        self.receiver.recv_typed().await
    }
}

/// Cross-platform IPC Server Listener.
pub struct IpcListener {
    #[cfg(unix)]
    listener: tokio::net::UnixListener,
    #[cfg(unix)]
    socket_path: Option<PathBuf>,

    #[cfg(windows)]
    pipe_name: String,
    #[cfg(windows)]
    server: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
}

impl IpcListener {
    /// Bind an IPC listener to the specified endpoint.
    pub async fn bind(endpoint: &IpcEndpoint) -> Result<Self, IpcTransportError> {
        match endpoint {
            #[cfg(unix)]
            IpcEndpoint::UnixSocket(path) => {
                if path.exists() {
                    // Probe if an existing daemon is active
                    match tokio::net::UnixStream::connect(path).await {
                        Ok(_) => {
                            return Err(IpcTransportError::AddressInUse(format!(
                                "Active daemon already listening on socket '{}'",
                                path.display()
                            )));
                        }
                        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                            // Stale socket from previous crash; remove it
                            let _ = std::fs::remove_file(path);
                        }
                        Err(_) => {
                            // Other error, attempt removal
                            let _ = std::fs::remove_file(path);
                        }
                    }
                }

                // Ensure parent directory exists
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                let listener = tokio::net::UnixListener::bind(path)?;
                Ok(Self {
                    listener,
                    socket_path: Some(path.clone()),
                })
            }
            #[cfg(not(unix))]
            IpcEndpoint::UnixSocket(_) => Err(IpcTransportError::Io(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unix domain sockets are not supported on this platform",
            ))),
            #[cfg(windows)]
            IpcEndpoint::NamedPipe(pipe_name) => {
                let server = tokio::net::windows::named_pipe::ServerOptions::new()
                    .first_pipe_instance(true)
                    .create(pipe_name)?;
                Ok(Self {
                    pipe_name: pipe_name.clone(),
                    server: Some(server),
                })
            }
            #[cfg(not(windows))]
            IpcEndpoint::NamedPipe(_) => Err(IpcTransportError::Io(io::Error::new(
                io::ErrorKind::Unsupported,
                "Named pipes are not supported on this platform",
            ))),
        }
    }

    /// Accept the next incoming IPC connection.
    pub async fn accept(&mut self) -> Result<IpcConnection, IpcTransportError> {
        #[cfg(unix)]
        {
            let (stream, _addr) = self.listener.accept().await?;
            Ok(IpcConnection::from_stream(InnerStream::Unix(stream)))
        }
        #[cfg(windows)]
        {
            let server = self
                .server
                .take()
                .ok_or_else(|| io::Error::other("Named pipe server missing"))?;
            server.connect().await?;

            // Instantly create the subsequent server instance for future clients
            let next_server =
                tokio::net::windows::named_pipe::ServerOptions::new().create(&self.pipe_name)?;
            self.server = Some(next_server);

            Ok(IpcConnection::from_stream(InnerStream::NamedPipeServer(
                server,
            )))
        }
    }
}

#[cfg(unix)]
impl Drop for IpcListener {
    fn drop(&mut self) {
        if let Some(ref path) = self.socket_path {
            let _ = std::fs::remove_file(path);
        }
    }
}
