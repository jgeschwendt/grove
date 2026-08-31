//! The phase-5 gate: the real `grove` binary, driven end to end, twice.
//!
//! Every other test in this crate calls a function. These two spawn
//! `CARGO_BIN_EXE_grove` and read its exit code and its stdout, which is the only way
//! to prove the parts actually meet — that the clap tree reaches the dispatch, that
//! the dispatch reads `GROVE_HOME`/`GROVE_BIND` from the environment an operator sets,
//! that the client and the daemon agree on the wire, and that an exit code survives
//! the trip from a `CliError` to a process.
//!
//! The same flow runs against both halves of the single-realizer gate:
//!
//! - [`slow_the_served_flow_drives_a_real_daemon`] — a daemon is up, so every mutating
//!   command declares and delegates, and the *daemon* is what puts the directories on
//!   disk. The assertions wait for that to happen rather than assuming it already has.
//! - [`slow_the_offline_flow_realizes_in_process`] — nothing is listening, so the CLI
//!   realizes inline and every effect is on disk the moment the process exits.
//!
//! Hermetic by construction: a `TempDir` home, a local fixture repo as the remote (no
//! network), and the daemon on port 0 with its real address read back out of its own
//! startup log rather than guessed — a pre-picked port would race whatever else on the
//! machine takes it between the pick and the bind.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use grove_ops::clock::{Clock, SystemClock};
use grove_ops::testfix;
use tempfile::TempDir;

const SLUG: &str = "o/r";
const BRANCH: &str = "feature/x";
const NAME: &str = "feature-x";

/// How long the served flow waits for the daemon to realize a declaration. Generous:
/// the work is a real clone plus a real `git worktree add`, queued on the root's lane
/// behind whatever the reconcile pass is already doing.
const REALIZE_BUDGET: Duration = Duration::from_secs(30);

/// Gap between `realized` polls — the same event-poor busy-wait `ServerControl` runs,
/// and bounded the same way, off the clock seam.
const POLL: Duration = Duration::from_millis(50);

/// What one `grove …` invocation produced.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    #[track_caller]
    fn ok(self) -> Self {
        assert_eq!(
            self.code, 0,
            "expected success\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout, self.stderr
        );
        self
    }

    #[track_caller]
    fn says(self, needle: &str) -> Self {
        assert!(
            self.stdout.contains(needle),
            "stdout missing {needle:?}\n--- stdout ---\n{}",
            self.stdout
        );
        self
    }
}

/// Run the real binary with `home` and, when given, a daemon address.
fn grove(home: &Path, bind: Option<&str>, args: &[&str]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_grove"));
    cmd.args(args)
        .env("GROVE_HOME", home)
        // The watch is opportunistic coverage for hand-edits; the CLI's nudge is the
        // reliable path and the one under test. Leaving it on would let an autonomous
        // convergence pass, rather than the command, be what realized a declaration.
        .env("GROVE_MANIFEST_FS_WATCH", "0")
        .env_remove("GROVE_BIND");
    if let Some(bind) = bind {
        cmd.env("GROVE_BIND", bind);
    }
    let out = cmd.output().expect("the grove binary runs");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A `grove serve` child, killed when the test's binding goes out of scope — including
/// on a panic, so a failed assertion never strands a daemon holding a port.
struct Serving {
    child: Child,
    addr: String,
    #[expect(
        dead_code,
        reason = "held so the drain thread's sink outlives the child; read only when \
                  a boot failure needs explaining"
    )]
    log: Arc<Mutex<Vec<String>>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start `grove serve` on port 0 and wait until it says what port it got.
///
/// stderr is drained on a thread for the child's whole life, not merely until the
/// address is found: a daemon whose log pipe fills blocks in `write`, and a test that
/// stopped reading would wedge the very thing it is driving.
fn serve(home: &Path) -> Serving {
    let mut child = Command::new(env!("CARGO_BIN_EXE_grove"))
        .arg("serve")
        .env("GROVE_HOME", home)
        .env("GROVE_BIND", "127.0.0.1:0")
        .env("GROVE_MANIFEST_FS_WATCH", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the grove binary runs");

    let stderr = child.stderr.take().expect("stderr is piped");
    let log = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn({
        let log = Arc::clone(&log);
        move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(addr) = listening_addr(&line) {
                    let _ = tx.send(addr);
                }
                log.lock().unwrap().push(line);
            }
        }
    });

    let addr = rx.recv_timeout(REALIZE_BUDGET).unwrap_or_else(|e| {
        panic!(
            "the daemon never announced its bind ({e}): {:?}",
            log.lock()
        )
    });
    Serving { child, addr, log }
}

/// Pull the bound address out of the daemon's startup line, whose `bind=` field is
/// the only place a port-0 daemon's real port is published.
///
/// The line is stripped of ANSI escapes first: `tracing-subscriber`'s formatter
/// colours its field names, and `\u{1b}[3mbind\u{1b}[0m\u{1b}[2m=\u{1b}[0m` does not
/// start with `bind=`.
fn listening_addr(line: &str) -> Option<String> {
    let line = strip_ansi(line);
    if !line.contains("grove server listening") {
        return None;
    }
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("bind="))
        .map(str::to_owned)
}

/// Drop CSI sequences (`ESC [ … <final byte>`) from a log line.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // Skip the `[`, then everything up to and including the final byte (@–~).
        for c in chars.by_ref() {
            if ('@'..='~').contains(&c) && c != '[' {
                break;
            }
        }
    }
    out
}

/// Poll `done` until it holds or the budget runs out, off the clock seam.
#[track_caller]
fn until(what: &str, done: impl Fn() -> bool) {
    let clock = SystemClock;
    let deadline = clock.deadline(REALIZE_BUDGET);
    while !done() {
        assert!(
            !deadline.expired(&clock),
            "the daemon never {what} inside {}s",
            REALIZE_BUDGET.as_secs()
        );
        std::thread::sleep(POLL);
    }
}

/// Move the fixture source one commit ahead of everything cloned from it, so a sync
/// has something to fast-forward onto. Inside the test's own scratch, so it touches
/// nothing else.
fn ahead(src: &str) {
    let src = Path::new(src);
    std::fs::write(src.join("AHEAD.md"), "ahead").unwrap();
    let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
    testfix::git(src, &[&id[..], &["add", "."]].concat());
    testfix::git(src, &[&id[..], &["commit", "-q", "-m", "ahead"]].concat());
}

/// A home directory and a local fixture repo to clone from, both inside one scratch
/// that reclaims them together.
fn scratch(tmp: &TempDir) -> (PathBuf, String) {
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // The slug `clone add` derives is the source's last two path segments.
    let src = tmp.path().join("src/o/r");
    testfix::fixture_repo(&src);
    (home, src.to_str().unwrap().to_owned())
}

fn root_dir(home: &Path) -> PathBuf {
    home.join("code").join(SLUG)
}

fn manifest(home: &Path) -> String {
    std::fs::read_to_string(home.join("manifest.toml")).unwrap_or_default()
}

/// The whole served lifecycle through the real binary: declare → the daemon realizes →
/// read it back → doctor → remove → remove → stop.
///
/// Every mutating step here proves the *delegating* arm of the single-realizer gate:
/// the CLI writes the manifest and nudges, and the directory appears because the
/// daemon put it there.
#[test]
fn slow_the_served_flow_drives_a_real_daemon() {
    let tmp = TempDir::new().unwrap();
    let (home, src) = scratch(&tmp);
    let daemon = serve(&home);
    let bind = Some(daemon.addr.as_str());

    grove(&home, bind, &["ok"]).ok().says("grove server:");

    // clone add — declared here, cloned there.
    grove(&home, bind, &["clone", "add", &src])
        .ok()
        .says("grove server is realizing it");
    assert!(manifest(&home).contains(r#"[roots."o/r"]"#), "declared");
    until("cloned the root", || {
        root_dir(&home).join(".trunk").is_dir()
    });

    // tree add — same shape, one level down. The name is the branch with `/` folded.
    grove(
        &home,
        bind,
        &["tree", "add", SLUG, BRANCH, "--base", "main"],
    )
    .ok()
    .says("grove server is creating it");
    until("created the worktree", || {
        root_dir(&home).join(NAME).is_dir()
    });

    // tree list against a running daemon reports what only the daemon knows — the
    // engine status and the pool level — beside the worktrees.
    grove(&home, bind, &["tree", "list", SLUG])
        .ok()
        .says("root o/r:")
        .says("pool 0/0")
        .says(NAME);

    grove(&home, bind, &["doctor", SLUG]).ok().says("check ");

    // sync — the one delegating command that is *not* synchronous. It exits on the
    // ack, so the trunk is still where it was; the daemon's engine moves it, and the
    // wait below is exactly what an operator does with `grove tree list`.
    ahead(&src);
    grove(&home, bind, &["sync", SLUG])
        .ok()
        .says("sync accepted for o/r");
    until("fast-forwarded the trunk", || {
        root_dir(&home).join(".trunk/AHEAD.md").is_file()
    });

    // The destructive pair, both synchronous through the daemon: by the time the
    // command has exited, the daemon has already done the work on the root's lane.
    grove(&home, bind, &["tree", "remove", SLUG, NAME])
        .ok()
        .says("grove server is deleting it");
    assert!(!root_dir(&home).join(NAME).exists(), "the worktree is gone");

    grove(&home, bind, &["clone", "remove", SLUG])
        .ok()
        .says("grove server is deleting it");
    assert!(!root_dir(&home).exists(), "the root is gone");
    assert!(!manifest(&home).contains(r#"[roots."o/r"]"#), "undeclared");

    // `off` with no pid file is the served-mode drain: identify the listener as grove,
    // POST the shutdown, wait for the port to close.
    grove(&home, bind, &["off"])
        .ok()
        .says("grove server stopped");
    grove(&home, bind, &["ok"]);
    assert_eq!(
        grove(&home, bind, &["ok"]).code,
        4,
        "a stopped daemon is exit 4, the code every script branches on"
    );
}

/// The same lifecycle with nothing listening: the CLI is the realizer, so every effect
/// is on disk the moment each command returns.
#[test]
fn slow_the_offline_flow_realizes_in_process() {
    let tmp = TempDir::new().unwrap();
    let (home, src) = scratch(&tmp);
    // No daemon, and a bind nothing holds: `reachable` reads Offline by refusal.
    let bind = Some("127.0.0.1:1");

    assert_eq!(grove(&home, bind, &["ok"]).code, 4, "no daemon is exit 4");

    grove(&home, bind, &["clone", "add", &src])
        .ok()
        .says("cloned  o/r");
    assert!(root_dir(&home).join(".git").is_dir(), "the bare is here");
    assert!(root_dir(&home).join(".trunk").is_dir(), "and the trunk");

    grove(
        &home,
        bind,
        &["tree", "add", SLUG, BRANCH, "--base", "main"],
    )
    .ok()
    .says("created worktree");
    assert!(root_dir(&home).join(NAME).is_dir(), "realized in-process");

    grove(&home, bind, &["tree", "list", SLUG])
        .ok()
        .says(NAME)
        .says(BRANCH);

    // Doctor offline runs the same share pass plus the plumbing checks, and derives
    // the one status a filesystem can support.
    grove(&home, bind, &["doctor", SLUG])
        .ok()
        .says("check manifest: 1 ok");

    // sync offline: this process is the realizer, so the fetch has already happened by
    // the time the command exits — and the report the delegating arm cannot give is
    // printed instead of an ack.
    ahead(&src);
    grove(&home, bind, &["sync", SLUG])
        .ok()
        .says("synced o/r: fetched, trunk updated")
        .says("  tip ");
    assert!(root_dir(&home).join(".trunk/AHEAD.md").is_file());

    // `apply` never delegates, so it is the same command in both worlds — here it is
    // a no-op over an already-realized home, which must still be exit 0.
    grove(&home, bind, &["apply"]).ok().says("present o/r");

    grove(&home, bind, &["tree", "remove", SLUG, NAME])
        .ok()
        .says("removed worktree");
    assert!(!root_dir(&home).join(NAME).exists());

    grove(&home, bind, &["clone", "remove", SLUG])
        .ok()
        .says("removed o/r");
    assert!(!root_dir(&home).exists());
    assert!(!manifest(&home).contains(r#"[roots."o/r"]"#), "undeclared");

    // And the exit codes an operator reads. Listing a root that is not there is not an
    // error — a read of nothing is an empty list — while creating a worktree under one
    // cannot work. (`up` is exercised in `update_e2e`, which owns the release fixtures
    // it needs to run without reaching the network.)
    grove(&home, bind, &["tree", "list", "o/nope"])
        .ok()
        .says("no worktrees for o/nope");
    assert_eq!(grove(&home, bind, &["tree", "add", "o/nope", "x"]).code, 1);
    // Removing what was never declared is exit 3 and deletes nothing — `<home>/code/
    // <slug>` is a real directory whether or not grove put it there, and the offline
    // arm used to `remove_dir_all` any valid-shaped slug and report success.
    let stranger = home.join("code/mine/notes");
    std::fs::create_dir_all(&stranger).unwrap();
    std::fs::write(stranger.join("IMPORTANT.txt"), "not grove's").unwrap();
    assert_eq!(
        grove(&home, bind, &["clone", "remove", "mine/notes"]).code,
        3
    );
    assert!(stranger.join("IMPORTANT.txt").exists(), "nothing deleted");
    assert_eq!(
        grove(&home, bind, &["tree", "remove", "o/nope", "x"]).code,
        3
    );
    grove(&home, bind, &["version"]).ok().says("grove 0.");
}
