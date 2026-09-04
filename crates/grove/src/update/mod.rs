//! Self-update: **versioned dirs + an atomic symlink flip**. The updater installs
//! into immutable `versions/<v>/` directories; only the `current` symlink ever moves,
//! and a `previous` symlink makes rollback a single flip. No sentinel state machine,
//! no in-place swap, no orphan trampoline — a failed download or extract leaves
//! `current` untouched, so a broken release can never take a server down.
//!
//! ```text
//! $GROVE_INSTALL/                       # ~/.local/share/grove — the install root
//! ├── versions/0.3.1/bin/grove          # immutable after write
//! ├── versions/0.4.0/bin/grove
//! ├── current  → versions/0.4.0         # the only mutation point (rename(2))
//! ├── previous → versions/0.3.1         # rollback target
//! └── pending                           # a flip whose health gate has not answered
//! ```
//!
//! That root is the *install*, not the workspace: it holds only what a release
//! lays down, so it is disposable and regenerable, while `$GROVE_HOME` holds the
//! manifest and every checkout under `code/` and is not. Both are resolved once,
//! in `grove-ops` — see [`grove_ops::install_home`] and [`grove_ops::home`].
//!
//! [`Layout`] is the pure on-disk mechanism (no network, fully unit-tested).
//! [`Updater`] orchestrates a `grove up`: fetch → verify → install → flip → bounce →
//! version-aware health gate → prune or auto-rollback. Its side effects (where
//! bundles come from, how the server restarts, how health is probed) are injected
//! seams, so the whole flow is testable against local fixtures.
//!
//! # The flip and its gate
//!
//! v1 flipped, then gated, and held the verdict nowhere but in the updater's own
//! memory: a crash in between stranded `current` on a version nothing had ever
//! proven, recoverable only by an operator who knew to run `grove up` again. v2 keeps
//! the flip *first* — the alternative, gating before the flip, cannot be expressed at
//! all in served mode, where the bounce hands off to a supervisor that restarts
//! whatever `current` points at — and instead makes the owed verdict **durable**:
//! [`Layout::flip_to`] writes a `pending` marker naming the version **and the
//! direction** before `current` moves, and the updater clears it the moment the gate
//! answers, whatever it answers. A marker still naming `current` at the start of the
//! next `grove up` is therefore proof that a previous updater died mid-update, and
//! [`recover_unproven_flip`](Updater::recover_unproven_flip) settles it before going
//! forward. The gate itself, and every rollback path below it, is unchanged.
//!
//! The direction is not decoration. **Both** movers of `current` mark a flip pending —
//! a rollback owes a gate exactly as a forward flip does — and the rollback's marker
//! names the version that was just *proven*, so a recovery that read it as a forward
//! flip would undo the rollback and land back on the release the gate had rejected,
//! then report "already on `<v>`" and exit 0 with no bounce at all.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use grove_ops::clock::{Clock, SystemClock};

use crate::api::Reachability;
use crate::{ApiClient, CliError, ServerControl, timeouts};

mod layout;
mod source;

pub use layout::Layout;
use layout::{Flip, KEEP_VERSIONS, valid_version};
pub use source::{BundleSource, host_target};
use source::{
    DEFAULT_CHANNEL, install_source, read_persisted_channel, supported_host_target, verify_sha256,
};

/// Whether a bounce should short-circuit to [`Bounce::NotRunning`] — skipping the
/// health-gate. Only when there's no running pid, we aren't force-starting, AND
/// nothing answers `/api/health`. A pid-less server that still answers is managed
/// externally, so it must be gated (and auto-rolled-back) like any other.
fn should_skip_bounce(has_pid: bool, reachable: Reachability, ensure_running: bool) -> bool {
    !has_pid && !ensure_running && reachable == Reachability::Offline
}

/// The production bounce pipeline behind [`Updater::from_env`], with the probes
/// and actions as seams: decide via [`should_skip_bounce`], restart, then gate on
/// readiness. Extracted so tests exercise the predicate→action WIRING — the
/// external-server case (no pid file, health answering) must reach `restart` +
/// the gate, and only a fully-stopped server may short-circuit — rather than only
/// the predicate's truth table.
fn gated_bounce(
    has_pid: bool,
    reachable: Reachability,
    ensure_running: bool,
    restart: impl FnOnce() -> Result<(), CliError>,
    healthy: impl FnOnce() -> bool,
) -> Result<Bounce, CliError> {
    // No pid file doesn't always mean "nothing to gate": a server managed by an
    // external supervisor answers /api/health without one. Skip the gate only
    // when nothing answers at all (a fully-stopped server).
    if should_skip_bounce(has_pid, reachable, ensure_running) {
        return Ok(Bounce::NotRunning);
    }
    restart()?;
    // Gate on a real 200/ready reporting the expected version: a draining old
    // server answers 503, and a stale process on the port reports a different
    // version — neither may count as the new version being healthy.
    Ok(if healthy() {
        Bounce::Healthy
    } else {
        Bounce::Unhealthy
    })
}

/// What [`Updater::recover_unproven_flip`] settled, which decides whether `up` may
/// still take its "already on `<v>`" shortcut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recovery {
    /// Nothing was owed, or the marker named a flip that never landed.
    None,
    /// `current` was moved off the version nothing had proven (or had nowhere to move
    /// to). The forward path must run, gate and all.
    Undid,
    /// An interrupted rollback: nothing flipped — `current` already named the proven
    /// version — and the gate it was owed has just been re-run here.
    Regated,
}

/// Outcome of bouncing the server onto the freshly-flipped version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bounce {
    /// No server was running — nothing to restart; the next `grove on` picks up
    /// the new version. (First install, or updating an offline server.)
    NotRunning,
    /// Restarted and reported healthy.
    Healthy,
    /// Restarted but did not become healthy — triggers rollback.
    Unhealthy,
}

/// Restart the server onto `current` and report health, gating on it coming back
/// as the given version. Args: `(expected_version, ensure_running)`. Injected so
/// the update flow is testable against fixtures.
type BounceFn = Box<dyn Fn(&str, bool) -> Result<Bounce, CliError>>;

/// Orchestrates a `grove up`. Both side effects are injected so the flow is
/// testable against local fixtures: `source` (where bundles come from) and
/// `bounce` (restart the server, if running, and report whether it came back
/// healthy).
pub struct Updater {
    layout: Layout,
    target: String,
    source: BundleSource,
    keep: usize,
    bounce: BounceFn,
    channel: String,
}

impl Updater {
    /// `bounce(expected_version, ensure_running)` restarts the server onto
    /// `current` and reports health, gating on the server coming back **as
    /// `expected_version`**. `ensure_running = false` (a normal update) leaves a
    /// stopped server stopped → `NotRunning`; `true` (recovering after a rollback)
    /// starts it even if it wasn't running.
    pub fn new(
        layout: Layout,
        target: impl Into<String>,
        source: BundleSource,
        bounce: impl Fn(&str, bool) -> Result<Bounce, CliError> + 'static,
    ) -> Self {
        Self {
            layout,
            target: target.into(),
            source,
            keep: KEEP_VERSIONS,
            bounce: Box::new(bounce),
            channel: DEFAULT_CHANNEL.to_string(),
        }
    }

    /// Override the channel `up(None)` resolves against — the `grove up --channel`
    /// flag. `None` leaves the env/file-derived channel from [`from_env`] intact.
    ///
    /// [`from_env`]: Self::from_env
    #[must_use]
    pub fn with_channel(mut self, channel: Option<&str>) -> Self {
        if let Some(c) = channel {
            self.channel = c.to_string();
        }
        self
    }

    /// Production wiring: bundles from this repo's GitHub Releases (or
    /// `GROVE_INSTALL_BASE_URL`), and a bounce that restarts the server — standalone
    /// reboots it in place, served hands off to the supervisor — then polls
    /// `/api/health` for **ready at the expected version**. The channel follows
    /// `GROVE_CHANNEL` → the persisted `$GROVE_INSTALL/channel` → `stable`.
    ///
    /// Everything this reads and writes hangs off the *install* root, never the
    /// workspace `$GROVE_HOME`: the layout, the channel file and the `update.lock`
    /// are all release state, and pointing them at the workspace is what made
    /// `uninstall.sh` unable to delete one without the other.
    ///
    /// What restarts is the v2 launcher contract: `ServerControl` runs
    /// `$GROVE_INSTALL/current/bin/grove serve` whenever an installed release is laid
    /// down — the very path the flip above just moved — and falls back to the running
    /// executable only when nothing is installed.
    pub fn from_env() -> Result<Self, CliError> {
        // First, and before anything is resolved or fetched: a platform the release
        // line publishes no bundle for is refused here, the way `install.sh` refuses
        // it before its first request — not several seconds later as a 404 on an
        // asset URL that was never built.
        let target = supported_host_target()?;
        let source = install_source(std::env::var("GROVE_INSTALL_BASE_URL").ok())?;

        let install = crate::grove_install_home();
        let channel = std::env::var("GROVE_CHANNEL")
            .ok()
            .or_else(|| read_persisted_channel(&install))
            .unwrap_or_else(|| DEFAULT_CHANNEL.to_string());

        let supervised = matches!(std::env::var("GROVE_MODE").as_deref(), Ok("served"));
        let gate = health_gate(std::env::var("GROVE_HEALTH_GATE_SECS").ok());
        // Resolve the server control NOW: a bad GROVE_BIND is a pure config error
        // and must fail before anything is downloaded or flipped — surfacing it
        // from inside the bounce would misread it as "the new version failed to
        // start" and trigger a rollback of a perfectly good release.
        let server = ServerControl::from_env()?;
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let bounce = move |expected: &str, ensure_running: bool| {
            gated_bounce(
                server.running_pid().is_some(),
                ApiClient::from_env().reachable(),
                ensure_running,
                || {
                    if supervised {
                        // Stop; the substrate supervisor restarts it on `current`.
                        server.off()
                    } else {
                        server.reboot() // off+on — starts even if it wasn't running
                    }
                },
                || poll_ready(&*clock, gate, expected),
            )
        };

        let mut updater = Self::new(Layout::new(install), target, source, bounce);
        updater.channel = channel;
        Ok(updater)
    }

    /// `grove up [--version V]`. Resolves the target (flag or the channel), installs
    /// it beside `current`, flips, and bounces a running server. A new version that
    /// boots unhealthy — or whose restart errors outright — is rolled back.
    // stele:landmark self-update-flip
    pub fn up(&self, version: Option<&str>) -> Result<(), CliError> {
        let _lock = self.layout.lock()?; // serialize concurrent `grove up`
        // Before anything else, and inside the lock: an update that died between its
        // flip and its gate left `current` on an unproven version, and going forward
        // from there would stack a second unproven flip on top of it.
        let recovered = self.recover_unproven_flip()?;

        let version = match version {
            Some(v) => v.to_string(),
            None => self.source.channel_version(&self.channel)?,
        };
        if !valid_version(&version) {
            return Err(CliError::Update(format!(
                "invalid version {version:?}: expected an alphanumeric/.-+ identifier"
            )));
        }

        // The idempotence shortcut, and the one state it must NOT fire in: recovery
        // just moved `current` back, so what is running may still be the version it
        // undid. Exiting 0 here would launder an interrupted update into "already on
        // <v>" with no bounce and no gate — the worst available outcome, since the
        // version it reports is the one nothing has proven. `Regated` is safe: that
        // arm has already re-run the gate against `current`.
        if recovered != Recovery::Undid
            && self.layout.current_version().as_deref() == Some(version.as_str())
        {
            println!("grove is already on {version}");
            return Ok(());
        }

        let (bytes, want_sha) = self.source.fetch(&version, &self.target)?;
        verify_sha256(&bytes, &want_sha)?;
        self.layout.install_bundle(&version, &bytes)?;
        self.layout.flip_to(&version)?; // marks the flip pending

        let verdict = (self.bounce)(&version, false);
        // The gate answered — healthy, unhealthy, or errored outright. `current` is
        // no longer *unproven* whichever way it went, so the marker's job is done;
        // only a crash before this line leaves one behind for the next `grove up`.
        self.layout.clear_pending();

        match verdict {
            Ok(Bounce::NotRunning) => {
                println!("grove updated to {version} (server not running; `grove on` to start it)");
                self.prune_quietly();
                Ok(())
            }
            Ok(Bounce::Healthy) => {
                println!("grove updated to {version}");
                // Prune is housekeeping — a failure here must NOT fail a landed,
                // healthy update (that would misreport success as exit 7).
                self.prune_quietly();
                Ok(())
            }
            Ok(Bounce::Unhealthy) => self.roll_back(&version, None),
            // The restart itself failed (e.g. the new version won't pass readiness):
            // treat it like a bad boot and roll back rather than strand `current`.
            Err(e) => self.roll_back(&version, Some(e)),
        }
    }

    /// Settle a flip whose health gate never answered.
    ///
    /// The marker names the version the flip was *to* and which way it moved, and it
    /// is written before `current` moves, so three crash windows are distinguishable:
    ///
    /// - it names something other than `current` → the process died before the
    ///   rename. Nothing moved, nothing is unproven; drop the marker and carry on.
    /// - it names `current` and moved **forward** → the flip landed and the gate never
    ///   answered. `current` is unproven: roll back to the version that *was* proven.
    /// - it names `current` and was a **rollback** → `current` is already the proven
    ///   version and the owed work is the recovery bounce, not a flip. Undoing this
    ///   one lands straight back on the release the gate rejected, which is how the
    ///   first cut of this file inverted: `Layout::rollback` marks pending too, and
    ///   [`Updater::roll_back`] clears the marker only after a bounce that spans a
    ///   full server restart plus up to [`timeouts::HEALTH_GATE`] of polling.
    ///
    /// Whichever way it moved, whatever is *running* may still be the version the
    /// interrupted update was leaving behind, so both landing arms re-run the bounce
    /// and surface an unhealthy result rather than letting `up` continue over a dead
    /// server. `ensure_running = false`, unlike [`Updater::roll_back`]'s own recovery
    /// bounce: that one knows the server was up (it had just bounced it), while a
    /// marker on disk says nothing about whether this box runs a daemon at all — and
    /// force-starting one an operator deliberately stopped is not recovery.
    fn recover_unproven_flip(&self) -> Result<Recovery, CliError> {
        let Some(pending) = self.layout.pending_flip() else {
            return Ok(Recovery::None);
        };
        if self.layout.current_version().as_deref() != Some(pending.version.as_str()) {
            self.layout.clear_pending();
            return Ok(Recovery::None);
        }

        let interrupted = "(an update was interrupted)";
        let (proven, outcome) = match pending.kind {
            Flip::Forward if self.layout.previous_version().is_some() => {
                let restored = self.layout.rollback()?;
                println!(
                    "grove: {} was flipped to but never health-gated {interrupted}; rolled back to {restored}",
                    pending.version
                );
                (restored, Recovery::Undid)
            }
            // Nothing to fall back to (a first install). `current` stays where it is —
            // it is the only version installed — but it is still unproven, so it gets
            // the gate it was owed and `up` must not shortcut past it.
            Flip::Forward => {
                println!(
                    "grove: {} was flipped to but never health-gated {interrupted}; no previous version to fall back to",
                    pending.version
                );
                (pending.version.clone(), Recovery::Undid)
            }
            Flip::Rollback => {
                println!(
                    "grove: the rollback to {} never finished {interrupted}; re-running its health gate",
                    pending.version
                );
                (pending.version.clone(), Recovery::Regated)
            }
        };

        let verdict = (self.bounce)(&proven, false);
        self.layout.clear_pending();
        match verdict {
            Ok(Bounce::Healthy | Bounce::NotRunning) => Ok(outcome),
            Ok(Bounce::Unhealthy) => Err(CliError::Unhealthy(format!(
                "{proven} did not come back healthy after an interrupted update — run `grove on`"
            ))),
            Err(e) => Err(CliError::Unhealthy(format!(
                "restarting {proven} after an interrupted update failed: {e}"
            ))),
        }
    }

    /// Roll `current` back to the previous version and bring it back up. `cause`
    /// is the restart error when the bounce failed outright (vs an unhealthy boot).
    fn roll_back(&self, version: &str, cause: Option<CliError>) -> Result<(), CliError> {
        match self.layout.previous_version() {
            Some(_) => {
                let restored = self.layout.rollback()?; // marks the flip pending
                let recovery = (self.bounce)(&restored, true); // ensure the good version runs
                self.layout.clear_pending();
                let base = match cause {
                    Some(e) => format!("{version} failed to start ({e})"),
                    None => format!("{version} booted unhealthy"),
                };
                // Distinguish a clean recovery from one where the restored version
                // *also* failed to come back — the latter leaves the box down and
                // must read as unhealthy (exit 6), not a routine rollback (exit 7).
                if matches!(recovery, Ok(Bounce::Healthy)) {
                    Err(CliError::Update(format!(
                        "{base}; rolled back to {restored}"
                    )))
                } else {
                    Err(CliError::Unhealthy(format!(
                        "{base}; rolled back to {restored} but it did not come back healthy — run `grove on`"
                    )))
                }
            }
            // No fallback — surface the real cause if we have one.
            None => Err(cause.unwrap_or_else(|| {
                CliError::Update(format!(
                    "{version} booted unhealthy; no previous version to roll back to"
                ))
            })),
        }
    }

    /// `grove up --rollback`. Flip `current` to `previous` and restart a running
    /// server (`ensure_running = false`: an operator rolling back an offline
    /// install just flips, like a normal update).
    ///
    /// Deliberately does NOT run [`recover_unproven_flip`](Self::recover_unproven_flip)
    /// first: after an interrupted update this command is *already* the recovery, and
    /// rolling back twice would land on the unproven version it exists to escape.
    pub fn rollback(&self) -> Result<(), CliError> {
        let _lock = self.layout.lock()?; // serialize against a concurrent `grove up`
        let restored = self.layout.rollback()?; // marks the flip pending
        // The flip already happened. A running server that boots unhealthy on the
        // restored version — or whose restart errors outright — must be a non-zero
        // exit: reporting success over a dead server is what `up` guards against.
        // Both map to Unhealthy (exit 6, the README contract), not the raw bounce
        // error's code. `NotRunning`/`Healthy` (incl. offline rollback) stay success.
        let verdict = (self.bounce)(&restored, false);
        self.layout.clear_pending();
        match verdict {
            Ok(Bounce::Unhealthy) => Err(CliError::Unhealthy(format!(
                "rolled back to {restored}, but it booted unhealthy"
            ))),
            Err(e) => Err(CliError::Unhealthy(format!(
                "rolled back to {restored} (`current` now points at it), but restarting it failed: {e}"
            ))),
            Ok(_) => {
                println!("grove rolled back to {restored}");
                Ok(())
            }
        }
    }

    fn prune_quietly(&self) {
        if let Ok(pruned) = self.layout.prune(self.keep)
            && !pruned.is_empty()
        {
            println!("pruned old versions: {}", pruned.join(", "));
        }
    }
}

/// The health gate's budget: [`timeouts::HEALTH_GATE`], or the whole-seconds override
/// in `GROVE_HEALTH_GATE_SECS`.
///
/// The seam exists for one caller — the end-to-end test that flips onto a bundle
/// lying about its version and waits for the gate to reject it. That wait is pure
/// deadline (the daemon under it is healthy and answering, it simply reports a
/// different version), so at the production 30 s one test was 96% of the CI suite's
/// wall clock while proving nothing the same test proves at 5. Everything else about
/// the run stays real: the same flip, the same restart, the same rollback.
///
/// A missing, unparseable, or zero value is no override — the constant stands, so a
/// typo cannot silently shorten the gate on a real box.
fn health_gate(raw: Option<String>) -> Duration {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map_or(timeouts::HEALTH_GATE, Duration::from_secs)
}

/// Poll `/api/health` until the server reports ready **as `expected`**, or the
/// deadline passes. Two things it deliberately does NOT accept as healthy:
/// mere reachability (a draining old server answers 503 — `ready_version` returns
/// `None`), and a ready 200 reporting a *different* version (a stale process still
/// bound to the port, or a separately-started `grove on`). Without the version
/// check, either would green-light a failed flip and mask auto-rollback.
fn poll_ready(clock: &dyn Clock, timeout: Duration, expected: &str) -> bool {
    poll_ready_with(clock, timeout, expected, || {
        ApiClient::from_env().ready_version()
    })
}

/// `poll_ready` with the version probe injected, so the version-match gate is
/// testable without a live server. The budget resolves through the clock seam like
/// every other ladder in this crate — never an inline wall-clock read.
fn poll_ready_with(
    clock: &dyn Clock,
    timeout: Duration,
    expected: &str,
    probe: impl Fn() -> Option<String>,
) -> bool {
    let deadline = clock.deadline(timeout);
    loop {
        if probe().as_deref() == Some(expected) {
            return true;
        }
        if deadline.expired(clock) {
            return false;
        }
        thread::sleep(timeouts::POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::source::hex;
    use super::*;
    use sha2::{Digest, Sha256};
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::io::Write;
    use std::path::Path;
    use std::rc::Rc;
    use tempfile::TempDir;

    /// A `.tar.gz` containing `bin/grove` whose contents name the version.
    fn fake_bundle(v: &str) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        let body = format!("grove {v}");
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "bin/grove", body.as_bytes())
            .unwrap();
        let tar_bytes = tar.into_inner().unwrap();

        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap()
    }

    /// Write a fixture release for `versions` into `dir`, with `latest` naming the last.
    fn fixture_source(dir: &Path, target: &str, versions: &[&str]) -> BundleSource {
        for v in versions {
            let vdir = dir.join(v);
            fs::create_dir_all(&vdir).unwrap();
            let bundle = fake_bundle(v);
            fs::write(vdir.join(format!("{target}.tar.gz")), &bundle).unwrap();
            fs::write(
                vdir.join(format!("{target}.tar.gz.sha256")),
                hex(&Sha256::digest(&bundle)),
            )
            .unwrap();
        }
        fs::write(dir.join("latest"), versions.last().unwrap()).unwrap();
        BundleSource::LocalDir(dir.to_path_buf())
    }

    struct Probe {
        bounces: Rc<Cell<usize>>,
        /// Every `(expected_version, ensure_running)` the bounce was handed, in order
        /// — which version a gate ran against is the thing a recovery gets wrong.
        calls: Rc<RefCell<Vec<(String, bool)>>>,
    }

    /// An `Updater` over a two-version fixture release whose bounce records its
    /// call count and returns `outcome` (ignoring the ensure-running flag).
    fn updater(home: &TempDir, src: &TempDir, outcome: Bounce) -> (Updater, Probe) {
        let bounces = Rc::new(Cell::new(0));
        let calls: Rc<RefCell<Vec<(String, bool)>>> = Rc::new(RefCell::new(Vec::new()));
        let b = bounces.clone();
        let c = calls.clone();
        let source = fixture_source(src.path(), "test-target", &["0.3.1", "0.4.0"]);
        let updater = Updater::new(
            Layout::new(home.path()),
            "test-target",
            source,
            move |expected: &str, ensure_running| {
                b.set(b.get() + 1);
                c.borrow_mut().push((expected.to_string(), ensure_running));
                Ok(outcome)
            },
        );
        (updater, Probe { bounces, calls })
    }

    #[test]
    fn up_installs_flips_and_bounces_healthy() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (up, probe) = updater(&home, &src, Bounce::Healthy);

        up.up(Some("0.4.0")).unwrap();

        let layout = Layout::new(home.path());
        assert_eq!(layout.current_version().as_deref(), Some("0.4.0"));
        assert_eq!(probe.bounces.get(), 1, "one bounce on success");
        assert_eq!(
            fs::read_to_string(home.path().join("current/bin/grove")).unwrap(),
            "grove 0.4.0"
        );
    }

    #[test]
    fn up_without_version_uses_latest_channel() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (up, _probe) = updater(&home, &src, Bounce::Healthy);

        up.up(None).unwrap();
        assert_eq!(
            Layout::new(home.path()).current_version().as_deref(),
            Some("0.4.0")
        );
    }

    #[test]
    fn up_flips_without_rollback_when_server_not_running() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (up, probe) = updater(&home, &src, Bounce::NotRunning);

        // First install: nothing running → flip, no health-gate, no rollback.
        up.up(Some("0.4.0")).unwrap();
        assert_eq!(
            Layout::new(home.path()).current_version().as_deref(),
            Some("0.4.0")
        );
        assert_eq!(probe.bounces.get(), 1);
    }

    #[test]
    fn up_rolls_back_when_new_version_is_unhealthy() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        // First land 0.3.1 healthy.
        let (healthy_up, _p) = updater(&home, &src, Bounce::Healthy);
        healthy_up.up(Some("0.3.1")).unwrap();

        // 0.4.0 boots unhealthy (forward bounce), but the recovery bounce onto the
        // restored 0.3.1 comes back healthy → routine rollback, exit 7.
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let sick_up = Updater::new(
            Layout::new(home.path()),
            "test-target",
            fixture_source(src.path(), "test-target", &["0.3.1", "0.4.0"]),
            move |_expected: &str, ensure_running| {
                c.set(c.get() + 1);
                Ok(if ensure_running {
                    Bounce::Healthy // recovery onto 0.3.1
                } else {
                    Bounce::Unhealthy // forward onto 0.4.0
                })
            },
        );
        let err = sick_up.up(Some("0.4.0")).unwrap_err();

        assert_eq!(err.exit_code(), 7, "clean rollback → Update");
        let layout = Layout::new(home.path());
        assert_eq!(layout.current_version().as_deref(), Some("0.3.1"));
        // forward bounce + post-rollback recovery bounce = 2.
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn up_rollback_recovery_failure_reads_unhealthy() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (healthy_up, _p) = updater(&home, &src, Bounce::Healthy);
        healthy_up.up(Some("0.3.1")).unwrap();

        // Both the forward bounce AND the recovery bounce fail → the box is left
        // down: exit 6 (unhealthy), not the routine exit 7.
        let (sick_up, _probe) = updater(&home, &src, Bounce::Unhealthy);
        let err = sick_up.up(Some("0.4.0")).unwrap_err();

        assert_eq!(
            err.exit_code(),
            6,
            "rolled back but recovery failed → Unhealthy"
        );
        assert!(err.to_string().contains("did not come back healthy"));
        assert_eq!(
            Layout::new(home.path()).current_version().as_deref(),
            Some("0.3.1"),
            "the flip still landed on the previous version"
        );
    }

    #[test]
    fn up_unhealthy_first_install_errors_without_rollback() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (up, _probe) = updater(&home, &src, Bounce::Unhealthy);

        // No previous version to fall back to.
        let err = up.up(Some("0.4.0")).unwrap_err();
        assert_eq!(err.exit_code(), 7);
        assert!(err.to_string().contains("no previous"));
    }

    #[test]
    fn health_gate_requires_the_expected_version() {
        let clock = SystemClock;
        let short = Duration::from_millis(30);
        // The version we flipped to is reported → healthy immediately.
        assert!(poll_ready_with(&clock, short, "0.4.0", || Some(
            "0.4.0".into()
        )));
        // A stale process on the port reporting the OLD version must NOT pass —
        // a 200 alone would mask a failed flip and suppress auto-rollback.
        assert!(!poll_ready_with(&clock, short, "0.4.0", || Some(
            "0.3.1".into()
        )));
        // A draining old server (503 → None) must not pass either.
        assert!(!poll_ready_with(&clock, short, "0.4.0", || None));
    }

    /// The override is a test seam, so it fails closed: only a positive whole number
    /// of seconds shortens the gate, and everything else leaves the production budget
    /// standing rather than reducing it to zero on a real box.
    #[test]
    fn only_a_positive_number_overrides_the_health_gate() {
        assert_eq!(health_gate(Some(" 5 ".into())), Duration::from_secs(5));
        for no in [
            None,
            Some(String::new()),
            Some("0".into()),
            Some("2s".into()),
        ] {
            assert_eq!(health_gate(no.clone()), timeouts::HEALTH_GATE, "{no:?}");
        }
    }

    /// The gate's budget runs off the clock seam, so the 30-second production wait
    /// is provable in microseconds: with a hand-driven clock the loop exits only
    /// when the test advances past the deadline.
    #[test]
    fn the_health_gate_budget_expires_on_the_injected_clock() {
        use grove_ops::clock::TestClock;
        let clock = TestClock::new();
        let probes = Cell::new(0usize);
        let landed = poll_ready_with(&clock, timeouts::HEALTH_GATE, "0.4.0", || {
            probes.set(probes.get() + 1);
            // Advance past the budget from inside the loop — nothing else moves this
            // clock, so a deadline read against a real `Instant` would hang here.
            clock.advance(timeouts::HEALTH_GATE + Duration::from_secs(1));
            None
        });
        assert!(!landed, "the budget expired without a matching version");
        assert_eq!(probes.get(), 1, "one probe, then the deadline");
    }

    #[test]
    fn manual_rollback_surfaces_an_unhealthy_boot() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        // Land two versions so there's a previous to roll back to.
        let (healthy, _p) = updater(&home, &src, Bounce::Healthy);
        healthy.up(Some("0.3.1")).unwrap();
        healthy.up(Some("0.4.0")).unwrap();

        // Rolling back onto a version that boots unhealthy must NOT report success.
        let (sick, _p) = updater(&home, &src, Bounce::Unhealthy);
        let err = sick.rollback().unwrap_err();
        assert_eq!(err.exit_code(), 6, "unhealthy rollback target → exit 6");
        // The flip still happened — rollback swapped current to the previous.
        assert_eq!(
            Layout::new(home.path()).current_version().as_deref(),
            Some("0.3.1")
        );
    }

    #[test]
    fn manual_rollback_restart_error_reads_unhealthy() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        // Land two versions so there's a previous to roll back to.
        let (healthy, _p) = updater(&home, &src, Bounce::Healthy);
        healthy.up(Some("0.3.1")).unwrap();
        healthy.up(Some("0.4.0")).unwrap();

        // The restored version's restart errors outright (not merely unhealthy).
        // The flip already happened, so this must map to exit 6 — not the raw
        // bounce error's exit 4 — and say `current` moved.
        let up = Updater::new(
            Layout::new(home.path()),
            "test-target",
            fixture_source(src.path(), "test-target", &["0.3.1", "0.4.0"]),
            |_expected: &str, _force| Err(CliError::Daemon("spawn failed".into())),
        );
        let err = up.rollback().unwrap_err();
        assert_eq!(err.exit_code(), 6, "post-flip restart error → Unhealthy");
        assert!(err.to_string().contains("current"));
        assert_eq!(
            Layout::new(home.path()).current_version().as_deref(),
            Some("0.3.1")
        );
    }

    #[test]
    fn up_succeeds_even_when_prune_fails() {
        use std::os::unix::fs::PermissionsExt;
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let mut up = Updater::new(
            Layout::new(home.path()),
            "test-target",
            fixture_source(src.path(), "test-target", &["0.1.0", "0.2.0", "0.3.0"]),
            |_e: &str, _f| Ok(Bounce::Healthy),
        );
        // Land 0.1.0 then 0.2.0 (current=0.2.0, previous=0.1.0).
        up.up(Some("0.1.0")).unwrap();
        up.up(Some("0.2.0")).unwrap();

        // Make 0.1.0 un-prunable: strip all permissions so `remove_dir_all` can't
        // open it to enumerate children (EACCES).
        let v010 = Layout::new(home.path()).version_path("0.1.0");
        fs::set_permissions(&v010, fs::Permissions::from_mode(0o000)).unwrap();
        // A privileged runner (root) ignores the mode; then the failure can't be
        // forced, but the exit-0 contract below still holds.
        let privileged = fs::read_dir(&v010).is_ok();

        // keep=1 → prune wants to drop 0.1.0. The update is healthy, so it must
        // still exit 0 despite the prune failure (was previously exit 7).
        up.keep = 1;
        up.up(Some("0.3.0")).unwrap();
        assert_eq!(
            Layout::new(home.path()).current_version().as_deref(),
            Some("0.3.0")
        );
        if !privileged {
            assert!(
                v010.exists(),
                "prune failed on the locked dir yet the update still succeeded"
            );
        }

        // Restore perms so TempDir cleanup can remove it.
        let _ = fs::set_permissions(&v010, fs::Permissions::from_mode(0o755));
    }

    #[test]
    fn should_skip_bounce_only_when_fully_stopped() {
        // No pid + nothing answers → nothing to gate.
        assert!(should_skip_bounce(false, Reachability::Offline, false));
        // No pid but health answers (externally managed) → must gate.
        assert!(!should_skip_bounce(false, Reachability::Up, false));
        // No pid but the server is busy → still up, must gate.
        assert!(!should_skip_bounce(false, Reachability::Busy, false));
        // A running pid → always gate.
        assert!(!should_skip_bounce(true, Reachability::Offline, false));
        // Recovery bounce (ensure_running) never skips.
        assert!(!should_skip_bounce(false, Reachability::Offline, true));
    }

    /// The wiring, not just the predicate: an externally-managed server (no pid
    /// file, health answering) must flow through restart + the health gate, and
    /// its gate verdict must surface. A regression that short-circuits this case
    /// to `NotRunning` would ship un-gated flips to supervisor-managed servers.
    #[test]
    fn gated_bounce_gates_an_externally_managed_server() {
        for reachable in [Reachability::Up, Reachability::Busy] {
            let restarted = Cell::new(false);
            let out = gated_bounce(
                false, // no pid file — the shape the gate must NOT skip
                reachable,
                false,
                || {
                    restarted.set(true);
                    Ok(())
                },
                || true,
            )
            .unwrap();
            assert!(restarted.get(), "{reachable:?}: restart reached");
            assert_eq!(out, Bounce::Healthy);
        }

        // And an unhealthy gate verdict surfaces (feeds auto-rollback upstream).
        let out = gated_bounce(false, Reachability::Up, false, || Ok(()), || false).unwrap();
        assert_eq!(out, Bounce::Unhealthy);
    }

    #[test]
    fn gated_bounce_skips_only_a_fully_stopped_server() {
        // Fully stopped (no pid, nothing answers, not force-starting): skip —
        // and the restart action must NOT run.
        let restarted = Cell::new(false);
        let out = gated_bounce(
            false,
            Reachability::Offline,
            false,
            || {
                restarted.set(true);
                Ok(())
            },
            || unreachable!("no gate when skipped"),
        )
        .unwrap();
        assert_eq!(out, Bounce::NotRunning);
        assert!(!restarted.get(), "a fully-stopped server is not bounced");

        // ensure_running (the rollback recovery path) forces the restart anyway.
        let restarted = Cell::new(false);
        let out = gated_bounce(
            false,
            Reachability::Offline,
            true,
            || {
                restarted.set(true);
                Ok(())
            },
            || true,
        )
        .unwrap();
        assert_eq!(out, Bounce::Healthy);
        assert!(restarted.get(), "ensure_running restarts a stopped server");

        // A restart failure propagates rather than reaching the gate.
        let err = gated_bounce(
            true,
            Reachability::Up,
            false,
            || Err(CliError::Daemon("boom".into())),
            || unreachable!("no gate after a failed restart"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn manual_rollback_succeeds_when_restored_version_is_healthy() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (up, _p) = updater(&home, &src, Bounce::Healthy);
        up.up(Some("0.3.1")).unwrap();
        up.up(Some("0.4.0")).unwrap();
        up.rollback().unwrap();
        assert_eq!(
            Layout::new(home.path()).current_version().as_deref(),
            Some("0.3.1")
        );
    }

    #[test]
    fn up_is_idempotent_when_already_current() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (up, probe) = updater(&home, &src, Bounce::Healthy);
        up.up(Some("0.4.0")).unwrap();
        up.up(Some("0.4.0")).unwrap(); // no-op

        assert_eq!(probe.bounces.get(), 1, "second up does nothing");
    }

    #[test]
    fn up_rejects_a_corrupt_bundle() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        fixture_source(src.path(), "test-target", &["0.4.0"]);
        // Corrupt the checksum sidecar.
        fs::write(
            src.path().join("0.4.0/test-target.tar.gz.sha256"),
            "deadbeef",
        )
        .unwrap();

        let up = Updater::new(
            Layout::new(home.path()),
            "test-target",
            BundleSource::LocalDir(src.path().to_path_buf()),
            |_expected: &str, _force| Ok(Bounce::Healthy),
        );
        let err = up.up(Some("0.4.0")).unwrap_err();
        assert_eq!(err.exit_code(), 7);
        // Nothing flipped — a bad release never becomes current.
        assert!(Layout::new(home.path()).current_version().is_none());
    }

    #[test]
    fn up_rolls_back_when_the_restart_errors() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        // Land 0.3.1 healthy first.
        let (healthy, _p) = updater(&home, &src, Bounce::Healthy);
        healthy.up(Some("0.3.1")).unwrap();

        // 0.4.0's restart fails outright (not merely unhealthy) → still roll back.
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let up = Updater::new(
            Layout::new(home.path()),
            "test-target",
            fixture_source(src.path(), "test-target", &["0.3.1", "0.4.0"]),
            move |_expected: &str, ensure_running| {
                c.set(c.get() + 1);
                // Forward bounce errors; the recovery bounce (ensure_running) succeeds.
                if ensure_running {
                    Ok(Bounce::Healthy)
                } else {
                    Err(CliError::Daemon("spawn failed".into()))
                }
            },
        );

        let err = up.up(Some("0.4.0")).unwrap_err();
        assert_eq!(err.exit_code(), 7);
        assert_eq!(
            Layout::new(home.path()).current_version().as_deref(),
            Some("0.3.1"),
            "a failed restart rolls back rather than stranding current"
        );
        assert_eq!(calls.get(), 2, "forward bounce (err) + recovery bounce");
    }

    #[test]
    fn up_rejects_a_traversal_version() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (up, _p) = updater(&home, &src, Bounce::NotRunning);
        assert_eq!(up.up(Some("../etc")).unwrap_err().exit_code(), 7);
        assert!(Layout::new(home.path()).current_version().is_none());
    }

    // ─── the durable gate verdict ────────────────────────────────────────────

    /// A settled gate leaves no marker behind, whichever way it settled — otherwise
    /// the next `grove up` would roll back a version that was in fact proven (or,
    /// worse, one the gate had already rejected and rolled back for us).
    #[test]
    fn every_settled_verdict_clears_the_marker() {
        for outcome in [Bounce::Healthy, Bounce::NotRunning] {
            let home = TempDir::new().unwrap();
            let src = TempDir::new().unwrap();
            let (up, _p) = updater(&home, &src, outcome);
            up.up(Some("0.4.0")).unwrap();
            assert!(
                Layout::new(home.path()).pending_version().is_none(),
                "{outcome:?} is an answer, so nothing stays owed"
            );
        }

        // The unhealthy path settles too — through the rollback, which flips again
        // and must not leave *its* marker behind either.
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (healthy, _p) = updater(&home, &src, Bounce::Healthy);
        healthy.up(Some("0.3.1")).unwrap();
        let (sick, _p) = updater(&home, &src, Bounce::Unhealthy);
        assert!(sick.up(Some("0.4.0")).is_err());
        assert!(Layout::new(home.path()).pending_version().is_none());
    }

    /// The hazard v1 shipped: a crash between the flip and the gate strands `current`
    /// on a version nothing ever proved. The marker makes that state legible, and the
    /// next `grove up` undoes it before flipping anything new.
    #[test]
    fn an_interrupted_flip_is_rolled_back_by_the_next_up() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (up, _p) = updater(&home, &src, Bounce::Healthy);
        up.up(Some("0.3.1")).unwrap();

        // The crash: flip to 0.4.0 landed, then the updater died before its gate.
        let layout = Layout::new(home.path());
        layout
            .install_bundle("0.4.0", &fake_bundle("0.4.0"))
            .unwrap();
        layout.flip_to("0.4.0").unwrap();
        assert_eq!(layout.current_version().as_deref(), Some("0.4.0"));
        assert_eq!(layout.pending_version().as_deref(), Some("0.4.0"));

        // A fresh `grove up` finds the marker, restores the proven version, and only
        // then goes forward — 0.4.0 lands again, this time with a gate behind it.
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let next = Updater::new(
            Layout::new(home.path()),
            "test-target",
            fixture_source(src.path(), "test-target", &["0.3.1", "0.4.0"]),
            move |_expected: &str, _force| {
                c.set(c.get() + 1);
                Ok(Bounce::Healthy)
            },
        );
        next.up(Some("0.4.0")).unwrap();

        assert_eq!(
            calls.get(),
            2,
            "the recovery re-gated the restored version, then the re-flip was gated"
        );
        assert_eq!(layout.current_version().as_deref(), Some("0.4.0"));
        assert_eq!(
            layout.previous_version().as_deref(),
            Some("0.3.1"),
            "recovery rolled back to the proven version before going forward"
        );
        assert!(layout.pending_version().is_none());
    }

    /// The other crash window: the marker is written *before* the rename, so a death
    /// in between leaves one naming a version `current` never reached. Nothing moved,
    /// so recovery must move nothing either.
    #[test]
    fn a_marker_for_a_flip_that_never_landed_moves_nothing() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (up, _p) = updater(&home, &src, Bounce::Healthy);
        up.up(Some("0.3.1")).unwrap();
        up.up(Some("0.4.0")).unwrap();
        // current=0.4.0, previous=0.3.1, and a marker for a flip that never happened.
        fs::write(home.path().join("pending"), "9.9.9\n").unwrap();

        let (next, _p) = updater(&home, &src, Bounce::Healthy);
        next.up(Some("0.3.1")).unwrap();

        let layout = Layout::new(home.path());
        assert_eq!(
            layout.current_version().as_deref(),
            Some("0.3.1"),
            "the requested update ran; the stale marker triggered no rollback"
        );
        assert_eq!(layout.previous_version().as_deref(), Some("0.4.0"));
        assert!(layout.pending_version().is_none());
    }

    /// **The inversion.** `Layout::rollback` marks a flip pending too, and
    /// [`Updater::roll_back`] clears that marker only *after* its recovery bounce — a
    /// window spanning a whole server restart plus the health gate's polling. A crash
    /// inside it leaves `pending == current == the RESTORED, proven version`, which a
    /// direction-blind recovery reads as an unproven forward flip and undoes: `current`
    /// flips onto the release the gate had just rejected, and because the channel still
    /// resolves to that release, `up`'s idempotence check then prints "already on
    /// <bad>" and returns Ok with zero bounces and zero health gates.
    #[test]
    fn a_crash_in_the_auto_rollback_never_flips_back_onto_the_rejected_version() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (up, _p) = updater(&home, &src, Bounce::Healthy);
        up.up(Some("0.3.1")).unwrap();
        up.up(Some("0.4.0")).unwrap(); // 0.4.0 is what the channel resolves to

        // 0.4.0 booted unhealthy, so `roll_back` restored 0.3.1 — and the process died
        // before its recovery bounce cleared the marker. This is that state, written
        // by the very call `roll_back` makes.
        let layout = Layout::new(home.path());
        assert_eq!(layout.rollback().unwrap(), "0.3.1");
        assert_eq!(layout.pending_version().as_deref(), Some("0.3.1"));

        let (next, probe) = updater(&home, &src, Bounce::Healthy);
        next.up(None).unwrap(); // the channel still says 0.4.0

        let calls = probe.calls.borrow().clone();
        assert_eq!(
            calls.first().map(|(v, _)| v.as_str()),
            Some("0.3.1"),
            "the owed gate was re-run against the PROVEN version, not the rejected \
             one; a direction-blind recovery bounced nothing at all: {calls:?}"
        );
        assert!(
            !calls.is_empty(),
            "an interrupted rollback must never be laundered into an un-gated exit 0"
        );
        assert!(layout.pending_version().is_none());
    }

    /// The same window on the manual path (`grove up --rollback` clears its marker
    /// only after its own bounce), and the half of the recovery that must not be
    /// silent: the re-run gate says the restored version is *not* healthy, so `up`
    /// exits 6 rather than continuing over a box that is down.
    #[test]
    fn a_crash_in_a_manual_rollback_surfaces_the_re_run_gate() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (up, _p) = updater(&home, &src, Bounce::Healthy);
        up.up(Some("0.3.1")).unwrap();
        up.up(Some("0.4.0")).unwrap();

        // `Updater::rollback` flips, bounces, then clears. Died in between.
        let layout = Layout::new(home.path());
        assert_eq!(layout.rollback().unwrap(), "0.3.1");

        let (next, probe) = updater(&home, &src, Bounce::Unhealthy);
        let err = next.up(None).unwrap_err();

        assert_eq!(err.exit_code(), 6, "a dead box is not a successful update");
        assert!(err.to_string().contains("interrupted update"), "{err}");
        assert_eq!(probe.bounces.get(), 1, "the gate ran, and stopped there");
        assert_eq!(
            layout.current_version().as_deref(),
            Some("0.3.1"),
            "nothing flipped: `current` already named the proven version"
        );
        assert!(layout.pending_version().is_none(), "the marker is settled");
    }

    /// `grove up --rollback` is itself the operator's recovery, so it must not first
    /// run the automatic one: two rollbacks would land back on the unproven version.
    #[test]
    fn manual_rollback_does_not_double_undo_an_interrupted_flip() {
        let home = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        let (up, _p) = updater(&home, &src, Bounce::Healthy);
        up.up(Some("0.3.1")).unwrap();

        // Crash state: current=0.4.0 (unproven), previous=0.3.1, marker set.
        let layout = Layout::new(home.path());
        layout
            .install_bundle("0.4.0", &fake_bundle("0.4.0"))
            .unwrap();
        layout.flip_to("0.4.0").unwrap();

        let (operator, _p) = updater(&home, &src, Bounce::Healthy);
        operator.rollback().unwrap();

        assert_eq!(layout.current_version().as_deref(), Some("0.3.1"));
        assert!(layout.pending_version().is_none());
    }
}
