//! Local daemon custody: `on` / `off` / `reboot`.
//!
//! [`ServerControl`] launches the daemon as a detached child, tracks it by PID file
//! under `GROVE_HOME`, and stops it gracefully (drain over the API, then signal).
//! The CLI is not itself a daemon — this type *controls* one. Since v2 ships a
//! single executable, the thing it launches is the grove binary itself: the launcher
//! contract is `$GROVE_INSTALL/current/bin/grove serve` for an installed release, and
//! this very binary (`current_exe`) when nothing is installed yet.
//!
//! Both roots are held, because they are different things: `GROVE_HOME` is the
//! workspace the daemon realizes (pid file, lock, log, manifest, `code/`), and
//! `GROVE_INSTALL` is the release tree the launcher runs out of. The child is handed
//! both, so a daemon that runs its own `grove up` flips the install this CLI
//! resolved rather than one it re-derives.
//!
//! Every external dependency is held as data (paths, bind, timeouts, the [`Clock`],
//! the server [`Interface`]), so tests construct the struct with fakes — no env
//! reads, no globals, parallel-safe.

use std::fs::{self, OpenOptions};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use grove_ops::clock::{Clock, SystemClock};
use nix::errno::Errno;
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};

use crate::api::ApiClient;
use crate::error::CliError;
// Custody's budgets live with every other budget the CLI spends — see
// [`crate::timeouts`] for why they are one module rather than literals at the sites
// that spend them.
use crate::timeouts::{
    CUSTODY_PROBE as PROBE_TIMEOUT, KILL as KILL_TIMEOUT, POLL_INTERVAL, READY as READY_TIMEOUT,
    STOP_GRACE,
};

/// How [`ServerControl`] talks to the daemon it manages. Production speaks `Http`
/// (readiness via `GET /api/health`, graceful stop via `POST /api/daemon/shutdown`);
/// tests use `Tcp` against a fake child, probing only that the port is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Interface {
    Http,
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "constructed only in tests; production always speaks Http"
        )
    )]
    Tcp,
}

/// What a candidate pid *is*, beyond bare liveness — so a recycled pid (the same
/// number reassigned to an unrelated process) reads as stale rather than as our
/// daemon still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PidState {
    /// No such process (`ESRCH`) — the pid file is stale.
    Dead,
    /// Alive and ours to inspect; carries its start-time identity token (`None`
    /// when the platform probe is unavailable — then only liveness is known).
    Alive(Option<u64>),
    /// Alive but owned by another user (`EPERM`): running, not inspectable. The
    /// pid file must NOT be cleared — a live process still holds the port.
    Foreign,
}

/// Probe a pid: liveness (via `kill(pid, 0)`) plus, when it's alive and ours, a
/// start-time identity token so a recycled pid is detected. Seam: a field on
/// [`ServerControl`], so tests inject `Dead`/`Alive`/`Foreign` without a real
/// process.
type PidProbe = fn(Pid) -> PidState;

/// Controls the local grove daemon: start, stop, restart.
pub struct ServerControl {
    home: PathBuf,
    install: PathBuf,
    bind: SocketAddr,
    server_program: PathBuf,
    server_args: Vec<String>,
    interface: Interface,
    ready_timeout: Duration,
    stop_grace: Duration,
    kill_timeout: Duration,
    pid_probe: PidProbe,
    clock: Arc<dyn Clock>,
}

impl ServerControl {
    /// Resolve from the environment: `GROVE_HOME` (→ `~/.grove`), `GROVE_INSTALL`
    /// (→ `~/.local/share/grove`), `GROVE_BIND` (→ `127.0.0.1:7777`), and the
    /// launcher. An unparseable `GROVE_BIND` is a loud error, never a silent default
    /// — otherwise `on`/`off` would target one address while the API client talks to
    /// another.
    ///
    /// The launcher hangs off the *install* root, not the workspace: what to run is
    /// release state, and the two roots are resolved once each in `grove-ops`.
    pub fn from_env() -> Result<Self, CliError> {
        let home = grove_ops::home();
        let install = grove_ops::install_home();
        let bind = resolve_bind(std::env::var("GROVE_BIND").ok())?;
        let server_program = launcher(&install);

        Ok(Self {
            home,
            install,
            bind,
            server_program,
            server_args: vec!["serve".to_string()],
            interface: Interface::Http,
            ready_timeout: READY_TIMEOUT,
            stop_grace: STOP_GRACE,
            kill_timeout: KILL_TIMEOUT,
            pid_probe: probe_pid,
            clock: Arc::new(SystemClock),
        })
    }

    /// Start the daemon. Errors with [`CliError::Conflict`] if already running.
    pub fn on(&self) -> Result<(), CliError> {
        fs::create_dir_all(&self.home)
            .map_err(|e| CliError::Daemon(format!("create {}: {e}", self.home.display())))?;

        // Resolve the launcher before locking, so "not installed" errors cheaply.
        let cmd = self.server_command()?;

        // Serialize the liveness-check → spawn → pid-write under an exclusive
        // lock, so two concurrent `grove on` can't both start a daemon. Released
        // before the readiness wait (a second `on` then sees the live pid).
        let child = {
            let _lock = self.lock_start()?;
            if let Some(pid) = self.running_pid() {
                return Err(CliError::Conflict(format!(
                    "server already running (pid {pid}) at {}",
                    self.url()
                )));
            }
            // No pid file, but the bind is already held (a supervisor-managed
            // grove, or another service): a spawn is doomed to EADDRINUSE — and
            // worse, the squatter's 200 could answer OUR readiness poll during
            // the child's boot window. Refuse up front instead. Http-only: under
            // the `Tcp` test seam the pre-bound listener IS the fake readiness.
            if self.interface == Interface::Http && self.port_open() {
                return Err(CliError::Conflict(format!(
                    "something is already listening at {} (no pid file — a \
                     supervisor-managed grove server or another service); \
                     refusing to start a second server",
                    self.url()
                )));
            }
            match self.launch(cmd) {
                Ok(child) => child,
                Err(e) => {
                    let _ = fs::remove_file(self.pid_path());
                    return Err(e);
                }
            }
        };

        // A readiness timeout leaves the (running) child + pid in place so the
        // operator can inspect logs / `grove off` it.
        self.await_ready(child)?;
        println!("grove server listening at {}", self.url());
        Ok(())
    }

    /// Acquire an exclusive advisory lock for the start critical section. Held
    /// for the lifetime of the returned handle (released on drop). `O_NOFOLLOW`
    /// refuses a symlinked lock path (no redirect into truncating another file).
    ///
    /// `O_CLOEXEC` is load-bearing, not hygiene. `flock` belongs to the open file
    /// *description*, which a spawned child inherits: without it, the daemon we are
    /// about to launch keeps this lock for its whole life, and the next `grove on`
    /// blocks forever on `flock` instead of reporting the conflict it can already
    /// see. (`rustix::fs::open` passes exactly the flags given — unlike `std`, which
    /// adds `O_CLOEXEC` for you.) Pinned by
    /// `a_second_on_conflicts_instead_of_blocking_on_the_leaked_lock`.
    fn lock_start(&self) -> Result<OwnedFd, CliError> {
        let lock = open(
            self.home.join("grove.lock"),
            OFlags::CLOEXEC | OFlags::CREATE | OFlags::WRONLY | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|e| CliError::Daemon(format!("open start lock: {e}")))?;
        flock(&lock, FlockOperation::LockExclusive)
            .map_err(|e| CliError::Daemon(format!("acquire start lock: {e}")))?;
        Ok(lock)
    }

    /// Open the log, launch the detached child, and record its real pid. Returns
    /// the child pid so [`on`](Self::on) can gate readiness on our own child
    /// staying alive. The caller holds the start lock and clears the pid file on
    /// error.
    fn launch(&self, mut cmd: Command) -> Result<Pid, CliError> {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
            .map_err(|e| {
                CliError::Daemon(format!("open log at {}: {e}", self.log_path().display()))
            })?;

        cmd.stdin(Stdio::null())
            .stdout(
                log.try_clone()
                    .map_err(|e| CliError::Daemon(format!("clone log handle: {e}")))?,
            )
            .stderr(log)
            .process_group(0); // detach from the CLI's process group

        let child = cmd
            .spawn()
            .map_err(|e| CliError::Daemon(format!("spawn server: {e}")))?;

        // Record pid + start-time identity so a later recycled pid reads as stale.
        // `<pid>\nv2:<start>\n`; a bare `<pid>` (no probe) stays liveness-only.
        // The `v2:` prefix versions the token DERIVATION: a reader that finds a
        // token in another format (a pre-v2 file, or a future v3) must treat the
        // identity as unverifiable — liveness-only — never as stale, or a
        // derivation change would delete a live server's pid file at upgrade.
        let pid = Pid::from_raw(child.id().cast_signed());
        let contents = match (self.pid_probe)(pid) {
            PidState::Alive(Some(start)) => format!("{}\nv2:{start}\n", child.id()),
            _ => child.id().to_string(),
        };
        fs::write(self.pid_path(), contents)
            .map_err(|e| CliError::Daemon(format!("write pid: {e}")))?;
        Ok(pid)
    }

    /// Stop the daemon. First ask it to drain and stop itself over the API; if it
    /// hasn't exited within the grace period, escalate to SIGTERM, then SIGKILL.
    /// A daemon with no pid file — one managed by an external supervisor (served
    /// mode) — is drained over the API instead (see [`off_pidless`](Self::off_pidless)).
    pub fn off(&self) -> Result<(), CliError> {
        let Some((pid, recorded_start)) = self.running_server() else {
            return self.off_pidless();
        };

        // Graceful: let the daemon drain in-flight work and exit on its own. Gated on
        // identity like the pid-less path — a pid file and the bind can disagree
        // (GROVE_BIND moved since `grove on`, or the daemon lost its listener and a
        // local service took the port), and `off` never POSTs a shutdown at a stranger.
        // A stranger also can't answer for our pid, so an unverified drain would mean
        // waiting out the full stop grace before signalling the process we do own.
        let drained = self.identifies_as_grove()
            && self.request_shutdown()
            && self.await_exit(pid, self.stop_grace);

        // Escalate SIGTERM → SIGKILL, honoring the final wait: if even SIGKILL
        // didn't take within the window, the process is still alive. Each step
        // re-verifies identity first — a pid recycled inside the grace window is
        // not ours and must not be signalled.
        let exited = drained
            || self.escalate(pid, recorded_start, Signal::SIGTERM, self.stop_grace)
            || self.escalate(pid, recorded_start, Signal::SIGKILL, self.kill_timeout);

        if !exited {
            // Leave the pid file in place so the operator can see what's still
            // bound and retry; exit nonzero carrying the pid.
            return Err(CliError::Daemon(format!(
                "grove server (pid {pid}) did not exit after SIGKILL; pid file left at {}",
                self.pid_path().display()
            )));
        }

        let _ = fs::remove_file(self.pid_path());
        if drained {
            println!("grove server stopped");
        } else {
            println!("grove server stopped (forced; in-flight requests may have been interrupted)");
        }
        Ok(())
    }

    /// Stop a daemon that answers on the bind port but has no pid file to signal —
    /// e.g. one a supervisor started in served mode. Nothing answering → not
    /// running (success). A listener that does not identify as a grove server →
    /// also not running (success): `off` stays a safe no-op when a stranger holds
    /// the bind. A grove listener → drain over the API and wait for the port to
    /// close. A refused/failed drain can't be escalated without a pid, so it's a
    /// [`CliError::Daemon`]: the operator must stop it through whatever
    /// supervises it.
    fn off_pidless(&self) -> Result<(), CliError> {
        if !self.port_open() {
            println!("grove server is not running");
            return Ok(());
        }
        if !self.identifies_as_grove() {
            println!(
                "grove server is not running (another process is listening at {})",
                self.url()
            );
            return Ok(());
        }
        if self.request_shutdown() && self.await_port_closed(self.stop_grace + self.kill_timeout) {
            println!("grove server stopped");
            return Ok(());
        }
        Err(CliError::Daemon(format!(
            "a grove server is bound at {} but has no pid file at {}; it cannot be \
             force-stopped — stop it through whatever supervises it",
            self.url(),
            self.pid_path().display()
        )))
    }

    /// Whether the listener on the bind port is a grove server: `/api/health`
    /// answers the grove envelope (a JSON object carrying `ok`) at ANY status —
    /// a draining server 503s but still speaks it. Gates the pid-less drain so
    /// `off` never POSTs a shutdown at, or waits on, an unrelated service that
    /// happens to hold the bind. `Tcp` (tests) has no body to inspect, so it
    /// conservatively reads as grove.
    fn identifies_as_grove(&self) -> bool {
        if self.interface != Interface::Http {
            return true;
        }
        self.api_client().speaks_grove()
    }

    /// Re-verify identity, then signal `pid` and wait up to `timeout` for it to
    /// exit. Returns whether the daemon is gone. A pid that was recycled inside
    /// the grace window (Alive under a start time that no longer matches the one
    /// we recorded) is NOT ours — report it gone WITHOUT signalling the stranger.
    fn escalate(
        &self,
        pid: Pid,
        recorded_start: Option<u64>,
        sig: Signal,
        timeout: Duration,
    ) -> bool {
        if self.pid_recycled(pid, recorded_start) {
            return true; // our server already exited; an unrelated process holds the pid
        }
        let _ = signal::kill(pid, sig);
        self.await_exit(pid, timeout)
    }

    /// Whether the recorded server pid is no longer ours: gone (`Dead`), or Alive
    /// under a start-time identity that no longer matches the recorded one (the
    /// pid was recycled). A `Foreign` or unverifiable (`None`) identity stays ours
    /// to signal. Clears the pid file when it detects a recycle/death.
    fn pid_recycled(&self, pid: Pid, recorded_start: Option<u64>) -> bool {
        match (self.pid_probe)(pid) {
            PidState::Dead => {
                let _ = fs::remove_file(self.pid_path());
                true
            }
            PidState::Alive(Some(cur)) if recorded_start.is_some_and(|rec| rec != cur) => {
                let _ = fs::remove_file(self.pid_path());
                true
            }
            PidState::Alive(_) | PidState::Foreign => false,
        }
    }

    /// Whether the bind port currently accepts a TCP connection — a pid-less
    /// liveness probe (is a server there at all?) and the drain-complete signal
    /// (has it stopped accepting?).
    fn port_open(&self) -> bool {
        TcpStream::connect_timeout(&self.bind, PROBE_TIMEOUT).is_ok()
    }

    /// Block until the bind port stops accepting connections or `timeout` elapses.
    /// Returns `true` once the port is closed (the pid-less drain finished).
    fn await_port_closed(&self, timeout: Duration) -> bool {
        self.poll_until(timeout, || !self.port_open())
    }

    /// Ask the daemon to drain and stop itself. Returns `true` only when the
    /// shutdown request was accepted (so the caller can wait for a clean exit);
    /// `Tcp` interface (tests) and any transport error fall straight through to
    /// signalling.
    fn request_shutdown(&self) -> bool {
        self.interface == Interface::Http && self.api_client().request_shutdown()
    }

    /// Restart the daemon: stop (awaiting full exit) then start.
    pub fn reboot(&self) -> Result<(), CliError> {
        self.off()?;
        self.on()
    }

    /// The live server PID, if the PID file names a running process *with the
    /// recorded identity*. Clears the file when the pid is gone (`ESRCH`) or has
    /// been recycled (start-time mismatch); a live-but-foreign pid (`EPERM`) is
    /// reported running and its file preserved.
    #[must_use]
    pub fn running_pid(&self) -> Option<Pid> {
        self.running_server().map(|(pid, _)| pid)
    }

    /// Resolve the running server as `(pid, recorded_start)` — the pid plus its
    /// recorded `v2:` start-time identity token (`None` for a legacy/absent/foreign
    /// token). Same staleness handling as [`running_pid`](Self::running_pid); `off`
    /// carries the recorded start so each escalation signal can re-verify identity.
    fn running_server(&self) -> Option<(Pid, Option<u64>)> {
        let raw = fs::read_to_string(self.pid_path()).ok()?;
        let mut lines = raw.lines();
        let pid = Pid::from_raw(lines.next()?.trim().parse::<i32>().ok()?);
        // Only a `v2:` token participates in the identity comparison. Any other
        // shape (absent, pre-v2 legacy, future format) leaves `recorded_start`
        // None ⇒ liveness-only below — a live process is never read as stale
        // because the token DERIVATION changed underneath it.
        let recorded_start = lines
            .next()
            .and_then(|l| l.trim().strip_prefix("v2:"))
            .and_then(|t| t.parse::<u64>().ok());

        match (self.pid_probe)(pid) {
            PidState::Dead => {
                let _ = fs::remove_file(self.pid_path());
                None
            }
            // Alive but not ours to inspect — never clear a file a live process holds.
            PidState::Foreign => Some((pid, recorded_start)),
            PidState::Alive(current_start) => {
                // A recorded identity that no longer matches ⇒ the pid was recycled.
                match (recorded_start, current_start) {
                    (Some(rec), Some(cur)) if rec != cur => {
                        let _ = fs::remove_file(self.pid_path());
                        None
                    }
                    _ => Some((pid, recorded_start)),
                }
            }
        }
    }

    fn pid_path(&self) -> PathBuf {
        self.home.join("grove.pid")
    }

    fn log_path(&self) -> PathBuf {
        self.home.join("grove.log")
    }

    fn url(&self) -> String {
        format!("http://{}", self.bind)
    }

    /// An `ApiClient` bound to this server's address with the probe budget — the
    /// one place the readiness/identity HTTP policy (timeout, proxy) is set.
    fn api_client(&self) -> ApiClient {
        ApiClient::at(self.bind, PROBE_TIMEOUT)
    }

    /// The command that launches the daemon. Seam: the program/args are fields, so
    /// tests inject a fake. Production runs `<grove> serve` — the installed release's
    /// binary when there is one, otherwise this very executable.
    fn server_command(&self) -> Result<Command, CliError> {
        if !self.server_program.exists() {
            return Err(CliError::Daemon(format!(
                "no grove binary to launch at {} — run `grove up` to install one",
                self.server_program.display()
            )));
        }
        let mut cmd = Command::new(&self.server_program);
        cmd.args(&self.server_args);
        // Hand the child the RESOLVED bind and both roots this CLI decided on. The
        // CLI accepts a hostname (`localhost:7777`, resolved above) but the daemon
        // binds only literal IPs, and a child that inherited a *different*
        // `GROVE_HOME` would realize a different manifest than the one the operator
        // is looking at. `GROVE_INSTALL` travels for the mirror-image reason: a
        // served daemon runs its own `grove up`, and one that re-derived the install
        // root would flip a different tree than the one it was launched out of.
        cmd.env("GROVE_BIND", self.bind.to_string());
        cmd.env("GROVE_HOME", &self.home);
        cmd.env("GROVE_INSTALL", &self.install);
        Ok(cmd)
    }

    /// Poll until *our* child reports ready, or the timeout elapses. Readiness
    /// means the spawned `child` is still alive AND health answers 2xx: a child
    /// that dies on spawn (e.g. `EADDRINUSE` because another server already holds
    /// the bind) must not read as success off that other server's 200. The child
    /// is checked first each round so a foreign 200 can't win a race against the
    /// death we're about to observe.
    // stele:landmark cli-health-poll
    fn await_ready(&self, child: Pid) -> Result<(), CliError> {
        let deadline = self.clock.deadline(self.ready_timeout);
        loop {
            if self.has_exited(child) {
                let _ = fs::remove_file(self.pid_path());
                return Err(CliError::Daemon(format!(
                    "grove server process exited immediately; check logs at {} \
                     (another process may already be bound at {})",
                    self.log_path().display(),
                    self.bind
                )));
            }
            if self.is_ready() {
                return Ok(());
            }
            if deadline.expired(&*self.clock) {
                return Err(CliError::Daemon(format!(
                    "server did not become ready at {} within {:?}; check logs at {}",
                    self.bind,
                    self.ready_timeout,
                    self.log_path().display()
                )));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// One readiness probe. `Http` means a real grove daemon — `GET /api/health`
    /// must return 2xx (the boot state is `ready`), so we never report ready on a
    /// merely-open socket. `Tcp` (tests) just checks the port accepts a connection.
    fn is_ready(&self) -> bool {
        match self.interface {
            Interface::Http => self.api_client().health_ok(),
            Interface::Tcp => TcpStream::connect_timeout(&self.bind, PROBE_TIMEOUT).is_ok(),
        }
    }

    /// Block until `pid` is gone or `timeout` elapses. Returns `true` if it exited.
    fn await_exit(&self, pid: Pid, timeout: Duration) -> bool {
        self.poll_until(timeout, || self.has_exited(pid))
    }

    /// The shared shape of every wait in this file: poll `done` until it holds or
    /// the budget runs out. The budget is a [`Deadline`](grove_ops::clock::Deadline)
    /// off the injected clock, never an inline wall-clock read — that is what makes
    /// a "30 s ready timeout" testable without waiting 30 seconds for it.
    fn poll_until(&self, timeout: Duration, done: impl Fn() -> bool) -> bool {
        let deadline = self.clock.deadline(timeout);
        loop {
            if done() {
                return true;
            }
            if deadline.expired(&*self.clock) {
                return false;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Whether `pid` is gone. Reaps first: a fake child under the test process
    /// becomes a zombie on kill and would otherwise answer `kill(0)` alive
    /// forever; `waitpid(WNOHANG)` reaps it and reports exit. Production's daemon
    /// is reparented to init (`waitpid` → `ECHILD`), so we fall back to the pid
    /// probe there.
    fn has_exited(&self, pid: Pid) -> bool {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => false,
            Ok(_) => true, // exited/signaled — reaped, gone
            Err(_) => matches!((self.pid_probe)(pid), PidState::Dead),
        }
    }
}

/// The launcher: the installed release's binary when one is laid down, else this
/// very executable.
///
/// It reads the *install* root, not the workspace, because `current` is the symlink
/// `grove up` flips and every version behind it is disposable release state — the
/// one thing the workspace must never hold.
///
/// v1 launched a *separate* Mix release (`current/bin/grove_server start`) and had
/// nothing to fall back on, so `grove on` before an install was an error. v2 ships
/// one binary, so a dev or freshly-built grove can start its own daemon; `current`
/// still wins when present, because that is the path `grove up` flips and the
/// version the health gate expects to see answer.
fn launcher(install: &Path) -> PathBuf {
    let installed = install.join("current/bin/grove");
    if installed.exists() {
        return installed;
    }
    std::env::current_exe().unwrap_or(installed)
}

/// Resolve `GROVE_BIND` to a socket address. A literal `host:port` parses
/// directly; anything else (e.g. `localhost:8080`) goes through name resolution
/// so the CLI and the API client agree on the target. An absent value is the
/// default; an unresolvable one is an error — a silent fallback to the default
/// would send `on`/`off` to a different address than the API client uses.
///
/// Note the asymmetry with the daemon's own [`grove_daemon::parse_bind`], which
/// refuses a hostname outright: the daemon decides what to *expose* and must not
/// let a resolver choose that; the CLI only has to reach whatever is listening.
fn resolve_bind(raw: Option<String>) -> Result<SocketAddr, CliError> {
    let Some(raw) = raw else {
        return Ok(grove_daemon::DEFAULT_BIND);
    };
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Ok(addr);
    }
    raw.to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
        .ok_or_else(|| {
            CliError::Daemon(format!(
                "GROVE_BIND {raw:?} is not a valid socket address or resolvable host:port"
            ))
        })
}

/// Probe a pid's liveness + identity (see [`PidState`]). `EPERM` means a live
/// process owned by another user — reported `Foreign`, never `Dead`, so its pid
/// file is preserved. Any other `kill` error is treated as gone.
fn probe_pid(pid: Pid) -> PidState {
    match signal::kill(pid, None) {
        Ok(()) => PidState::Alive(process_start_time(pid)),
        Err(Errno::EPERM) => PidState::Foreign,
        Err(_) => PidState::Dead,
    }
}

/// A pid's start time as an opaque identity token — stable for the life of the
/// process, distinct across a recycled pid. `None` when the platform probe is
/// unavailable, in which case identity can't be verified (liveness only).
#[cfg(target_os = "linux")]
fn process_start_time(pid: Pid) -> Option<u64> {
    // /proc/<pid>/stat field 22 (starttime), in clock ticks since boot. `comm`
    // (field 2) may contain spaces and ')', so split after the final ')'.
    let stat = fs::read_to_string(format!("/proc/{}/stat", pid.as_raw())).ok()?;
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().nth(19)?.parse::<u64>().ok()
}

/// Darwin: no `/proc`, and the `sysctl` FFI would need `unsafe` (forbidden
/// workspace-wide), so read the kernel-reported start time via `ps`. `lstart` is
/// an absolute, per-process-stable timestamp — a recycled pid started later
/// yields a different string — hashed to the token with FNV-1a, NOT
/// `DefaultHasher`: the token is written by one binary and compared by a
/// *different* binary after self-update, and `DefaultHasher`'s algorithm is
/// documented as unstable across Rust releases (a rustc bump would read every
/// live server as recycled). FNV-1a is fixed by its constants.
#[cfg(target_os = "macos")]
fn process_start_time(pid: Pid) -> Option<u64> {
    let out = ps_start_command(pid).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    (!s.is_empty()).then(|| fnv1a_64(s.as_bytes()))
}

/// FNV-1a, 64-bit — build-stable by construction (offset basis + prime are the
/// spec). Not for security; only a cheap, deterministic fingerprint of `lstart`.
#[cfg(target_os = "macos")]
fn fnv1a_64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The `ps` invocation behind [`process_start_time`], with `LC_ALL`/`TZ` pinned.
/// The token is written by `grove on` and re-read by a *later, separate* CLI
/// process (`off`, the `up` bounce, the `on` conflict check); `lstart`'s string
/// is locale- and timezone-dependent, so without pinning a server started under
/// one `LC_TIME`/`TZ` (e.g. a launchd boot in `C`) would hash differently when
/// read from a localized Terminal — the same live pid would look recycled and
/// its pid file would be wrongly deleted. Forcing a canonical `C`/`UTC` on both
/// writer and reader makes the string deterministic.
#[cfg(target_os = "macos")]
fn ps_start_command(pid: Pid) -> Command {
    let mut cmd = Command::new("/bin/ps");
    cmd.args(["-o", "lstart=", "-p", &pid.as_raw().to_string()])
        .env("LC_ALL", "C")
        .env("TZ", "UTC");
    cmd
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_start_time(_pid: Pid) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_ops::clock::TestClock;
    use std::net::TcpListener;
    use tempfile::TempDir;

    /// SIGKILLs a spawned fake child on drop, so a failing assert never leaks it.
    struct KillGuard(Option<Pid>);
    impl Drop for KillGuard {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                let _ = signal::kill(pid, Signal::SIGKILL);
            }
        }
    }

    /// A `ServerControl` wired to a temp home + a fake long-lived child + short
    /// timeouts. `/bin/sleep` is the fake server (POSIX; unix-only target). The
    /// `Tcp` interface probes the port directly — no HTTP server to stand up.
    ///
    /// The fake install root is a `install/` subdirectory of the same `TempDir`: these
    /// ladders never launch the installed binary (`server_program` is the fake), so
    /// the only thing the root has to be is *distinct from the home* — which is the
    /// property [`the_child_command_carries_the_resolved_bind_and_both_roots`] reads.
    ///
    /// The clock is the real one: these ladders poll a *live* process, so a frozen
    /// clock would spin forever rather than fail fast. The seam is exercised on its
    /// own in `a_ready_deadline_expires_on_the_injected_clock`.
    fn control(home: &TempDir, bind: SocketAddr) -> ServerControl {
        ServerControl {
            home: home.path().to_path_buf(),
            install: home.path().join("install"),
            bind,
            server_program: PathBuf::from("/bin/sleep"),
            server_args: vec!["600".to_string()],
            interface: Interface::Tcp,
            ready_timeout: Duration::from_secs(2),
            // Short: `off` reaps the fake child (a child of the test process) via
            // waitpid, so a kill is observed promptly and these rarely run their
            // full duration.
            stop_grace: Duration::from_millis(50),
            kill_timeout: Duration::from_millis(50),
            pid_probe: probe_pid,
            clock: Arc::new(SystemClock),
        }
    }

    /// An ephemeral port with nothing listening (listener bound then dropped).
    fn closed_port() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }

    /// A throwaway HTTP server that answers the next request with `200 OK` — the
    /// minimum for the `Http` readiness probe to succeed.
    fn http_ok_server() -> SocketAddr {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for mut s in listener.incoming().take(2).flatten() {
                let _ = s.read(&mut [0u8; 1024]);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
            }
        });
        addr
    }

    /// A throwaway HTTP server that answers `200 OK` to each request until it
    /// sees a `POST` (the drain), after which it drops its listener so the port
    /// stops accepting — models a pid-less server stopping on `POST
    /// /api/daemon/shutdown`.
    fn http_shutdown_server() -> SocketAddr {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).unwrap_or(0);
                // A grove-shaped envelope: `identifies_as_grove` gates the
                // pid-less drain on it before any shutdown POST is sent.
                let body = br#"{"ok":true,"data":{"status":"ready"}}"#;
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = s.write_all(body);
                if buf[..n].starts_with(b"POST") {
                    break; // shutdown accepted → stop accepting connections
                }
            }
        });
        addr
    }

    #[test]
    fn http_interface_reports_ready_on_200() {
        let home = TempDir::new().unwrap();
        let sc = ServerControl {
            interface: Interface::Http,
            ..control(&home, http_ok_server())
        };
        assert!(sc.is_ready(), "GET /api/health → 200 means ready");
    }

    #[test]
    fn http_interface_not_ready_when_unreachable() {
        let home = TempDir::new().unwrap();
        let sc = ServerControl {
            interface: Interface::Http,
            ..control(&home, closed_port())
        };
        assert!(!sc.is_ready(), "nothing listening → not ready");
    }

    #[test]
    fn running_pid_reports_live_and_clears_stale() {
        let home = TempDir::new().unwrap();
        let sc = control(&home, closed_port());

        assert!(sc.running_pid().is_none(), "no pid file yet");

        fs::write(sc.pid_path(), std::process::id().to_string()).unwrap();
        assert!(sc.running_pid().is_some(), "our own pid is alive");

        fs::write(sc.pid_path(), "999999999").unwrap();
        assert!(sc.running_pid().is_none(), "dead pid → None");
        assert!(!sc.pid_path().exists(), "stale pid file is cleared");
    }

    #[test]
    fn on_starts_fake_then_off_stops_it() {
        let home = TempDir::new().unwrap();
        // The test owns a listener on the bind port, so readiness connect succeeds.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap();
        let sc = control(&home, bind);

        sc.on().expect("on starts the fake server");
        let _guard = KillGuard(sc.running_pid());
        assert!(sc.running_pid().is_some(), "pid file names a live process");

        sc.off().expect("off stops it");
        assert!(sc.running_pid().is_none());
        assert!(!sc.pid_path().exists(), "pid file removed on stop");
    }

    /// The start lock must not travel into the daemon we launch. It did in v1: the
    /// `flock` lives on the open file description, the spawned child inherits it,
    /// and the *second* `grove on` then blocks on `flock` forever rather than
    /// reporting the conflict — a hang, on the machine's own `grove on`, with no
    /// diagnostic. `O_CLOEXEC` closes it; this test is what proves the fd is gone.
    #[test]
    fn a_second_on_conflicts_instead_of_blocking_on_the_leaked_lock() {
        let home = TempDir::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sc = control(&home, listener.local_addr().unwrap());

        sc.on().expect("the first start succeeds");
        let _guard = KillGuard(sc.running_pid());

        // Deadlocks here if the child kept the lock; nextest's slow-timeout is the
        // backstop, and a hang is exactly the failure being pinned.
        let err = sc.on().unwrap_err();
        assert_eq!(err.exit_code(), 5, "already running → Conflict, not a hang");

        sc.off().unwrap();
    }

    #[test]
    fn running_pid_treats_a_recycled_pid_as_stale() {
        let home = TempDir::new().unwrap();
        // Live pid, but a start time that differs from the recorded one → recycled.
        let sc = ServerControl {
            pid_probe: |_| PidState::Alive(Some(222)),
            ..control(&home, closed_port())
        };
        fs::write(sc.pid_path(), format!("{}\nv2:111\n", std::process::id())).unwrap();
        assert!(sc.running_pid().is_none(), "start-time mismatch → stale");
        assert!(!sc.pid_path().exists(), "a recycled pid file is cleared");
    }

    #[test]
    fn running_pid_matches_on_recorded_identity() {
        let home = TempDir::new().unwrap();
        let sc = ServerControl {
            pid_probe: |_| PidState::Alive(Some(111)),
            ..control(&home, closed_port())
        };
        fs::write(sc.pid_path(), format!("{}\nv2:111\n", std::process::id())).unwrap();
        assert!(sc.running_pid().is_some(), "matching start time → live");
    }

    #[test]
    fn running_pid_treats_a_legacy_format_token_as_liveness_only() {
        let home = TempDir::new().unwrap();
        // A pre-v2 pid file (bare-number token from an older binary) whose live
        // process probes with a DIFFERENT token value: the derivation changed
        // underneath it, not the process. Identity must degrade to liveness-only
        // — live is live; the file survives. (Deleting it here is the self-update
        // failure mode: the fresh binary would orphan the running server.)
        let sc = ServerControl {
            pid_probe: |_| PidState::Alive(Some(222)),
            ..control(&home, closed_port())
        };
        fs::write(sc.pid_path(), format!("{}\n111\n", std::process::id())).unwrap();
        assert!(sc.running_pid().is_some(), "legacy token → liveness-only");
        assert!(
            sc.pid_path().exists(),
            "a live legacy pid file is not cleared"
        );
    }

    #[test]
    fn running_pid_treats_eperm_as_running() {
        let home = TempDir::new().unwrap();
        // EPERM: a live process owned by another user. Must count as running and
        // its pid file must survive (a live process still holds the port).
        let sc = ServerControl {
            pid_probe: |_| PidState::Foreign,
            ..control(&home, closed_port())
        };
        fs::write(sc.pid_path(), "4242").unwrap();
        assert!(sc.running_pid().is_some(), "EPERM → alive");
        assert!(
            sc.pid_path().exists(),
            "a foreign live pid file is not cleared"
        );
    }

    #[test]
    fn process_start_time_is_stable_for_a_live_pid() {
        // The identity token must be deterministic across reads of the same live
        // process — a token that drifts would make a live server look recycled.
        let me = Pid::from_raw(std::process::id().cast_signed());
        let a = process_start_time(me);
        let b = process_start_time(me);
        assert!(a.is_some(), "our own start time is probeable");
        assert_eq!(a, b, "same live pid → same token");
    }

    /// The darwin `ps` probe must pin `LC_ALL`/`TZ`: `lstart`'s string is locale-
    /// and timezone-dependent, and the token is written and re-read by separate
    /// CLI invocations that may run under different environments.
    #[cfg(target_os = "macos")]
    #[test]
    fn ps_start_command_pins_locale_and_timezone() {
        use std::ffi::OsStr;
        let cmd = ps_start_command(Pid::from_raw(1));
        let envs: Vec<_> = cmd.get_envs().collect();
        let pinned = |k, want| {
            envs.iter()
                .any(|(key, val)| *key == OsStr::new(k) && *val == Some(OsStr::new(want)))
        };
        assert!(pinned("LC_ALL", "C"), "LC_ALL pinned to C");
        assert!(pinned("TZ", "UTC"), "TZ pinned to UTC");
    }

    #[test]
    fn off_reports_failure_when_the_process_survives() {
        let home = TempDir::new().unwrap();
        // A pid that never dies (probe always Alive) and isn't our child
        // (waitpid → ECHILD → falls back to the probe). SIGTERM/SIGKILL to this
        // long-dead pid number are no-ops.
        let sc = ServerControl {
            pid_probe: |_| PidState::Alive(None),
            ..control(&home, closed_port())
        };
        fs::write(sc.pid_path(), "999999999").unwrap();

        let err = sc.off().unwrap_err();
        assert_eq!(err.exit_code(), 4, "survived SIGKILL → Daemon failure");
        assert!(
            sc.pid_path().exists(),
            "pid file kept when the process won't die"
        );
    }

    #[test]
    fn on_conflicts_when_already_running() {
        let home = TempDir::new().unwrap();
        let sc = control(&home, closed_port());
        fs::write(sc.pid_path(), std::process::id().to_string()).unwrap();

        let err = sc.on().unwrap_err();
        assert_eq!(err.exit_code(), 5, "Conflict");
    }

    #[test]
    fn on_times_out_when_never_ready() {
        let home = TempDir::new().unwrap();
        let sc = ServerControl {
            ready_timeout: Duration::from_millis(300),
            ..control(&home, closed_port())
        };

        let err = sc.on().unwrap_err();
        let _guard = KillGuard(sc.running_pid()); // on() spawned the fake before readiness failed
        assert_eq!(err.exit_code(), 4, "Daemon (readiness timeout)");
    }

    /// The clock seam under the readiness ladder: with a hand-driven clock the
    /// budget expires only when the test says so, so a 30-second production timeout
    /// is testable in microseconds. `/bin/sleep` never opens the port, so the only
    /// way out of the loop is the deadline.
    #[test]
    fn a_ready_deadline_expires_on_the_injected_clock() {
        let home = TempDir::new().unwrap();
        let clock = Arc::new(TestClock::new());
        let sc = ServerControl {
            ready_timeout: Duration::from_secs(30),
            clock: clock.clone(),
            ..control(&home, closed_port())
        };
        let ticker = clock.clone();
        // Advance from the side: the ladder polls a live child, so the clock has to
        // move while the loop is running.
        let hand = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            ticker.advance(Duration::from_secs(31));
        });

        let err = sc.on().unwrap_err();
        let _guard = KillGuard(sc.running_pid());
        hand.join().unwrap();

        assert_eq!(err.exit_code(), 4);
        assert!(err.to_string().contains("did not become ready"));
    }

    #[test]
    fn on_errors_when_no_server_installed() {
        let home = TempDir::new().unwrap();
        let sc = ServerControl {
            server_program: PathBuf::from("/no/such/grove"),
            ..control(&home, closed_port())
        };

        let err = sc.on().unwrap_err();
        assert_eq!(err.exit_code(), 4, "Daemon (no binary to launch)");
        assert!(sc.running_pid().is_none(), "nothing spawned");
    }

    #[test]
    fn off_is_ok_when_not_running() {
        let home = TempDir::new().unwrap();
        let sc = control(&home, closed_port());
        assert!(sc.off().is_ok());
    }

    #[test]
    fn reboot_yields_a_fresh_process() {
        let home = TempDir::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap();
        let sc = control(&home, bind);

        sc.on().unwrap();
        let first = sc.running_pid();
        let _g1 = KillGuard(first);
        assert!(first.is_some());

        sc.reboot().unwrap();
        let second = sc.running_pid();
        let _g2 = KillGuard(second);
        assert!(second.is_some());
        assert_ne!(first, second, "reboot restarts under a new pid");

        sc.off().unwrap();
    }

    #[test]
    fn on_fails_when_the_child_dies_immediately() {
        let home = TempDir::new().unwrap();
        // A server that exits at once (EADDRINUSE, crash, …). Readiness must gate
        // on OUR child staying alive — never report success off a foreign 200 and
        // record the corpse's pid.
        let sc = ServerControl {
            server_args: vec!["0".to_string()], // `/bin/sleep 0` → immediate exit
            ready_timeout: Duration::from_secs(5),
            ..control(&home, closed_port())
        };
        let err = sc.on().unwrap_err();
        assert_eq!(err.exit_code(), 4, "child exited immediately → Daemon");
        assert!(err.to_string().contains("exited immediately"));
        assert!(
            !sc.pid_path().exists(),
            "the dead child's pid file is removed"
        );
    }

    #[test]
    fn off_drains_a_pidless_server_over_the_api() {
        let home = TempDir::new().unwrap();
        // A server answering on the port but with NO pid file (served mode: a
        // supervisor owns the process). `off` must drain it over the API, not
        // no-op — otherwise a served-mode self-update bounce can never land.
        let sc = ServerControl {
            interface: Interface::Http,
            ..control(&home, http_shutdown_server())
        };
        assert!(
            sc.running_pid().is_none(),
            "no pid file for a supervised server"
        );
        sc.off().expect("a pid-less server drains via the API");
    }

    #[test]
    fn off_is_a_no_op_when_a_stranger_holds_the_bind() {
        use std::io::{Read, Write};
        let home = TempDir::new().unwrap();
        // An unrelated HTTP service on the bind (no grove envelope): `off` must
        // keep its "not running → Ok" contract, never POST a shutdown at it.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).unwrap_or(0);
                assert!(
                    !buf[..n].starts_with(b"POST"),
                    "a stranger must never receive the shutdown POST"
                );
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello");
            }
        });
        let sc = ServerControl {
            interface: Interface::Http,
            ..control(&home, addr)
        };
        sc.off()
            .expect("a foreign listener is a no-op, not an error");
    }

    /// The pid-ful mirror. A pid file and the bind can disagree — `GROVE_BIND` moved
    /// since `grove on`, or the daemon lost its listener and a local service took the
    /// port — and the drain is gated on identity there too: the stranger receives no
    /// shutdown POST, and `off` falls straight through to the signal ladder for the
    /// process it actually owns. Counted in the main thread, not asserted in the
    /// listener's: a panic over there fails no test.
    #[test]
    fn off_never_drains_a_stranger_holding_the_bind_of_a_live_pid() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let home = TempDir::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let posts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&posts);
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).unwrap_or(0);
                if buf[..n].starts_with(b"POST") {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello");
            }
        });

        let sc = ServerControl {
            interface: Interface::Http,
            ..control(&home, addr)
        };
        // A daemon we own that is not the thing on the bind. A bare `<pid>` pid file
        // is what `launch` writes when no start-time probe is available.
        let mut child = Command::new("/bin/sleep").arg("600").spawn().unwrap();
        let pid = Pid::from_raw(child.id().cast_signed());
        let _guard = KillGuard(Some(pid));
        fs::write(sc.pid_path(), child.id().to_string()).unwrap();

        sc.off().expect("the ladder stops the process we own");

        assert_eq!(
            posts.load(Ordering::SeqCst),
            0,
            "a stranger must never receive the shutdown POST"
        );
        assert!(sc.has_exited(pid), "the signal reached our own process");
        assert!(!sc.pid_path().exists(), "pid file removed on stop");
        let _ = child.wait(); // ECHILD: `off`'s own waitpid already reaped it
    }

    #[test]
    fn on_refuses_when_the_bind_is_already_held_without_a_pid() {
        let home = TempDir::new().unwrap();
        // A listener on the bind and NO pid file: spawning is doomed to
        // EADDRINUSE, and the squatter's 200 could answer our readiness poll
        // during the child's boot window — `on` must refuse up front.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let sc = ServerControl {
            interface: Interface::Http,
            ..control(&home, addr)
        };
        let err = sc.on().unwrap_err();
        assert_eq!(err.exit_code(), 5, "held bind → Conflict");
        assert!(err.to_string().contains("already listening"));
        drop(listener);
    }

    #[test]
    fn off_errors_on_a_pidless_server_that_wont_drain() {
        let home = TempDir::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap();
        // Bound at the port, no pid file, and the drain doesn't take (the `Tcp`
        // interface never issues the shutdown). With no pid to signal, `off` must
        // surface a loud error rather than claim success over a live server.
        let sc = control(&home, bind);
        let err = sc.off().unwrap_err();
        assert_eq!(err.exit_code(), 4, "bound but undrainable → Daemon");
        assert!(err.to_string().contains("no pid file"));
    }

    #[test]
    fn escalate_skips_a_recycled_pid() {
        let home = TempDir::new().unwrap();
        // Alive, but under a start time that no longer matches the recorded one —
        // the pid was recycled to an unrelated process inside the grace window.
        // `escalate` must report it gone WITHOUT signalling the stranger.
        let sc = ServerControl {
            pid_probe: |_| PidState::Alive(Some(999)),
            ..control(&home, closed_port())
        };
        fs::write(sc.pid_path(), "4242\nv2:111\n").unwrap();
        assert!(
            sc.escalate(
                Pid::from_raw(4242),
                Some(111),
                Signal::SIGKILL,
                sc.kill_timeout
            ),
            "a recycled pid reads as already exited"
        );
        assert!(!sc.pid_path().exists(), "the recycled pid file is cleared");
    }

    #[test]
    fn resolve_bind_parses_literal_resolves_host_and_rejects_garbage() {
        // Absent → the daemon's default bind, shared with it rather than re-typed.
        assert_eq!(
            resolve_bind(None).unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 7777))
        );
        // A literal socket address parses directly.
        assert_eq!(
            resolve_bind(Some("127.0.0.1:8080".into())).unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 8080))
        );
        // A resolvable host:port resolves rather than silently defaulting — this
        // is the case that used to split `on`/`off` from the API client.
        let resolved = resolve_bind(Some("localhost:8080".into())).unwrap();
        assert!(resolved.ip().is_loopback() && resolved.port() == 8080);
        // Garbage is a loud error, never a fallback to the default.
        assert!(resolve_bind(Some("not a bind".into())).is_err());
    }

    /// The v2 launcher contract: `<grove> serve`, pointed at the installed release
    /// when one is laid down and at this very binary when none is.
    #[test]
    fn the_launcher_prefers_an_installed_release_then_falls_back_to_this_binary() {
        let install = TempDir::new().unwrap();
        let fallback = launcher(install.path());
        assert_eq!(
            fallback,
            std::env::current_exe().unwrap(),
            "no install → run our own binary"
        );

        let installed = install.path().join("current/bin");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("grove"), "#!/bin/sh\n").unwrap();
        assert_eq!(
            launcher(install.path()),
            installed.join("grove"),
            "an installed release wins — it is what `grove up` flips"
        );
    }

    /// The child inherits the resolved bind and *both* roots the CLI decided on: a
    /// hostname the daemon would refuse never reaches it, the child realizes the same
    /// manifest the operator is looking at, and its own `grove up` flips the same
    /// install this CLI resolved rather than one it re-derives from its environment.
    #[test]
    fn the_child_command_carries_the_resolved_bind_and_both_roots() {
        use std::ffi::OsStr;
        let home = TempDir::new().unwrap();
        let sc = control(&home, SocketAddr::from(([127, 0, 0, 1], 7777)));
        let cmd = sc.server_command().unwrap();

        let envs: Vec<_> = cmd.get_envs().collect();
        let value = |k| {
            envs.iter()
                .find(|(key, _)| *key == OsStr::new(k))
                .and_then(|(_, v)| *v)
        };
        assert_eq!(value("GROVE_BIND"), Some(OsStr::new("127.0.0.1:7777")));
        assert_eq!(value("GROVE_HOME"), Some(home.path().as_os_str()));
        assert_eq!(
            value("GROVE_INSTALL"),
            Some(home.path().join("install").as_os_str()),
            "the install root travels too, and is not the home"
        );
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("600")],
            "the args are a field, so the fake child is injectable"
        );
    }
}
