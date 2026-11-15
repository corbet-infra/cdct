//! The client side of the protocol: connect, ask, subscribe.
//!
//! The CLI and the tray speak through this module. A local client connects to
//! the daemon's Unix socket; a guest's tray opens its virtio-serial device and
//! speaks the same JSON lines over that byte stream. Polling never happens
//! here: a subscription blocks on the daemon's pushed replies.

use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::protocol::{Reply, Request, Status, decode, encode};

/// The largest accepted message, so a broken peer cannot grow the buffer
/// without bound.
const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// A bidirectional byte stream the protocol runs over.
pub trait Stream: Read + Write + Send {}

impl<T: Read + Write + Send> Stream for T {}

/// Where a client connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// The daemon's local control socket on the host.
    LocalSocket(PathBuf),
    /// A virtio-serial device inside a guest.
    Device(PathBuf),
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalSocket(path) => write!(f, "unix:{}", path.display()),
            Self::Device(path) => write!(f, "device:{}", path.display()),
        }
    }
}

impl Endpoint {
    /// Parse `unix:<path>` or `device:<path>`. A bare path is a Unix socket.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        if let Some(path) = value.strip_prefix("device:") {
            return Self::Device(PathBuf::from(path));
        }
        if let Some(path) = value.strip_prefix("unix:") {
            return Self::LocalSocket(PathBuf::from(path));
        }
        Self::LocalSocket(PathBuf::from(value))
    }

    /// The default endpoint for this platform.
    ///
    /// On a host the daemon's socket is used. Inside a guest the socket does
    /// not exist, so the platform's virtio-serial device is used instead. The
    /// device path can always be overridden with `$CDCT_ENDPOINT`.
    #[must_use]
    pub fn detect(socket: &Path) -> Self {
        if let Some(value) = std::env::var_os("CDCT_ENDPOINT") {
            return Self::parse(&value.to_string_lossy());
        }
        if socket.exists() {
            return Self::LocalSocket(socket.to_path_buf());
        }
        if let Some(device) = guest_device_path()
            && device.exists()
        {
            return Self::Device(device);
        }
        Self::LocalSocket(socket.to_path_buf())
    }

    /// Open the underlying stream.
    pub fn connect(&self) -> io::Result<Box<dyn Stream>> {
        match self {
            #[cfg(unix)]
            Self::LocalSocket(path) => Ok(Box::new(std::os::unix::net::UnixStream::connect(path)?)),
            #[cfg(not(unix))]
            Self::LocalSocket(path) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "Unix sockets are unavailable on this platform: {}",
                    path.display()
                ),
            )),
            Self::Device(path) => Ok(Box::new(
                OpenOptions::new().read(true).write(true).open(path)?,
            )),
        }
    }
}

/// The virtio-serial device a guest's tray uses when no endpoint is configured.
#[must_use]
pub fn guest_device_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Some(PathBuf::from("/dev/virtio-ports/cdct.0"))
    }
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from("/dev/cu.cdct"))
    }
    #[cfg(target_os = "windows")]
    {
        Some(PathBuf::from(r"\\.\COM3"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// A connected client that frames messages as JSON lines.
pub struct Client {
    stream: Box<dyn Stream>,
    buffer: Vec<u8>,
}

impl Client {
    /// Connect to an endpoint.
    pub fn connect(endpoint: &Endpoint) -> Result<Self, ClientError> {
        let stream = endpoint.connect().map_err(|source| ClientError::Connect {
            endpoint: endpoint.clone(),
            source,
        })?;
        Ok(Self {
            stream,
            buffer: Vec::new(),
        })
    }

    /// Send one request.
    pub fn send(&mut self, request: &Request) -> Result<(), ClientError> {
        let line = encode(request)?;
        self.stream.write_all(line.as_bytes())?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()?;
        Ok(())
    }

    /// Receive one reply; `None` means the daemon closed the connection.
    pub fn receive(&mut self) -> Result<Option<Reply>, ClientError> {
        let Some(line) = self.read_line()? else {
            return Ok(None);
        };
        if line.trim().is_empty() {
            return self.receive();
        }
        Ok(Some(decode(&line)?))
    }

    /// Send a request and read its single answer, turning a daemon-side error
    /// reply into an error.
    pub fn ask(&mut self, request: &Request) -> Result<Reply, ClientError> {
        self.send(request)?;
        match self.receive()? {
            Some(Reply::Error { message }) => Err(ClientError::Refused(message)),
            Some(reply) => Ok(reply),
            None => Err(ClientError::Closed),
        }
    }

    /// Ask for the current status.
    pub fn status(&mut self) -> Result<Status, ClientError> {
        match self.ask(&Request::Status)? {
            Reply::Status { status } => Ok(status),
            other => Err(ClientError::Unexpected(reply_name(&other))),
        }
    }

    /// Ask for a switch and return the decision.
    pub fn switch(&mut self, target: &cdrv::KernelOwner) -> Result<Reply, ClientError> {
        self.ask(&Request::Switch {
            target: target.clone(),
        })
    }

    /// Store the live replica counts of the displaced tenants with the card's
    /// holder and return the holder's identifier. Local socket only.
    pub fn record_displaced(
        &mut self,
        displaced: &[crate::gpu::DisplacedTenant],
    ) -> Result<String, ClientError> {
        match self.ask(&Request::RecordDisplaced {
            displaced: displaced.to_vec(),
        })? {
            Reply::Recorded { holder } => Ok(holder),
            other => Err(ClientError::Unexpected(reply_name(&other))),
        }
    }

    /// Report that a granted plan ran to its end; returns who owns the card
    /// now. Local socket only.
    pub fn completed(
        &mut self,
        target: &cdrv::KernelOwner,
    ) -> Result<cdrv::KernelOwner, ClientError> {
        match self.ask(&Request::Completed {
            target: target.clone(),
        })? {
            Reply::Completed { owner } => Ok(owner),
            other => Err(ClientError::Unexpected(reply_name(&other))),
        }
    }

    fn read_line(&mut self) -> Result<Option<String>, ClientError> {
        loop {
            if let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=position).collect();
                let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                return Ok(Some(text));
            }
            if self.buffer.len() > MAX_MESSAGE_BYTES {
                return Err(ClientError::TooLarge);
            }
            let mut chunk = [0_u8; 8192];
            let read = self.stream.read(&mut chunk)?;
            if read == 0 {
                return Ok(None);
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

fn reply_name(reply: &Reply) -> &'static str {
    match reply {
        Reply::Status { .. } => "status",
        Reply::Subscribed { .. } => "subscribed",
        Reply::Plan { .. } => "plan",
        Reply::Switch { .. } => "switch",
        Reply::Granted { .. } => "granted",
        Reply::Recorded { .. } => "recorded",
        Reply::Completed { .. } => "completed",
        Reply::Event { .. } => "event",
        Reply::Error { .. } => "error",
    }
}

/// A client-side failure.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The endpoint could not be reached.
    #[error("cannot connect to {endpoint}: {source}")]
    Connect {
        /// The endpoint that failed.
        endpoint: Endpoint,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The connection failed while in use.
    #[error("connection failed: {0}")]
    Io(#[from] io::Error),
    /// A message was not valid JSON for the protocol.
    #[error("invalid protocol message: {0}")]
    Protocol(#[from] serde_json::Error),
    /// The daemon answered with an error.
    #[error("{0}")]
    Refused(String),
    /// The daemon closed the connection before answering.
    #[error("the daemon closed the connection")]
    Closed,
    /// The daemon answered with a reply this request never produces.
    #[error("unexpected reply: {0}")]
    Unexpected(&'static str),
    /// A peer sent a line larger than the protocol allows.
    #[error("a protocol message exceeded {MAX_MESSAGE_BYTES} bytes")]
    TooLarge,
}
