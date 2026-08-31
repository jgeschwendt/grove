//! Daemon configuration, and the bind gate that refuses to expose it.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use tracing::Level;

use crate::Error;
use crate::app::DEFAULT_DRAIN;
use crate::engine::DEFAULT_CLONE_LIMIT;
use crate::lane::DEFAULT_IDLE;
use crate::logs::{DEFAULT_LEVEL, parse_level};

/// `GROVE_BIND`'s default: the loopback interface, port 7777.
pub const DEFAULT_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7777);

/// The version this daemon reports on `/api/health` and `/api/daemon/version` — the
/// single workspace version, which is also what the self-update health gate compares
/// against the bundle it just flipped to.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything the daemon needs before it can bind: where to listen, which home it
/// realizes, whether the shutdown route is armed, and the version it publishes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub bind: SocketAddr,
    pub home: PathBuf,
    /// Whether `POST /api/daemon/shutdown` actually drains the server. Always true
    /// in production; a test that wants the acknowledgement without losing its
    /// server turns it off, exactly as v1's `:enable_shutdown` did.
    pub enable_shutdown: bool,
    pub version: String,
    /// Whether to watch `manifest.toml` for out-of-band edits.
    ///
    /// **Off in [`Config::new`], on in [`Config::from_env`]** — the deliberate split
    /// v1 expressed as "default on, off in test". The HTTP nudge is the reliable
    /// path and every test drives it; a filesystem watch in a test turns the test's
    /// own writes to its home into autonomous convergence passes racing its
    /// assertions. `GROVE_MANIFEST_FS_WATCH=0` turns it off in production too.
    pub fs_watch: bool,
    /// How many roots may hold a clone at once. `GROVE_CLONE_LIMIT`.
    pub clone_limit: usize,
    /// How long a per-root lane sits idle before reaping itself.
    pub lane_idle: Duration,
    /// How long the whole graceful drain has once shutdown is requested — open
    /// responses first, then lane work. See [`crate::app::DEFAULT_DRAIN`] for why it
    /// is a bound rather than a target.
    pub drain_budget: Duration,
    /// The level at or above which lines enter the log ring `GET /api/events`
    /// streams. `GROVE_LOG_RING`; `debug` and below are dropped by default.
    ///
    /// Separate from `GROVE_LOG`, which is what the *process* writes to stderr:
    /// one is a UI feed, the other a file an operator greps, and wanting a debug
    /// log on disk is not wanting one in a dashboard.
    pub log_level: Level,
}

impl Config {
    /// A config for `home` on `bind`, refusing a non-loopback address.
    pub fn new(home: impl Into<PathBuf>, bind: SocketAddr) -> Result<Self, Error> {
        guard_loopback(bind)?;
        Ok(Self {
            bind,
            home: home.into(),
            enable_shutdown: true,
            version: VERSION.to_string(),
            fs_watch: false,
            clone_limit: DEFAULT_CLONE_LIMIT,
            lane_idle: DEFAULT_IDLE,
            drain_budget: DEFAULT_DRAIN,
            log_level: DEFAULT_LEVEL,
        })
    }

    /// Resolve from the environment: `GROVE_HOME` (→ `~/.grove`), `GROVE_BIND`
    /// (→ [`DEFAULT_BIND`]), `GROVE_CLONE_LIMIT` and `GROVE_MANIFEST_FS_WATCH`.
    pub fn from_env() -> Result<Self, Error> {
        // `grove_ops::home`, not a second copy of the rule: the daemon realizes the
        // home the CLI beside it resolves, and a divergence here would be a split
        // brain nothing fails on until the two disagree.
        let home = grove_ops::home();
        let bind = match std::env::var("GROVE_BIND") {
            Ok(raw) => parse_bind(&raw)?,
            Err(_) => DEFAULT_BIND,
        };
        let mut config = Self::new(home, bind)?;
        config.fs_watch = flag("GROVE_MANIFEST_FS_WATCH", true);
        // An unparseable or zero limit falls back to the default rather than
        // failing the boot or, worse, reading as "no clones at all".
        config.clone_limit = std::env::var("GROVE_CLONE_LIMIT")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(DEFAULT_CLONE_LIMIT);
        config.log_level = parse_level(
            std::env::var("GROVE_LOG_RING").ok().as_deref(),
            DEFAULT_LEVEL,
        );
        Ok(config)
    }

    /// Arm or disarm the shutdown route.
    #[must_use]
    pub fn with_shutdown_enabled(mut self, enabled: bool) -> Self {
        self.enable_shutdown = enabled;
        self
    }

    /// Report this version instead of the compiled-in one.
    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Turn the filesystem watch on or off.
    #[must_use]
    pub fn with_fs_watch(mut self, enabled: bool) -> Self {
        self.fs_watch = enabled;
        self
    }

    /// Bound concurrent clones differently.
    #[must_use]
    pub fn with_clone_limit(mut self, limit: usize) -> Self {
        self.clone_limit = limit;
        self
    }

    /// Reap idle lanes after `idle` instead of [`DEFAULT_IDLE`].
    #[must_use]
    pub fn with_lane_idle(mut self, idle: Duration) -> Self {
        self.lane_idle = idle;
        self
    }

    /// Bound the graceful drain at `budget` instead of [`DEFAULT_DRAIN`].
    #[must_use]
    pub fn with_drain_budget(mut self, budget: Duration) -> Self {
        self.drain_budget = budget;
        self
    }

    /// Capture log lines at `level` and above into the ring.
    #[must_use]
    pub fn with_log_level(mut self, level: Level) -> Self {
        self.log_level = level;
        self
    }
}

/// An on/off environment flag, read through [`flag_value`].
fn flag(name: &str, default: bool) -> bool {
    flag_value(std::env::var(name).ok().as_deref(), default)
}

/// The flag's rule, separated from the environment read so it is testable: `0`,
/// `false`, `no` and `off` are off, anything else present is on, absent is `default`.
///
/// A pure function rather than a test that sets the variable — `std::env::set_var`
/// is `unsafe` in edition 2024 (it races every other thread's `getenv`) and this
/// workspace forbids `unsafe`, so the environment is read at exactly one place and
/// the decision is tested where it lives.
fn flag_value(raw: Option<&str>, default: bool) -> bool {
    raw.map_or(default, |raw| {
        !matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// Parse a `GROVE_BIND` value. **Literal addresses only** — a hostname is refused
/// rather than resolved.
///
/// Resolution is where a loopback-only posture leaks: a name resolves to whatever
/// the resolver says today, so `grove.local:7777` could pass a gate written against
/// the address it happened to yield at boot and bind the LAN after a DHCP lease
/// changes. The CLI still resolves a hostname for its *client* address (it has to
/// reach whatever is listening); the daemon, which decides what to expose, does not.
pub fn parse_bind(raw: &str) -> Result<SocketAddr, Error> {
    let addr = raw.parse::<SocketAddr>().map_err(|_| {
        Error::Bind(format!(
            "GROVE_BIND {raw:?} is not a literal `ip:port` (a hostname is not accepted: \
             the daemon binds only literal loopback addresses — use 127.0.0.1:7777 or \
             [::1]:7777)"
        ))
    })?;
    guard_loopback(addr)?;
    Ok(addr)
}

/// Refuse any non-loopback bind, with no override.
///
/// The mutating API (`/api/daemon/shutdown`, `/api/roots/remove`, …) is
/// unauthenticated until served-mode auth lands, so a non-loopback bind would put it
/// on the network. Carried law 10; retires only with the auth layer, together with
/// the mutation guard.
// stele:landmark loopback-bind-gate
pub fn guard_loopback(bind: SocketAddr) -> Result<(), Error> {
    if bind.ip().is_loopback() {
        return Ok(());
    }
    Err(Error::Bind(format!(
        "refusing to bind grove to the non-loopback address {bind} while the API is \
         unauthenticated. grove is loopback-only until served-mode auth lands; bind to \
         127.0.0.1 or [::1]"
    )))
}

#[cfg(test)]
mod tests {
    use super::{Config, DEFAULT_BIND, parse_bind};
    use std::net::SocketAddr;
    use std::time::Duration;

    #[test]
    fn a_loopback_literal_parses() {
        assert_eq!(
            parse_bind("127.0.0.1:7777").unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 7777))
        );
        // The whole 127/8 block is loopback, and so is IPv6's `::1`.
        assert!(parse_bind("127.9.9.9:1234").is_ok());
        assert!(parse_bind("[::1]:7777").is_ok());
        assert_eq!(DEFAULT_BIND, SocketAddr::from(([127, 0, 0, 1], 7777)));
    }

    /// The gate itself: a routable address is refused at startup, loudly, with no
    /// override to reach for.
    #[test]
    fn a_non_loopback_bind_is_refused() {
        let err = parse_bind("0.0.0.0:7777").unwrap_err().to_string();
        assert!(err.contains("non-loopback"), "{err}");
        assert!(parse_bind("192.168.1.10:7777").is_err());
        assert!(parse_bind("[::]:7777").is_err());
        // …and through the constructor, not only the parser.
        assert!(Config::new("/tmp/home", SocketAddr::from(([10, 0, 0, 2], 7777))).is_err());
    }

    /// A hostname is refused rather than resolved: what a name points at is not the
    /// daemon's to decide, and a gate applied after resolution is a gate applied to
    /// whatever the resolver said that second.
    #[test]
    fn a_hostname_bind_is_refused_with_a_diagnostic() {
        let err = parse_bind("localhost:7777").unwrap_err().to_string();
        assert!(err.contains("hostname"), "{err}");
        assert!(err.contains("127.0.0.1:7777"), "the fix is named: {err}");
        assert!(parse_bind("not a bind").is_err());
    }

    #[test]
    fn a_config_defaults_to_an_armed_shutdown_and_the_crate_version() {
        let config = Config::new("/tmp/home", DEFAULT_BIND).unwrap();
        assert!(config.enable_shutdown);
        assert_eq!(config.version, env!("CARGO_PKG_VERSION"));
        assert!(!config.with_shutdown_enabled(false).enable_shutdown);
    }

    /// The engine-room defaults, and the fs-watch split: a hand-built config (every
    /// test's, and the only constructor that does not read the environment) leaves
    /// the filesystem watch off, so a test's own writes to its home never become
    /// autonomous convergence passes racing its assertions.
    #[test]
    fn a_hand_built_config_leaves_the_fs_watch_off() {
        let config = Config::new("/tmp/home", DEFAULT_BIND).unwrap();
        assert!(!config.fs_watch, "fs-watch is opt-in, not opt-out");
        assert_eq!(config.clone_limit, super::DEFAULT_CLONE_LIMIT);
        assert_eq!(config.lane_idle, super::DEFAULT_IDLE);
        assert_eq!(config.drain_budget, super::DEFAULT_DRAIN);
        assert_eq!(config.log_level, super::DEFAULT_LEVEL);

        assert!(config.clone().with_fs_watch(true).fs_watch);
        assert_eq!(config.clone().with_clone_limit(1).clone_limit, 1);
        assert_eq!(
            config
                .clone()
                .with_drain_budget(Duration::from_secs(2))
                .drain_budget,
            Duration::from_secs(2)
        );
        assert_eq!(
            config.with_lane_idle(Duration::from_secs(1)).lane_idle,
            Duration::from_secs(1)
        );
    }

    /// The drain budget must sit under the CLI's own `STOP_GRACE` (10 s), or a
    /// graceful stop always ends in the escalation ladder it exists to avoid.
    #[test]
    fn the_drain_budget_fits_inside_the_cli_stop_grace() {
        assert!(
            super::DEFAULT_DRAIN < Duration::from_secs(10),
            "a drain longer than `grove off`'s grace is a drain that never completes"
        );
    }

    /// The environment flag's spellings, since "the watcher silently stayed on"
    /// is not a failure anything else would surface.
    #[test]
    fn an_off_flag_reads_the_usual_spellings() {
        for raw in ["0", "false", "no", "off", "OFF", " false "] {
            assert!(!super::flag_value(Some(raw), true), "{raw:?} is off");
        }
        for raw in ["1", "true", "yes", ""] {
            assert!(super::flag_value(Some(raw), false), "{raw:?} is on");
        }
        assert!(super::flag_value(None, true), "absent takes the default");
        assert!(!super::flag_value(None, false));
    }
}
