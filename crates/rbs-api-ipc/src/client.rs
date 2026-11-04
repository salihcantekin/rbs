//! A blocking client of the IPC transport (`rbs-cli`, tests). It checks that the
//! server runs as the current user before it sends anything, reads the preamble, and
//! then exchanges whole messages.

use std::fmt;
use std::time::{Duration, Instant};

use rbs_api::CloseReason;

use crate::frame::{self, Header, PREAMBLE};
use crate::platform::ClientIo;
use crate::{Endpoint, NORMAL_CLOSE, close_reason};

/// Why a client could not connect.
#[derive(Debug)]
pub enum ClientError {
    /// Nothing serves the endpoint (RBS is not running, or remote control is off).
    NotRunning(std::io::Error),
    /// The program serving the endpoint does not run as the current user: nothing was
    /// sent to it.
    WrongServer,
    /// The server refused the connection.
    Refused(Option<CloseReason>),
    /// The server does not speak this framing.
    NotRbs,
    /// Reading or writing failed.
    Io(std::io::Error),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::NotRunning(e) => write!(f, "RBS is not serving remote control ({e})"),
            ClientError::WrongServer => {
                f.write_str("the program at the endpoint runs as another user, so it was not used")
            }
            ClientError::Refused(r) => write!(f, "the connection was refused ({r:?})"),
            ClientError::NotRbs => f.write_str("the endpoint is not an RBS control endpoint"),
            ClientError::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ClientError {}

/// What arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    /// A whole message.
    Message(String),
    /// The server closed, with the reason when it sent one.
    Closed(Option<CloseReason>),
}

/// A connection to an RBS IPC endpoint.
pub struct IpcClient {
    io: ClientIo,
    buf: Vec<u8>,
    closed: bool,
}

impl IpcClient {
    /// Connects to `endpoint`, checks the server's user and waits at most `timeout` for
    /// the preamble.
    pub fn connect(endpoint: &Endpoint, timeout: Duration) -> Result<IpcClient, ClientError> {
        let io = ClientIo::connect(&endpoint.0, timeout)?;
        let mut c = IpcClient {
            io,
            buf: Vec::new(),
            closed: false,
        };
        let deadline = Instant::now() + timeout;
        while c.buf.len() < PREAMBLE.len() {
            if c.buf.first().is_some_and(|b| b & 0x80 != 0) {
                // A close frame: refused.
                return match c.recv(deadline.saturating_duration_since(Instant::now())) {
                    Ok(Some(Received::Closed(r))) => Err(ClientError::Refused(r)),
                    _ => Err(ClientError::Refused(None)),
                };
            }
            match c.fill(deadline).map_err(ClientError::Io)? {
                Fill::Data => {}
                Fill::Eof => return Err(ClientError::Refused(None)),
                Fill::Timeout => return Err(ClientError::NotRbs),
            }
        }
        if c.buf[..PREAMBLE.len()] != PREAMBLE {
            return Err(ClientError::NotRbs);
        }
        c.buf.drain(..PREAMBLE.len());
        Ok(c)
    }

    /// Sends one message.
    pub fn send(&mut self, text: &str) -> std::io::Result<()> {
        self.io.write_all(&frame::message(text)?)
    }

    /// Sends raw bytes (tests of the server's framing checks).
    pub fn send_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.io.write_all(bytes)
    }

    /// The next message or the close, waiting at most `timeout`; `None` when nothing
    /// whole came in time.
    pub fn recv(&mut self, timeout: Duration) -> std::io::Result<Option<Received>> {
        if self.closed {
            return Ok(Some(Received::Closed(None)));
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(r) = self.take() {
                return Ok(Some(r));
            }
            match self.fill(deadline)? {
                Fill::Data => {}
                Fill::Timeout => return Ok(None),
                Fill::Eof => {
                    self.closed = true;
                    return Ok(Some(Received::Closed(None)));
                }
            }
        }
    }

    /// Closes the connection with a normal close frame.
    pub fn close(&mut self) {
        let _ = self.io.write_all(&frame::close(NORMAL_CLOSE, ""));
        self.io.shutdown_write();
    }

    /// A whole frame from the buffer, if one is there.
    fn take(&mut self) -> Option<Received> {
        if self.buf.len() < 4 {
            return None;
        }
        let header = Header::parse([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        let (len, close) = match header {
            Header::Message(n) => (n as usize, false),
            Header::Close(n) => (n as usize, true),
        };
        if self.buf.len() < 4 + len {
            return None;
        }
        let payload: Vec<u8> = self.buf.drain(..4 + len).skip(4).collect();
        Some(if close {
            self.closed = true;
            Received::Closed(frame::close_code_of(&payload).and_then(close_reason))
        } else {
            Received::Message(String::from_utf8_lossy(&payload).into_owned())
        })
    }

    fn fill(&mut self, deadline: Instant) -> std::io::Result<Fill> {
        let mut chunk = [0u8; 16 * 1024];
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(Fill::Timeout);
        }
        match self.io.read(&mut chunk, Some(left))? {
            None => Ok(Fill::Timeout),
            Some(0) => Ok(Fill::Eof),
            Some(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(Fill::Data)
            }
        }
    }
}

enum Fill {
    Data,
    Eof,
    Timeout,
}
