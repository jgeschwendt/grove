//! Startup failures. Nothing here crosses the wire — a daemon that cannot bind
//! never answers a request; these reach the operator through `grove serve`'s exit.

use std::fmt;

/// A failure that stops the daemon before it serves.
#[derive(Debug)]
pub enum Error {
    /// The configured bind is not a literal loopback address (see
    /// [`crate::config::guard_loopback`]).
    Bind(String),
    /// The listener could not be opened — the port is held, or the address is not
    /// assigned to this host.
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind(message) => f.write_str(message),
            Self::Io(e) => write!(f, "bind: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind(_) => None,
            Self::Io(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
