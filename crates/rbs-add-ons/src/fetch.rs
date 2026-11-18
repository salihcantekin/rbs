//! The HTTP side of a transfer, behind a trait.

use std::fmt;
use std::io::Read;

/// The answer to one GET. Redirects are returned, never followed, so this crate
/// checks every hop against the entry's host list (ADR-0145 decision 4).
pub enum Response {
    /// A redirect to this absolute URL.
    Redirect(String),
    /// A success; the body follows.
    Body(Box<dyn Read + Send>),
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redirect(url) => f.debug_tuple("Redirect").field(url).finish(),
            Self::Body(_) => f.write_str("Body(..)"),
        }
    }
}

/// A failed GET or a failed body read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// No connection, a TLS failure, an HTTP error status, or a broken body.
    Network(String),
    /// No byte arrived within the stall timeout.
    Stalled,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(e) => write!(f, "network error: {e}"),
            Self::Stalled => f.write_str("the download stalled"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Makes HTTPS requests.
///
/// An implementation must verify TLS against the OS trust roots, never follow a
/// redirect itself, time out a connection after 15 s and a read that receives no
/// byte for 30 s (a body read then fails with an `io::Error` whose inner error is
/// [`FetchError::Stalled`] or whose kind is `TimedOut`).
pub trait Fetch: Send + Sync {
    /// One GET of `url`.
    fn get(&self, url: &str) -> Result<Response, FetchError>;
}
