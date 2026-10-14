//! Startup failures. Nothing here crosses the wire — a daemon that cannot bind
//! never answers a request; these reach the operator through `grove serve`'s exit.

/// A failure that stops the daemon before it serves.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The configured bind is not a literal loopback address (see
    /// [`crate::config::guard_loopback`]).
    #[error("{0}")]
    Bind(String),
    /// The listener could not be opened — the port is held, or the address is not
    /// assigned to this host.
    #[error("bind: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    /// The two arms an operator reads at `grove serve`'s exit, and the io cause kept
    /// as the source rather than flattened into the message alone.
    #[test]
    fn a_held_port_reads_as_bind_and_keeps_its_cause() {
        let held = super::Error::from(std::io::Error::other("address in use"));
        assert_eq!(held.to_string(), "bind: address in use");
        assert_eq!(held.source().unwrap().to_string(), "address in use");
        let bind = super::Error::Bind("not loopback".into());
        assert_eq!(bind.to_string(), "not loopback");
        assert!(bind.source().is_none());
    }
}
