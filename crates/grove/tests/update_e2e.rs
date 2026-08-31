//! The phase-6 gate: the self-update state machine against a **real daemon**.
//!
//! The unit tests in `update::tests` drive an injected bounce closure, so they prove
//! the state machine's shape and nothing about what it is wired to. These two spawn
//! the real `CARGO_BIN_EXE_grove`, install it into a real versioned layout, start a
//! real `grove serve` off `current/bin/grove`, and let the real version-aware health
//! gate decide — which is the only way to prove that the flip moves the path the
//! launcher reads, that the daemon answers `/api/health` with the version the gate
//! compares, and that a rollback puts a working server back.
//!
//! The lying bundle is the trick that makes the gate observable without a second
//! build: a fixture "9.9.9" whose `bin/grove` is *this* binary, which reports its own
//! version. A gate that accepted a mere 200 would call that update healthy; the
//! version-aware one rolls it back, which is exactly the failure v1's gate was
//! written to catch.
//!
//! Hermetic: a `TempDir` home, a `TempDir` release base (no network), and a port
//! bound-then-released rather than the default `127.0.0.1:7777` — which on a
//! developer's box is their own running daemon, and `grove up` bounces whatever
//! answers there.

use std::io::Write as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// The version the shipped binary reports — and therefore the only version a health
/// gate over that binary can ever accept.
const HONEST: &str = grove::VERSION;

/// A fixture version the bundle *claims* and the binary inside it does not report.
const LYING: &str = "9.9.9";

/// What one `grove …` invocation produced.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    #[track_caller]
    fn ok(self) -> Self {
        self.exits(0)
    }

    #[track_caller]
    fn exits(self, code: i32) -> Self {
        assert_eq!(
            self.code, code,
            "expected exit {code}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout, self.stderr
        );
        self
    }

    #[track_caller]
    fn says(self, needle: &str) -> Self {
        assert!(
            self.stdout.contains(needle) || self.stderr.contains(needle),
            "output missing {needle:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout,
            self.stderr
        );
        self
    }
}

/// One installed world: a home, a fixture release base, and the bind every command
/// in it points at.
struct Box_ {
    home: PathBuf,
    base: PathBuf,
    bind: String,
    #[expect(
        dead_code,
        reason = "the scratch both paths live in; dropped last, reclaiming them"
    )]
    tmp: TempDir,
}

impl Box_ {
    fn new(bind: String) -> Self {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let base = tmp.path().join("release");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&base).unwrap();
        Self {
            home,
            base,
            bind,
            tmp,
        }
    }

    /// Run a `grove` subcommand against this world. The binary under test is the
    /// build's own, except where a test deliberately runs the *installed* one.
    fn grove(&self, args: &[&str]) -> Run {
        self.run(Path::new(env!("CARGO_BIN_EXE_grove")), args)
    }

    /// Run the binary the layout's `current` symlink resolves to — what an operator
    /// on PATH actually invokes after an install.
    fn installed(&self, args: &[&str]) -> Run {
        self.run(&self.home.join("current/bin/grove"), args)
    }

    fn run(&self, program: &Path, args: &[&str]) -> Run {
        let out = Command::new(program)
            .args(args)
            .env("GROVE_HOME", &self.home)
            .env("GROVE_BIND", &self.bind)
            .env("GROVE_INSTALL_BASE_URL", &self.base)
            // The watcher is irrelevant here and would put a second thread on the
            // manifest for the daemon's whole life.
            .env("GROVE_MANIFEST_FS_WATCH", "0")
            // The one wait in this test that cannot be skipped: the gate onto the
            // lying bundle is a pure deadline (the daemon under it is healthy and
            // answering, it just reports another version), so at the production 30 s
            // this single test was most of the CI suite's wall clock. Everything it
            // asserts — which version `current` lands on — is unchanged at 5, and the
            // honest gates below never wait at all, because `grove on` has already
            // blocked on readiness before the gate's first probe.
            .env("GROVE_HEALTH_GATE_SECS", "5")
            .env_remove("GROVE_CHANNEL")
            .output()
            .expect("the grove binary runs");
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Stage one fixture release version: `<base>/<v>/<target>.tar.gz` carrying
    /// `bin/grove` (this build's binary), plus the sha256 sidecar `grove up`
    /// verifies. Exactly the layout `scripts/release.sh` writes.
    fn stage(&self, v: &str) {
        let dir = self.base.join(v);
        std::fs::create_dir_all(&dir).unwrap();
        let bundle = bundle_of(Path::new(env!("CARGO_BIN_EXE_grove")));
        let target = grove::host_target();
        std::fs::write(dir.join(format!("{target}.tar.gz")), &bundle).unwrap();
        std::fs::write(
            dir.join(format!("{target}.tar.gz.sha256")),
            hex(&Sha256::digest(&bundle)),
        )
        .unwrap();
        std::fs::write(self.base.join("latest"), v).unwrap();
    }

    fn link(&self, name: &str) -> Option<String> {
        let target = std::fs::read_link(self.home.join(name)).ok()?;
        Some(target.file_name()?.to_str()?.to_owned())
    }

    fn pending(&self) -> Option<String> {
        std::fs::read_to_string(self.home.join("pending"))
            .ok()
            .map(|s| s.trim().to_owned())
    }
}

/// Stops whatever daemon the enclosed world is running, on the way out of the test —
/// including out of a panic, so a failed assertion never strands a process holding a
/// port and a temp home that is about to vanish underneath it.
struct Stopped<'a>(&'a Box_);

impl Drop for Stopped<'_> {
    fn drop(&mut self) {
        let _ = self.0.grove(&["off"]);
    }
}

/// A `.tar.gz` whose single entry is `bin/grove`, the given executable.
fn bundle_of(binary: &Path) -> Vec<u8> {
    let body = std::fs::read(binary).expect("the built binary is readable");
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, "bin/grove", &body[..])
        .unwrap();

    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(&builder.into_inner().unwrap()).unwrap();
    enc.finish().unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A loopback address with nothing on it: bound to claim a free ephemeral port, then
/// released so the daemon under test can take it. A pre-picked constant would collide
/// with whatever else on the machine holds it.
fn free_port() -> String {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string()
}

/// An address nothing can ever answer on: port 1 is privileged, so every probe is
/// refused and the reachability gate reads `Offline` without a race.
fn refused() -> String {
    "127.0.0.1:1".to_string()
}

/// Install → start → update onto a bundle that lies about its version → the gate
/// catches it → rollback puts the working daemon back.
#[test]
fn slow_the_update_round_trip_gates_and_rolls_back_against_a_real_daemon() {
    let world = Box_::new(free_port());
    let _stop = Stopped(&world);

    // 1. First install, nothing running: the flip lands, no gate is owed.
    world.stage(HONEST);
    world
        .grove(&["up", "--version", HONEST])
        .ok()
        .says("server not running");
    assert_eq!(world.link("current").as_deref(), Some(HONEST));
    assert_eq!(world.pending(), None, "a settled verdict clears the marker");

    // 2. Start the daemon. `grove on` launches `current/bin/grove serve` — the path
    //    the flip above moved — so what answers below is the installed release.
    world.installed(&["on"]).ok().says("listening");
    world.installed(&["ok"]).ok().says(&format!("(v{HONEST})"));

    // 3. Update onto the lying bundle. The bounce restarts the daemon, which comes up
    //    perfectly healthy — and reports HONEST, not 9.9.9. A gate that took a 200
    //    for an answer would call this a success and leave `current` on a version the
    //    box is not running.
    world.stage(LYING);
    world
        .installed(&["up", "--version", LYING])
        .exits(7)
        .says("rolled back");

    // 4. …and the rollback is real: `current` is back on the proven version, nothing
    //    is left owed, and the daemon is up and answering as that version.
    assert_eq!(world.link("current").as_deref(), Some(HONEST));
    assert_eq!(world.link("previous").as_deref(), Some(LYING));
    assert_eq!(world.pending(), None);
    world.installed(&["ok"]).ok().says(&format!("(v{HONEST})"));

    world.installed(&["off"]).ok();
}

/// The hazard v1 shipped (`lm:self-update-flip`), end to end: a crash between the
/// flip and its gate leaves `current` on a version nothing proved, and the next
/// `grove up` finds the marker and undoes it before going anywhere.
#[test]
fn slow_an_interrupted_flip_is_undone_by_the_next_up() {
    // Nothing to bounce: this is the state machine, not the daemon.
    let world = Box_::new(refused());

    world.stage(HONEST);
    world.grove(&["up", "--version", HONEST]).ok();
    world.stage(LYING);
    world.grove(&["up", "--version", LYING]).ok();
    assert_eq!(world.link("current").as_deref(), Some(LYING));

    // The crash: re-arm the marker the completed update cleared, which is precisely
    // the on-disk state a `kill -9` between `flip_to` and the gate leaves behind —
    // the forward direction included, because that is the half that decides whether
    // recovery may move `current` at all.
    std::fs::write(
        world.home.join("pending"),
        format!("to={LYING}\nkind=forward\n"),
    )
    .unwrap();

    world
        .grove(&["up", "--version", HONEST])
        .ok()
        .says("never health-gated")
        .says(&format!("rolled back to {HONEST}"));

    assert_eq!(
        world.link("current").as_deref(),
        Some(HONEST),
        "recovery restored the proven version, and the requested update was already it"
    );
    assert_eq!(world.pending(), None);
}
