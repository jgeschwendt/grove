//! Git operations: bare clone (gix) and worktree creation (git CLI).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::clock::{Clock, Deadline, SystemClock};

/// Default hard cap for a single clone. Generous — it bounds a wedged clone, not
/// a slow-but-progressing one. Override with `GROVE_CLONE_TIMEOUT_SECS` (`0` off).
const DEFAULT_CLONE_TIMEOUT_SECS: u64 = 3600;

/// Default hard cap for a single fetch (`root.sync`). A fetch of an existing bare
/// moves far less data than a clone, so the cap is tighter. Override with
/// `GROVE_FETCH_TIMEOUT_SECS` (`0` off).
const DEFAULT_FETCH_TIMEOUT_SECS: u64 = 600;

/// Clone `url` into `dest` as a bare repository; returns the detected default
/// branch (falls back to `main`).
///
/// Timeout posture: gix's reqwest transport already bounds *connect* at 20s, so
/// an unreachable host fails fast. On top of that, a watchdog trips the `gix`
/// interrupt flag after [`clone_timeout`], aborting wherever gix polls it (the
/// receive / index / delta-resolution phases). The one case neither covers is a
/// connection that opens then goes silent mid-transfer (a blocking `recv` with no
/// bytes) — the reqwest backend exposes no read/stall timeout, so a hard bound
/// there would need the curl backend (libcurl/OpenSSL) or the async-clone-with-
/// progress phase. We accept that gap rather than regress to a blunt cap.
pub fn clone_bare(url: &str, dest: &Path) -> Result<String> {
    let mut prepare =
        gix::prepare_clone_bare(url, dest).with_context(|| format!("prepare clone of {url}"))?;

    let interrupt = Arc::new(AtomicBool::new(false));
    // The clone's budget is read off the clock seam, not `Instant::now()`: production
    // wires the real clock here, and the watchdog's own test drives a `TestClock`.
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let _watchdog = clone_timeout()
        .map(|d| Watchdog::spawn(interrupt.clone(), clock.deadline(d), Arc::clone(&clock)));

    let (repo, _outcome) = prepare
        .fetch_only(gix::progress::Discard, &interrupt)
        .map_err(|e| {
            if interrupt.load(Ordering::Relaxed) {
                anyhow::anyhow!("clone of {url} aborted: exceeded the clone timeout")
            } else {
                anyhow::Error::new(e).context(format!("fetch from {url}"))
            }
        })?;

    let branch = repo
        .head_name()
        .ok()
        .flatten()
        .map_or_else(|| "main".to_string(), |name| name.shorten().to_string());

    Ok(branch)
}

/// The configured clone hard cap, or `None` when disabled (`0`).
fn clone_timeout() -> Option<Duration> {
    parse_timeout(
        std::env::var("GROVE_CLONE_TIMEOUT_SECS").ok(),
        DEFAULT_CLONE_TIMEOUT_SECS,
    )
}

/// The configured fetch hard cap, or `None` when disabled (`0`).
fn fetch_timeout() -> Option<Duration> {
    parse_timeout(
        std::env::var("GROVE_FETCH_TIMEOUT_SECS").ok(),
        DEFAULT_FETCH_TIMEOUT_SECS,
    )
}

fn parse_timeout(value: Option<String>, default_secs: u64) -> Option<Duration> {
    let secs = value
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default_secs);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Flips `interrupt` once `deadline` passes on `clock`, unless dropped first (clone
/// finished). gix checks the flag through the receive/index phases and aborts.
struct Watchdog {
    done: Arc<AtomicBool>,
}

impl Watchdog {
    fn spawn(interrupt: Arc<AtomicBool>, deadline: Deadline, clock: Arc<dyn Clock>) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let done_for_thread = done.clone();
        thread::spawn(move || {
            while !done_for_thread.load(Ordering::Relaxed) {
                if deadline.expired(&*clock) {
                    interrupt.store(true, Ordering::Relaxed);
                    return;
                }
                thread::sleep(Duration::from_millis(500));
            }
        });
        Self { done }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed); // let the watchdog thread exit
    }
}

/// A `git` invocation with the **C locale pinned** (`LC_ALL=C`, `LANG=C`). grove-ops
/// parses git's **stderr** by English substring ("invalid reference", "already
/// checked out", "missing but already registered", …); a localized git — Linux ships
/// translations and honors `LANG`/`LC_*` — would break those matches, e.g. routing a
/// genuine unknown-revision into a `bail!` (a legit `tree add` hard-fails) or a
/// transient error into a silent new branch. Every git spawn in this module goes
/// through here so all stderr parsing is locale-robust, not just one call site.
#[must_use]
pub fn git_command() -> Command {
    let mut cmd = Command::new("git");
    cmd.env("LC_ALL", "C").env("LANG", "C");
    for var in LOCAL_REPO_ENV {
        cmd.env_remove(var);
    }
    cmd
}

/// git's local-repository environment (`git rev-parse --local-env-vars`): the
/// variables that pin a git invocation to a specific repo/index/worktree. A
/// grove process launched from inside a git hook or `rebase --exec` inherits
/// them (e.g. a relative `GIT_INDEX_FILE=.git/index`), which would silently
/// redirect every git spawn here away from the repo `-C` selects — clones
/// "succeed" against the wrong index, `worktree add` dies on `ENOTDIR`. Scrubbed
/// on every spawn; the paired test pins this list against the installed git's.
const LOCAL_REPO_ENV: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_INTERNAL_SUPER_PREFIX",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];

/// Drain a child pipe to completion on its own thread — see [`run_git`].
fn drain<R: std::io::Read + Send + 'static>(r: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut r = r;
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    })
}

/// The single git runner: spawn `cmd`, wait for it, and collect its `Output`,
/// optionally under a wall-clock cap. `None` = unbounded (plain `output()`); a
/// `Some(timeout)` bound is a kill of the child at the deadline — the git CLI has
/// no interrupt flag to trip (unlike the gix clone's watchdog). Pipes are drained
/// on threads so a chatty child can't deadlock against a full pipe buffer while the
/// deadline loop polls `try_wait`. `Err` is reserved for real faults — a failed
/// spawn/wait or the timeout; a **nonzero exit is not an error here**, because
/// callers interpret it differently (a hard `bail`, a DWIM fallback, a
/// default-branch guess). Use [`check_status`]/[`fail_with_stderr`] at the call
/// site to turn a nonzero exit into an error, and [`stdout_trimmed`] to read output.
fn run_git(cmd: Command, timeout: Option<Duration>, what: &str) -> Result<std::process::Output> {
    let clock = SystemClock;
    let deadline = timeout.map(|t| clock.deadline(t));
    run_until(cmd, deadline, &clock, what)
}

/// [`run_git`] with the budget already resolved against a [`Clock`]. The deadline is
/// absolute, so a caller that already spent part of its budget passes what is left
/// rather than re-basing a `Duration` (the v1 bug [`Deadline`] exists to prevent), and
/// a test drives the whole bounded loop off a `TestClock` without waiting on wall time.
fn run_until(
    mut cmd: Command,
    deadline: Option<Deadline>,
    clock: &dyn Clock,
    what: &str,
) -> Result<std::process::Output> {
    use std::process::Stdio;

    let Some(deadline) = deadline else {
        return cmd.output().with_context(|| format!("spawn {what}"));
    };

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("spawn {what}"))?;

    let stdout = drain(child.stdout.take().expect("piped stdout"));
    let stderr = drain(child.stderr.take().expect("piped stderr"));

    let status = loop {
        if let Some(status) = child.try_wait().with_context(|| format!("wait {what}"))? {
            break status;
        }
        if deadline.expired(clock) {
            let _ = child.kill();
            let _ = child.wait(); // reap; never leave a zombie
            bail!("{what} aborted: exceeded its timeout");
        }
        thread::sleep(Duration::from_millis(100));
    };

    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// `out`'s stdout, decoded lossily and trimmed — the "read a line of git output"
/// tail shared by `rev_parse`/`default_branch`/`remote_url`/…
fn stdout_trimmed(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The `<what> failed: <stderr>` error every status-check block bails with, stderr
/// decoded lossily and trimmed. Split out so a caller that interprets a nonzero exit
/// itself (the DWIM paths) can still build the terminal error identically.
fn fail_with_stderr(out: &std::process::Output, what: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{what} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// Bail with `<what> failed: <stderr>` unless `out` exited zero — the plain status
/// gate. The DWIM (`worktree_add`/`attach_branch`) and default-branch paths, which
/// read a nonzero exit as something other than a hard error, don't use this.
fn check_status(out: &std::process::Output, what: &str) -> Result<()> {
    if out.status.success() {
        Ok(())
    } else {
        Err(fail_with_stderr(out, what))
    }
}

/// Fetch `branch` from `remote` into `bare`, updating only the remote-tracking ref
/// (`refs/remotes/<remote>/<branch>`). Single-branch refspec — bounded work; nothing
/// downstream reads other refs (the trunk ffs from the tracking ref, pool slots detach
/// at the local branch tip). Wall time capped by [`fetch_timeout`] via a child kill.
pub fn fetch(bare: &Path, remote: &str, branch: &str) -> Result<()> {
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(bare)
        .args(["fetch", "--quiet", "--", remote, &refspec]);
    let out = run_git(cmd, fetch_timeout(), "git fetch")?;
    check_status(&out, "git fetch")?;
    Ok(())
}

/// The trunk half of a `root.sync`: what happened to the checkout when asked to
/// fast-forward onto `upstream`. `Diverged`/`Dirty` are *reported*, never forced —
/// sync must not reset a trunk someone has touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FastForward {
    /// The checkout moved to the upstream tip.
    Updated,
    /// Already at the upstream tip — nothing to do.
    AlreadyCurrent,
    /// Local commits not on the upstream: an ff is impossible. Left untouched.
    Diverged,
    /// Uncommitted *tracked* changes, or an untracked file an incoming commit would
    /// overwrite. Left untouched (never risk a clobber).
    Dirty,
}

/// Fast-forward the worktree at `wt` onto `upstream` (`merge --ff-only`), refusing
/// dirty/diverged states as structured outcomes rather than errors — see
/// [`FastForward`]. `Err` is reserved for real faults (a broken repo, git missing).
pub fn fast_forward(wt: &Path, upstream: &str) -> Result<FastForward> {
    // A *tracked*-file modification is reported dirty before any merge attempt: git
    // would happily ff over non-conflicting local edits, but a grove-managed trunk
    // with hand-made changes is a state the operator should resolve, not race.
    // `--untracked-files=no` is load-bearing: grove itself materializes share sources
    // as untracked files in the trunk (see `env::source_outcome`), so counting
    // untracked files as dirty would wedge every future ff on a repo that doesn't
    // gitignore that path. An untracked file the incoming commits *would* overwrite
    // is instead caught at the merge step below (git refuses → Dirty).
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(wt)
        .args(["status", "--porcelain", "--untracked-files=no"]);
    let status = run_git(cmd, None, "git status")?;
    check_status(&status, "git status")?;
    if !status.stdout.is_empty() {
        return Ok(FastForward::Dirty);
    }

    let before = rev_parse(wt, "HEAD")?;
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(wt)
        .args(["merge", "--ff-only", "--quiet", "--", upstream]);
    let merge = run_git(cmd, None, "git merge --ff-only")?;
    if !merge.status.success() {
        let err = String::from_utf8_lossy(&merge.stderr);
        let lower = err.to_ascii_lowercase();
        // git 2.39 phrases the refusal "fatal: Not possible to fast-forward,
        // aborting."; older/related wording says "not possible to fast-forward"
        // too. Locale-stable via the pinned C locale (see `git_command`).
        if lower.contains("not possible to fast-forward") {
            return Ok(FastForward::Diverged);
        }
        // An ff git refuses over *untracked* working-tree files aborts without
        // touching the tree — the untracked twin of the tracked dirty gate above.
        // unpack-trees.c emits two phrasings ("... would be overwritten by merge"
        // and "... would be removed by merge"); match the shared stem so both
        // report Dirty, never a clobber or a hard error.
        if lower.contains("untracked working tree files would be") {
            return Ok(FastForward::Dirty);
        }
        return Err(fail_with_stderr(&merge, "git merge --ff-only"));
    }

    Ok(if rev_parse(wt, "HEAD")? == before {
        FastForward::AlreadyCurrent
    } else {
        FastForward::Updated
    })
}

/// Resolve `rev` in the repo/worktree at `repo` to a full object id.
pub fn rev_parse(repo: &Path, rev: &str) -> Result<String> {
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options", rev]);
    let out = run_git(cmd, None, "git rev-parse")?;
    check_status(&out, &format!("git rev-parse {rev}"))?;
    Ok(stdout_trimmed(&out))
}

/// How many commits reachable from `HEAD` are on no remote-tracking ref — the
/// "would this delete work nobody else has?" question, answered for a branch with an
/// upstream and one that has never been pushed alike.
///
/// [`Status`]'s `ahead` cannot answer it: git omits `# branch.ab` for a branch with
/// no upstream, so a brand-new branch carrying a day's commits reports `ahead: 0`.
pub fn unpushed_count(repo: &Path) -> Result<u32> {
    let mut cmd = git_command();
    cmd.args(["--no-optional-locks", "-C"]).arg(repo).args([
        "rev-list",
        "--count",
        "HEAD",
        "--not",
        "--remotes",
    ]);
    let out = run_git(cmd, None, "git rev-list")?;
    check_status(&out, "git rev-list")?;
    Ok(stdout_trimmed(&out).parse().unwrap_or(0))
}

/// A checkout's drift, as the dashboard draws it: where `HEAD` sits, how far it
/// has moved from its upstream, and how much is uncommitted.
///
/// `ahead`/`behind` are `0` when the branch has no upstream (git omits
/// `# branch.ab` entirely) — indistinguishable from "in sync" by count alone, so
/// `upstream: None` is what callers test to tell the two apart.
///
/// `untracked` is reported but deliberately **not** part of the dirty signal a
/// trunk renders: grove materializes declared shares as untracked files inside
/// the trunk, so a healthy trunk carries a permanent nonzero untracked count. Same
/// reasoning as [`fast_forward`]'s gate, which counts only *tracked* modifications.
// `Clone`/`Deserialize` for the read surface the daemon publishes: a status is
// folded into `GET /api/roots`' per-worktree view and decoded back by its tests.
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq, Eq, serde::Serialize)]
pub struct Status {
    pub ahead: u32,
    pub behind: u32,
    /// The checked-out branch, or `None` when detached.
    pub branch: Option<String>,
    /// Paths with an unresolved merge conflict. Counted here *instead of* in
    /// `staged`/`unstaged`: a conflict sets both porcelain columns, so folding it
    /// into those would double-count one path as two pending edits.
    pub conflicted: u32,
    /// Full object id of `HEAD`, or `None` on an unborn branch (no commits yet).
    pub head: Option<String>,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    /// The upstream ref (`origin/main`), or `None` when the branch doesn't track one.
    pub upstream: Option<String>,
}

/// Read the git status of the checkout at `repo`.
///
/// One `git status --porcelain=v2 --branch` spawn carries head, branch, upstream,
/// ahead/behind, and every pending path — so a per-root dashboard read costs a
/// single subprocess rather than the four `rev-parse`/`rev-list`/`diff` calls the
/// same display would otherwise need.
///
/// `--no-optional-locks` is what keeps this a *read*. Plain `git status` may take
/// the index lock and write back a refreshed stat cache — observed rewriting
/// the bare's `worktrees/<name>/index` once this ran per worktree, which broke
/// `env::diagnose`'s must-not-mutate guarantee. That test
/// (`env::tests::diagnose_mutates_nothing`) is the pin: it fails without this flag.
/// Whether the rewrite happens depends on the checkout's stat state, so the flag is
/// not optional just because a given repo doesn't provoke it.
pub fn status(repo: &Path) -> Result<Status> {
    let mut cmd = git_command();
    cmd.args(["--no-optional-locks", "-C"]).arg(repo).args([
        "status",
        "--porcelain=v2",
        "--branch",
    ]);
    let out = run_git(cmd, None, "git status")?;
    check_status(&out, "git status")?;
    Ok(parse_status(&out.stdout))
}

/// Parse `git status --porcelain=v2 --branch`.
///
/// Header lines are `# branch.<key> <value>`; entries are one per line, keyed by
/// the leading token — `1` ordinary change, `2` rename/copy, `u` unmerged, `?`
/// untracked, `!` ignored. For `1`/`2` the second field is the two-column XY code:
/// `X` is the staged (index vs HEAD) state, `Y` the unstaged (worktree vs index)
/// one, `.` meaning unchanged. A path can be both — staged edit, then edited again.
///
/// Lossy UTF-8 decoding is safe here where [`parse_worktree_list`] needed bytes:
/// nothing downstream addresses a path from this output, it only counts lines.
fn parse_status(stdout: &[u8]) -> Status {
    let mut status = Status::default();

    for line in String::from_utf8_lossy(stdout).lines() {
        let mut fields = line.split(' ');
        match fields.next() {
            Some("#") => match (fields.next(), fields.next()) {
                // `(initial)` on an unborn branch, `(detached)` with no branch —
                // both are git's "no value" sentinels, left as `None`.
                (Some("branch.oid"), Some(oid)) if oid != "(initial)" => {
                    status.head = Some(oid.to_string());
                }
                (Some("branch.head"), Some(head)) if head != "(detached)" => {
                    status.branch = Some(head.to_string());
                }
                (Some("branch.upstream"), Some(up)) => status.upstream = Some(up.to_string()),
                // `+N -M`, signs included; a malformed count reads as 0 rather
                // than failing the whole status.
                (Some("branch.ab"), Some(ahead)) => {
                    status.ahead = ahead
                        .strip_prefix('+')
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0);
                    status.behind = fields
                        .next()
                        .and_then(|n| n.strip_prefix('-'))
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0);
                }
                _ => {}
            },
            Some("1" | "2") => {
                let mut xy = fields.next().unwrap_or("..").chars();
                if xy.next().is_some_and(|x| x != '.') {
                    status.staged += 1;
                }
                if xy.next().is_some_and(|y| y != '.') {
                    status.unstaged += 1;
                }
            }
            Some("u") => status.conflicted += 1,
            Some("?") => status.untracked += 1,
            _ => {}
        }
    }
    status
}

/// A worktree as git sees it (the actual state).
#[derive(Debug, PartialEq, Eq)]
pub struct GitWorktree {
    pub path: PathBuf,
    /// The checked-out branch, or `None` if detached.
    pub branch: Option<String>,
}

/// Add a worktree at `path` for `branch`.
///
/// **DWIM first** (`git worktree add <path> <branch>`): checks out an existing
/// local head, or — when `branch` matches exactly one `refs/remotes/<remote>/<branch>`
/// — creates a local tracking branch off that remote ref. This is git's own
/// behavior and is what users expect: `tree add slug feature/x` on a freshly
/// cloned repo should track `origin/feature/x`, not invent a new branch off
/// `HEAD`. (The pre-DWIM check `git rev-parse --verify <branch>` got the local
/// case but routed remote-only branches into the `-b` fallback, silently
/// orphaning them from their remote.)
///
/// **Fallback `-b`** (`git worktree add -b <branch> <path> <start>`): the branch
/// is truly new — create it off `start` (the requested base) or `HEAD` (the
/// bare's default).
pub fn worktree_add(bare: &Path, path: &Path, branch: &str, start: Option<&str>) -> Result<()> {
    // Codify the contract: every caller composes paths under `GROVE_HOME`, which
    // is absolute by construction. A relative path here would be silently
    // resolved against `-C bare` (git's first-positional-arg rule) and create
    // the worktree somewhere unexpected — a foot-gun worth surfacing loudly.
    debug_assert!(
        bare.is_absolute(),
        "bare path must be absolute; got {}",
        bare.display()
    );
    debug_assert!(
        path.is_absolute(),
        "worktree path must be absolute; got {}",
        path.display()
    );

    // A `-`-leading `branch`/`start` is refused HERE, at the syscall boundary, and
    // not merely fenced by the `--` below. `--` protects the positionals after it —
    // but the `-b <branch>` fallback puts the branch name *before* it, and git
    // implements `-b` by shelling out to `git branch`, which re-parses a flag-shaped
    // name as an option: `branch = "-D"` with a base force-deletes that base from
    // the bare, unrecoverably (a bare clone keeps no reflog). The manifest write
    // boundary rejects such values, but the READ side is lenient by design — a
    // hand-edited manifest is declared input — so this is the backstop that has to
    // hold, and the `--` alone did not.
    for (kind, value) in [("branch", Some(branch)), ("base", start)] {
        if value.is_some_and(|v| v.is_empty() || v.starts_with('-')) {
            bail!("refusing git worktree add: {kind} must not be empty or begin with '-'");
        }
    }

    // `--` ends git's option parsing for everything after it, so a path or
    // start-point can never be read as a flag either.
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(bare)
        .args(["worktree", "add", "--"])
        .arg(path)
        .arg(branch);
    let dwim = run_git(cmd, None, "git worktree add (DWIM)")?;
    if dwim.status.success() {
        return Ok(());
    }

    // Only mint a NEW branch when DWIM failed because the revision is *unknown* (no
    // local head, no unique remote-tracking ref). Any OTHER failure — a transient
    // `index.lock`, a branch already checked out elsewhere, a locked ref — must NOT
    // be papered over by silently creating a fresh branch off `HEAD`; surface it so
    // the caller/operator sees the real cause instead of an orphaned new branch.
    let dwim_err = String::from_utf8_lossy(&dwim.stderr);
    if !is_unknown_revision(&dwim_err) {
        bail!("git worktree add failed: {}", dwim_err.trim());
    }

    // Brand-new branch: create it off `start` (or HEAD).
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(bare)
        .args(["worktree", "add", "-b", branch, "--"])
        .arg(path)
        .arg(start.unwrap_or("HEAD"));
    let out = run_git(cmd, None, "git worktree add (-b)")?;
    check_status(&out, "git worktree add")?;
    Ok(())
}

/// Does git's stderr say the requested revision doesn't exist — the *only* signal
/// that should route `worktree_add`/`attach_branch` into the new-branch (`-b`/`-c`)
/// fallback. A stray `index.lock`, a locked ref, or a branch already checked out
/// elsewhere are real failures that must surface, never silently mint a branch. git
/// phrases the miss as `fatal: invalid reference: <rev>` (2.39); the other forms
/// cover older/related wording so the gate degrades safely across git versions.
fn is_unknown_revision(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("invalid reference")
        || s.contains("not a valid ref")
        || s.contains("unknown revision")
        || s.contains("did not match any file")
}

/// Does `branch` already exist in `bare` — as a local head, or as the
/// `origin/<branch>` tracking ref a checkout DWIMs onto?
///
/// The gate in front of a [`worktree_add`] whose branch the caller must not invent:
/// that call's `-b` fallback mints a missing branch off `HEAD`, which is right for
/// `tree add` (a new branch is the point) and wrong for a trunk the operator named —
/// a typo would otherwise become a real branch and grove would integrate on it.
#[must_use]
pub fn branch_exists(bare: &Path, branch: &str) -> bool {
    ["refs/heads/", "refs/remotes/origin/"]
        .iter()
        .any(|prefix| rev_parse(bare, &format!("{prefix}{branch}")).is_ok())
}

/// The bare's default branch — its `HEAD` symbolic-ref shortened (`refs/heads/main`
/// → `main`). The warm-pool checkout target: a detached slot sits at this branch's
/// tip. Falls back to `main` when `HEAD` is detached/unreadable (a fresh bare clone
/// always has a symbolic `HEAD`, so the fallback is the pathological case).
pub fn default_branch(bare: &Path) -> Result<String> {
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(bare)
        .args(["symbolic-ref", "--short", "HEAD"]);
    let out = run_git(cmd, None, "git symbolic-ref HEAD")?;
    if !out.status.success() {
        return Ok("main".to_string());
    }
    Ok(stdout_trimmed(&out))
}

/// Point the bare's `HEAD` at `refs/heads/<branch>` — [`default_branch`]'s write-side
/// twin, and the whole of "actual state" for which branch a root integrates on.
///
/// `symbolic-ref` rather than `switch`/`checkout`: a bare repo has no working tree to
/// move, and the ref need not resolve yet — a root declaring a trunk the remote has
/// not published still records the intent, and the checkout that follows is what
/// fails loudly if the branch really is not there.
pub fn set_head(bare: &Path, branch: &str) -> Result<()> {
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(bare)
        .args(["symbolic-ref", "HEAD"])
        .arg(format!("refs/heads/{branch}"));
    let out = run_git(cmd, None, "git symbolic-ref HEAD")?;
    check_status(&out, "git symbolic-ref HEAD")?;
    Ok(())
}

/// Add a **detached** worktree at `path`, checked out at `commitish` (no branch).
/// The warm-pool primitive: a slot is a ready checkout with no ref attached, so it
/// stays out of the branch namespace until `worktree_move` + `attach_branch` claim
/// it. `--` stops a flag-shaped `commitish` being parsed as an option.
pub fn worktree_add_detached(bare: &Path, path: &Path, commitish: &str) -> Result<()> {
    debug_assert!(
        bare.is_absolute(),
        "bare must be absolute; got {}",
        bare.display()
    );
    debug_assert!(
        path.is_absolute(),
        "path must be absolute; got {}",
        path.display()
    );
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(bare)
        .args(["worktree", "add", "--detach", "--"])
        .arg(path)
        .arg(commitish);
    let out = run_git(cmd, None, "git worktree add --detach")?;
    check_status(&out, "git worktree add --detach")?;
    Ok(())
}

/// Move a registered worktree from `from` to `to`, rewriting git's `gitdir`/gitlink
/// pointers (a plain `rename(2)` would leave them dangling). The pool-promote
/// primitive: claim a warm slot by relocating it to the user's canonical worktree
/// path. `--` ends option parsing for safety, though both args are grove-composed.
pub fn worktree_move(bare: &Path, from: &Path, to: &Path) -> Result<()> {
    debug_assert!(
        from.is_absolute(),
        "from must be absolute; got {}",
        from.display()
    );
    debug_assert!(
        to.is_absolute(),
        "to must be absolute; got {}",
        to.display()
    );
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(bare)
        .args(["worktree", "move", "--"])
        .arg(from)
        .arg(to);
    let out = run_git(cmd, None, "git worktree move")?;
    check_status(&out, "git worktree move")?;
    Ok(())
}

/// Re-derive git's worktree pointers after a directory was moved by `rename(2)`
/// rather than by [`worktree_move`]: the gitlink each linked worktree holds at its
/// own `.git`, and — for each path in `moved` — the admin `gitdir` that points back
/// at it. Both are absolute paths git recorded, so a rename of the bare or of a
/// checkout leaves them dangling until this runs.
///
/// Git's own repair rather than a hand-written rewrite of those files: it owns their
/// format, it is idempotent (a pointer already correct is left alone, so a migration
/// that crashed halfway resumes), and it is the documented answer to exactly this
/// move. A pointer it cannot repair is reported on stderr and is **not** an error —
/// the caller's own verification of the resulting checkout is what decides whether
/// the tree came out sound.
pub fn worktree_repair(bare: &Path, moved: &[&Path]) -> Result<()> {
    debug_assert!(
        bare.is_absolute(),
        "bare must be absolute; got {}",
        bare.display()
    );
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(bare)
        .args(["worktree", "repair", "--"])
        .args(moved);
    let out = run_git(cmd, None, "git worktree repair")?;
    check_status(&out, "git worktree repair")?;
    Ok(())
}

/// Attach `branch` into the **already-checked-out** worktree at `wt` (a detached
/// slot just moved into place), mirroring [`worktree_add`]'s DWIM semantics so a
/// warm promote of an existing branch behaves identically to a cold checkout:
///
/// - **DWIM first** (`git -C <wt> switch <branch>`): switches to an existing local
///   head, or — for a branch that lives only as `refs/remotes/<remote>/<branch>` —
///   creates a local tracking branch off it (git's guess-remote default). A bare
///   `checkout -b` here would orphan a remote-only branch from its upstream, the
///   exact divergence the cold path's DWIM avoids.
/// - **Fallback `-c`** (`git -C <wt> switch -c <branch> <start>`): the branch is
///   truly new — create it off `start` (the requested base) or the current detached
///   HEAD (the warm slot's tip = the default-branch tip).
pub fn attach_branch(wt: &Path, branch: &str, start: Option<&str>) -> Result<()> {
    debug_assert!(
        wt.is_absolute(),
        "worktree must be absolute; got {}",
        wt.display()
    );
    let mut cmd = git_command();
    cmd.arg("-C").arg(wt).args(["switch", "--"]).arg(branch);
    let dwim = run_git(cmd, None, "git switch (DWIM)")?;
    if dwim.status.success() {
        return Ok(());
    }

    // Same gate as `worktree_add`: only create a NEW branch (`-c`) when the DWIM
    // switch failed because the revision is unknown. A branch already checked out
    // elsewhere, or any transient fault, surfaces as-is — never a silent new branch.
    let dwim_err = String::from_utf8_lossy(&dwim.stderr);
    if !is_unknown_revision(&dwim_err) {
        bail!("git switch failed: {}", dwim_err.trim());
    }

    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(wt)
        .args(["switch", "-c", branch, "--"])
        .arg(start.unwrap_or("HEAD"));
    let out = run_git(cmd, None, "git switch -c")?;
    check_status(&out, "git switch")?;
    Ok(())
}

/// The linked worktrees of `bare`, parsed from `git worktree list --porcelain`.
/// The bare entry itself is omitted; callers filter grove's own dirs by path.
pub fn worktree_list(bare: &Path) -> Result<Vec<GitWorktree>> {
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(bare)
        .args(["worktree", "list", "--porcelain"]);
    let out = run_git(cmd, None, "git worktree list")?;
    check_status(&out, "git worktree list")?;
    Ok(parse_worktree_list(&out.stdout))
}

/// One porcelain entry accumulated across its attribute lines.
#[derive(Default)]
struct WorktreeEntry {
    path: Option<PathBuf>,
    branch: Option<String>,
    is_bare: bool,
    /// The `worktree` line's path was not valid UTF-8 — drop the whole entry.
    skip: bool,
}

impl WorktreeEntry {
    fn finish(self, out: &mut Vec<GitWorktree>) {
        if let (Some(path), false, false) = (self.path, self.is_bare, self.skip) {
            out.push(GitWorktree {
                path,
                branch: self.branch,
            });
        }
    }
}

/// Parse `git worktree list --porcelain` output. Byte-level on purpose: a worktree
/// whose `worktree <path>` line is not valid UTF-8 is **skipped** (with a stderr
/// WARN naming it lossily), never lossy-decoded. A `String::from_utf8_lossy` path
/// would mint U+FFFD-laced text that slips past `worktrees::adoptable_name`'s
/// `to_str` guard, so a non-UTF-8 out-of-band worktree would be adopted under a name
/// that maps to no real directory — and its delete would never stick (remove → prune
/// finds nothing → next reconcile re-adopts). Declared worktree names are always
/// UTF-8, so declared-worktree presence is unaffected.
///
/// Porcelain: one `worktree <path>` line per entry, then `bare` / `branch
/// refs/heads/<n>` / `detached`, entries separated by a blank line.
fn parse_worktree_list(stdout: &[u8]) -> Vec<GitWorktree> {
    let mut worktrees = Vec::new();
    let mut cur = WorktreeEntry::default();

    for line in stdout.split(|&b| b == b'\n') {
        if let Some(raw) = line.strip_prefix(b"worktree ".as_slice()) {
            std::mem::take(&mut cur).finish(&mut worktrees);
            if let Ok(p) = std::str::from_utf8(raw) {
                cur.path = Some(PathBuf::from(p));
            } else {
                eprintln!(
                    "grove-ops WARN worktree_list: skipping worktree with non-UTF-8 path {:?}",
                    String::from_utf8_lossy(raw)
                );
                cur.skip = true;
            }
        } else if line == b"bare".as_slice() {
            cur.is_bare = true;
        } else if let Some(raw) = line.strip_prefix(b"branch ".as_slice()) {
            let name = String::from_utf8_lossy(raw);
            let name = name.strip_prefix("refs/heads/").unwrap_or(name.as_ref());
            cur.branch = Some(name.to_string());
        }
    }
    cur.finish(&mut worktrees);
    worktrees
}

/// The URL of a remote in `bare`, or an error when the remote isn't configured.
/// Used by root adoption to recover a `url` for a bare repo found on disk that
/// isn't declared in the manifest.
pub fn remote_url(bare: &Path, remote: &str) -> Result<String> {
    let mut cmd = git_command();
    cmd.arg("-C").arg(bare).args(["remote", "get-url", remote]);
    let out = run_git(cmd, None, "git remote get-url")?;
    if !out.status.success() {
        bail!(
            "no remote {remote} in {}: {}",
            bare.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(stdout_trimmed(&out))
}

/// Prune stale worktree administrative entries under the bare's `worktrees/` whose working
/// tree is gone (`git worktree prune`). Clears the registration git keeps after a
/// worktree directory is removed out-of-band (a stray `rm -rf`): without it, that
/// registration survives so a re-`add` at the path is refused *and* reconcile would
/// re-adopt the deleted worktree. Only genuinely-missing trees are pruned — a live
/// worktree at any path is untouched, so this is safe to call broadly.
pub fn worktree_prune(bare: &Path) -> Result<()> {
    let mut cmd = git_command();
    cmd.arg("-C").arg(bare).args(["worktree", "prune"]);
    let out = run_git(cmd, None, "git worktree prune")?;
    check_status(&out, "git worktree prune")?;
    Ok(())
}

/// Remove the worktree at `path` from `bare` (refuses if it has changes).
pub fn worktree_remove(bare: &Path, path: &Path) -> Result<()> {
    let mut cmd = git_command();
    cmd.arg("-C")
        .arg(bare)
        .args(["worktree", "remove"])
        .arg(path);
    let out = run_git(cmd, None, "git worktree remove")?;
    check_status(&out, "git worktree remove")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git(cwd: &Path, args: &[&str]) {
        let ok = git_command()
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// A local source repo with one commit on `main`, to clone in tests.
    fn fixture_repo(dir: &Path) {
        crate::testfix::fixture_repo(dir);
    }

    #[test]
    fn parse_timeout_defaults_and_disables() {
        assert_eq!(
            parse_timeout(None, DEFAULT_CLONE_TIMEOUT_SECS),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(
            parse_timeout(Some("120".into()), DEFAULT_CLONE_TIMEOUT_SECS),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_timeout(Some("0".into()), DEFAULT_CLONE_TIMEOUT_SECS),
            None,
            "0 disables the cap"
        );
        // garbage falls back to the default rather than disabling protection
        assert_eq!(
            parse_timeout(Some("nonsense".into()), DEFAULT_FETCH_TIMEOUT_SECS),
            Some(Duration::from_secs(600))
        );
    }

    /// The bounded runner reads its budget off the clock seam: a wedged child is
    /// killed when the *clock* says the deadline passed, not when wall time does.
    /// The 60 s cap is stepped over by hand, so this costs milliseconds and can't
    /// flake — the payoff `clock.rs` promises, exercised on the one loop that has
    /// a budget today. A regression to `Instant::now()` doesn't just fail this
    /// assertion, it wedges the test on the child's own 30 s sleep.
    #[test]
    fn a_wedged_child_is_killed_when_the_clock_passes_the_deadline() {
        let clock = crate::clock::TestClock::new();
        let deadline = clock.deadline(Duration::from_secs(60));
        clock.advance(Duration::from_secs(61));

        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let err = run_until(cmd, Some(deadline), &clock, "sleep").unwrap_err();

        assert!(
            format!("{err:#}").contains("aborted: exceeded its timeout"),
            "timeout error, got {err:#}"
        );
    }

    /// An unexpired deadline still lets the child finish normally — the loop polls
    /// `try_wait` and only the seam decides when to kill.
    #[test]
    fn a_child_that_finishes_inside_its_budget_is_not_killed() {
        let clock = crate::clock::TestClock::new();
        let deadline = clock.deadline(Duration::from_secs(60));

        let mut cmd = Command::new("sleep");
        cmd.arg("0");
        let out = run_until(cmd, Some(deadline), &clock, "sleep").unwrap();
        assert!(out.status.success());
    }

    /// The clone watchdog trips off the same seam: a `TestClock` already past the
    /// deadline flips gix's interrupt flag on the watchdog's first pass.
    #[test]
    fn the_clone_watchdog_trips_the_interrupt_on_the_clocks_deadline() {
        let clock = Arc::new(crate::clock::TestClock::new());
        let deadline = clock.deadline(Duration::from_secs(3600));
        clock.advance(Duration::from_secs(3601));

        let interrupt = Arc::new(AtomicBool::new(false));
        let _watchdog = Watchdog::spawn(
            interrupt.clone(),
            deadline,
            Arc::clone(&clock) as Arc<dyn Clock>,
        );

        // Bounded spin (≈1 s) rather than a wall-clock read: `Instant::now()` lives
        // only in `clock.rs`, and the harness meta-check enforces that.
        for _ in 0..200 {
            if interrupt.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("watchdog never tripped on an expired deadline");
    }

    /// Every porcelain-v2 line kind in one fixture: headers, an index-only change
    /// (`M.`), a worktree-only change (`.M`), a both-columns change (`MM`, which is
    /// one path staged *and* unstaged), a rename (`2`), an unmerged path, an
    /// untracked path, and an ignored one.
    #[test]
    fn parse_status_counts_every_line_kind() {
        let out = b"# branch.oid abc123\n\
                    # branch.head main\n\
                    # branch.upstream origin/main\n\
                    # branch.ab +3 -2\n\
                    1 M. N... 100644 100644 100644 aaa bbb staged.txt\n\
                    1 .M N... 100644 100644 100644 aaa bbb unstaged.txt\n\
                    1 MM N... 100644 100644 100644 aaa bbb both.txt\n\
                    2 R. N... 100644 100644 100644 aaa bbb R100 new.txt\told.txt\n\
                    u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.txt\n\
                    ? untracked.txt\n\
                    ! ignored.txt\n";

        assert_eq!(
            parse_status(out),
            Status {
                ahead: 3,
                behind: 2,
                branch: Some("main".into()),
                conflicted: 1,
                head: Some("abc123".into()),
                staged: 3,
                unstaged: 2,
                untracked: 1,
                upstream: Some("origin/main".into()),
            },
            "`MM` counts once per column; `u` counts only as conflicted; `!` is ignored"
        );
    }

    /// git's sentinels for "no value" must read as `None`, not as literal text —
    /// an unborn branch has no head, a detached HEAD has no branch, and a branch
    /// with no upstream omits `branch.upstream`/`branch.ab` entirely.
    #[test]
    fn parse_status_reads_sentinels_as_absent() {
        let unborn = parse_status(b"# branch.oid (initial)\n# branch.head main\n");
        assert_eq!(unborn.head, None);
        assert_eq!(unborn.branch, Some("main".into()));

        let detached = parse_status(b"# branch.oid abc123\n# branch.head (detached)\n");
        assert_eq!(detached.branch, None);
        assert_eq!(detached.head, Some("abc123".into()));

        // No upstream: the counts sit at 0, and only `upstream: None` distinguishes
        // that from a branch genuinely level with its remote.
        assert_eq!(detached.upstream, None);
        assert_eq!((detached.ahead, detached.behind), (0, 0));
    }

    /// The parser against a real `git status` rather than a handwritten fixture —
    /// this is what pins the flag set (`--porcelain=v2 --branch`) to the output
    /// shape the parser expects.
    #[test]
    fn status_reads_a_live_checkout() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("src");
        fixture_repo(&repo);
        let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];

        let clean = status(&repo).unwrap();
        assert_eq!((clean.staged, clean.unstaged, clean.untracked), (0, 0, 0));
        assert_eq!(clean.branch, Some("main".into()));
        assert!(clean.head.is_some(), "a committed repo has a head");
        assert_eq!(clean.upstream, None, "a local fixture tracks nothing");

        // One staged add, one unstaged edit of a tracked file, one untracked file.
        std::fs::write(repo.join("STAGED.md"), "new").unwrap();
        git(&repo, &[&id[..], &["add", "STAGED.md"]].concat());
        std::fs::write(repo.join("README.md"), "edited").unwrap();
        std::fs::write(repo.join("UNTRACKED.md"), "loose").unwrap();

        let dirty = status(&repo).unwrap();
        assert_eq!(
            (dirty.staged, dirty.unstaged, dirty.untracked),
            (1, 1, 1),
            "each change lands in exactly one bucket"
        );
        assert_eq!(dirty.conflicted, 0);
    }

    /// Ahead/behind come from `# branch.ab`, which only appears once the branch
    /// has an upstream — so this walks a real clone away from its remote.
    #[test]
    fn status_counts_ahead_and_behind_against_upstream() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);
        let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];

        let clone = tmp.path().join("clone");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                src.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );

        // The clone commits once (ahead), then the source commits twice (behind,
        // once the clone fetches those commits without merging them).
        std::fs::write(clone.join("MINE.md"), "mine").unwrap();
        git(&clone, &[&id[..], &["add", "."]].concat());
        git(&clone, &[&id[..], &["commit", "-q", "-m", "mine"]].concat());

        for n in ["one", "two"] {
            std::fs::write(src.join(format!("{n}.md")), n).unwrap();
            git(&src, &[&id[..], &["add", "."]].concat());
            git(&src, &[&id[..], &["commit", "-q", "-m", n]].concat());
        }
        git(&clone, &["fetch", "-q"]);

        let drifted = status(&clone).unwrap();
        assert_eq!((drifted.ahead, drifted.behind), (1, 2));
        assert_eq!(drifted.upstream, Some("origin/main".into()));
    }

    #[test]
    fn clone_bare_returns_default_branch() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);

        let dest = tmp.path().join("repo.git");
        let branch = clone_bare(src.to_str().unwrap(), &dest).unwrap();

        assert!(dest.join("HEAD").exists());
        assert!(gix::open(&dest).unwrap().is_bare());
        assert_eq!(branch, "main");
    }

    #[test]
    fn worktree_add_list_remove() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);
        let bare = tmp.path().join("repo.git");
        let default = clone_bare(src.to_str().unwrap(), &bare).unwrap();

        // New branch off the default — checks out + appears in the porcelain list.
        let wt = tmp.path().join("feature");
        worktree_add(&bare, &wt, "feature", Some(&default)).unwrap();
        assert!(wt.join("README.md").exists(), "new worktree is checked out");

        let listed = worktree_list(&bare).unwrap();
        assert_eq!(listed.len(), 1, "bare itself is excluded");
        assert_eq!(listed[0].branch.as_deref(), Some("feature"));
        assert_eq!(
            listed[0].path.canonicalize().unwrap(),
            wt.canonicalize().unwrap()
        );

        worktree_remove(&bare, &wt).unwrap();
        assert!(worktree_list(&bare).unwrap().is_empty());
    }

    /// DWIM: a branch that exists only as `refs/remotes/origin/<x>` must be
    /// checked out as a local tracking branch, not silently re-created off HEAD.
    /// Regression guard for the `branch_exists` shape that gated this away.
    #[test]
    fn worktree_add_dwims_remote_tracking_branch() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);
        let bare = tmp.path().join("repo.git");
        clone_bare(src.to_str().unwrap(), &bare).unwrap();

        // Simulate a branch that only lives on the remote — no local head.
        git(
            &bare,
            &["update-ref", "refs/remotes/origin/remote-only", "HEAD"],
        );
        assert!(
            git_command()
                .arg("-C")
                .arg(&bare)
                .args(["rev-parse", "--verify", "--quiet", "remote-only"])
                .status()
                .unwrap()
                .code()
                .unwrap_or(1)
                != 0,
            "precondition: `remote-only` must not resolve as a local head"
        );

        let wt = tmp.path().join("remote-only");
        worktree_add(&bare, &wt, "remote-only", None).unwrap();

        // Local head now exists (DWIM created it), tracking origin/remote-only.
        let head = String::from_utf8(
            git_command()
                .arg("-C")
                .arg(&wt)
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(head.trim(), "remote-only");
        let upstream = String::from_utf8(
            git_command()
                .arg("-C")
                .arg(&wt)
                .args(["rev-parse", "--abbrev-ref", "remote-only@{upstream}"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(upstream.trim(), "origin/remote-only");
    }

    #[test]
    fn default_branch_reads_bare_head() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);
        let bare = tmp.path().join("repo.git");
        clone_bare(src.to_str().unwrap(), &bare).unwrap();
        assert_eq!(default_branch(&bare).unwrap(), "main");
    }

    #[test]
    fn detach_move_attach_promote_flow() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);
        let bare = tmp.path().join("repo.git");
        clone_bare(src.to_str().unwrap(), &bare).unwrap();

        // A warm slot: detached at the default-branch tip, no branch.
        let slot = tmp.path().join("slot");
        worktree_add_detached(&bare, &slot, "main").unwrap();
        assert!(slot.join("README.md").exists(), "warm checkout present");
        assert!(
            worktree_list(&bare)
                .unwrap()
                .iter()
                .any(|w| w.branch.is_none()),
            "slot is detached"
        );

        // Promote: move it, then attach a NEW branch.
        let dest = tmp.path().join("dest");
        worktree_move(&bare, &slot, &dest).unwrap();
        assert!(!slot.exists() && dest.join("README.md").exists());
        attach_branch(&dest, "feature/x", Some("main")).unwrap();
        let on = worktree_list(&bare)
            .unwrap()
            .into_iter()
            .find(|w| w.path.canonicalize().unwrap() == dest.canonicalize().unwrap())
            .unwrap();
        assert_eq!(on.branch.as_deref(), Some("feature/x"), "branch attached");
    }

    /// `attach_branch` must DWIM a remote-only branch into a tracking branch —
    /// identical to `worktree_add`'s cold-path semantics, so warm == cold.
    #[test]
    fn attach_branch_dwims_remote_tracking_branch() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);
        let bare = tmp.path().join("repo.git");
        clone_bare(src.to_str().unwrap(), &bare).unwrap();
        git(
            &bare,
            &["update-ref", "refs/remotes/origin/remote-only", "HEAD"],
        );

        let slot = tmp.path().join("slot");
        worktree_add_detached(&bare, &slot, "main").unwrap();
        attach_branch(&slot, "remote-only", None).unwrap();

        let upstream = String::from_utf8(
            git_command()
                .arg("-C")
                .arg(&slot)
                .args(["rev-parse", "--abbrev-ref", "remote-only@{upstream}"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(
            upstream.trim(),
            "origin/remote-only",
            "tracks the remote ref"
        );
    }

    /// A flag-shaped `branch` must never be parsed as a `git worktree add` option —
    /// and the case that matters is the DESTRUCTIVE one, not the harmless one.
    ///
    /// `--no-checkout` merely fails; `-D` with a base does not. The DWIM attempt
    /// fails with "invalid reference", which routes into the `-b` fallback, and
    /// `git worktree add -b -D -- <path> <base>` reaches `git branch -D <base>`:
    /// the base is force-deleted from the bare, unmerged commits and all, with no
    /// reflog in a bare clone to get it back. `--` sits after `-b`'s value and
    /// cannot protect it, so the refusal has to happen before git is spawned.
    #[test]
    fn worktree_add_refuses_a_dashed_branch_before_git_can_delete_one() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);
        let bare = tmp.path().join("repo.git");
        clone_bare(src.to_str().unwrap(), &bare).unwrap();
        let victim = branch_names(&bare);
        assert!(victim.contains(&"main".to_string()), "{victim:?}");

        let wt = tmp.path().join("x");
        assert!(
            worktree_add(&bare, &wt, "-D", Some("main")).is_err(),
            "flag-shaped branch must fail, not reach `git branch -D`"
        );
        assert_eq!(
            branch_names(&bare),
            victim,
            "the base branch survives — nothing was force-deleted"
        );
        assert!(
            worktree_add(&bare, &wt, "--no-checkout", None).is_err(),
            "flag-shaped branch must fail, not silently apply a git flag"
        );
        assert!(
            worktree_add(&bare, &wt, "feat", Some("-D")).is_err(),
            "a flag-shaped base is refused on the same boundary"
        );
        assert!(
            !wt.exists(),
            "no worktree created from a flag-shaped branch"
        );
    }

    /// Local branch names in a bare, sorted — the before/after of the guard above.
    fn branch_names(bare: &Path) -> Vec<String> {
        let out = git_command()
            .arg("-C")
            .arg(bare)
            .args(["branch", "--format=%(refname:short)"])
            .output()
            .unwrap();
        let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        names.sort();
        names
    }

    /// The stderr we parse must be locale-stable: every git invocation pins `LC_ALL=C`
    /// (and `LANG=C`), so a non-English git can't localize "invalid reference" out from
    /// under `is_unknown_revision` and turn a real branch-create into a hard failure.
    #[test]
    fn git_command_pins_a_c_locale() {
        let cmd = git_command();
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(
            envs.contains(&("LC_ALL".into(), Some("C".into()))),
            "LC_ALL=C pinned: {envs:?}"
        );
        assert!(
            envs.contains(&("LANG".into(), Some("C".into()))),
            "LANG=C pinned: {envs:?}"
        );
        // And the local-repo scrub is applied (an `env_remove` reads as `None`):
        // an inherited `GIT_DIR`/`GIT_INDEX_FILE` (grove run from a git hook)
        // must never leak into the spawn and redirect it off the `-C` repo.
        for var in LOCAL_REPO_ENV {
            assert!(
                envs.contains(&((*var).into(), None)),
                "{var} scrubbed: {envs:?}"
            );
        }
    }

    /// `LOCAL_REPO_ENV` mirrors the installed git's own authoritative list, so a
    /// git upgrade that grows a new repo-pinning variable is caught here instead
    /// of leaking through a hook environment unscrubbed.
    #[test]
    fn local_repo_env_matches_the_installed_gits_list() {
        let out = Command::new("git")
            .arg("rev-parse")
            .arg("--local-env-vars")
            .output()
            .unwrap();
        let unscrubbed: Vec<&str> = std::str::from_utf8(&out.stdout)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty() && !LOCAL_REPO_ENV.contains(l))
            .collect();
        // Subset, not equality: an older/newer git may drop variables from its
        // list (ours scrubbing extra names is harmless), but every variable the
        // INSTALLED git treats as repo-pinning must be covered.
        assert_eq!(unscrubbed, Vec::<&str>::new());
    }

    /// The DWIM→`-b` fallback must fire ONLY on an unknown revision — never on a
    /// transient/locked/already-checked-out failure, which would silently mint a
    /// fresh branch off HEAD.
    #[test]
    fn is_unknown_revision_gates_the_new_branch_fallback() {
        assert!(is_unknown_revision("fatal: invalid reference: feature/x"));
        assert!(is_unknown_revision("fatal: Not a valid ref: refs/heads/x"));
        // Non-revision failures must NOT be treated as unknown.
        assert!(!is_unknown_revision(
            "fatal: Unable to create '.git/index.lock': File exists."
        ));
        assert!(!is_unknown_revision(
            "fatal: 'x' is already checked out at '/path/x'"
        ));
    }

    /// The `root.sync` git triple: fetch moves the remote-tracking ref, the ff
    /// moves the checkout, and re-running reports `AlreadyCurrent` (idempotent).
    #[test]
    fn fetch_then_fast_forward_updates_a_trunk_checkout() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);
        let bare = tmp.path().join("repo.git");
        clone_bare(src.to_str().unwrap(), &bare).unwrap();
        let trunk = tmp.path().join("trunk");
        worktree_add(&bare, &trunk, "main", None).unwrap();

        // The remote moves ahead.
        std::fs::write(src.join("NEW.md"), "ahead").unwrap();
        let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
        git(&src, &[&id[..], &["add", "."]].concat());
        git(&src, &[&id[..], &["commit", "-q", "-m", "ahead"]].concat());

        fetch(&bare, "origin", "main").unwrap();
        assert_eq!(
            fast_forward(&trunk, "origin/main").unwrap(),
            FastForward::Updated
        );
        assert!(
            trunk.join("NEW.md").exists(),
            "trunk carries the new commit"
        );
        assert_eq!(
            rev_parse(&trunk, "HEAD").unwrap(),
            rev_parse(&src, "HEAD").unwrap(),
            "trunk is at the remote tip"
        );

        // Idempotent: nothing new to fetch/ff.
        fetch(&bare, "origin", "main").unwrap();
        assert_eq!(
            fast_forward(&trunk, "origin/main").unwrap(),
            FastForward::AlreadyCurrent
        );
    }

    /// Diverged and dirty trunks are *reported*, never reset — the never-clobber
    /// contract of `root.sync`.
    #[test]
    fn fast_forward_reports_diverged_and_dirty_without_touching_the_tree() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);
        let bare = tmp.path().join("repo.git");
        clone_bare(src.to_str().unwrap(), &bare).unwrap();
        let trunk = tmp.path().join("trunk");
        worktree_add(&bare, &trunk, "main", None).unwrap();
        let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];

        // Dirty: an uncommitted *tracked* edit blocks the ff before any merge runs.
        // (An untracked file no longer counts — see `fast_forward_ignores_an_untracked_file`.)
        std::fs::write(trunk.join("README.md"), "wip").unwrap();
        assert_eq!(
            fast_forward(&trunk, "origin/main").unwrap(),
            FastForward::Dirty
        );
        assert_eq!(
            std::fs::read_to_string(trunk.join("README.md")).unwrap(),
            "wip",
            "dirty tree untouched"
        );

        // Diverged: a local commit in the trunk + a different remote commit.
        git(&trunk, &[&id[..], &["add", "."]].concat());
        git(
            &trunk,
            &[&id[..], &["commit", "-q", "-m", "local"]].concat(),
        );
        std::fs::write(src.join("REMOTE.md"), "remote").unwrap();
        git(&src, &[&id[..], &["add", "."]].concat());
        git(&src, &[&id[..], &["commit", "-q", "-m", "remote"]].concat());
        fetch(&bare, "origin", "main").unwrap();

        let local_tip = rev_parse(&trunk, "HEAD").unwrap();
        assert_eq!(
            fast_forward(&trunk, "origin/main").unwrap(),
            FastForward::Diverged
        );
        assert_eq!(
            rev_parse(&trunk, "HEAD").unwrap(),
            local_tip,
            "diverged trunk left where it was"
        );
    }

    /// A branch already checked out elsewhere makes DWIM fail for a *non*-unknown
    /// reason. The gate must surface that cause and NOT route into the new-branch
    /// fallback (which pre-fix would have masked it / risked minting a branch).
    #[test]
    fn worktree_add_of_an_already_checked_out_branch_surfaces_not_mints() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);
        let bare = tmp.path().join("repo.git");
        clone_bare(src.to_str().unwrap(), &bare).unwrap();

        // Create branch `shared` and check it out in worktree A.
        let a = tmp.path().join("a");
        worktree_add(&bare, &a, "shared", Some("main")).unwrap();

        // A second checkout of `shared` (held by A) errors with the real cause.
        let b = tmp.path().join("b");
        let err = worktree_add(&bare, &b, "shared", Some("main")).unwrap_err();
        // git reworded this (older: "already checked out"; newer: "already used
        // by worktree") — accept both.
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("already checked out")
                || rendered.contains("already used by worktree"),
            "surfaces the real cause, not a new-branch error: {err:#}"
        );
        assert!(!b.exists(), "no worktree minted");
    }

    /// An untracked file in the trunk must NOT gate an ff. grove materializes share
    /// sources as untracked files in the trunk; counting them dirty would wedge
    /// every future sync on a repo that doesn't gitignore that path.
    #[test]
    fn fast_forward_ignores_an_untracked_file() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);
        let bare = tmp.path().join("repo.git");
        clone_bare(src.to_str().unwrap(), &bare).unwrap();
        let trunk = tmp.path().join("trunk");
        worktree_add(&bare, &trunk, "main", None).unwrap();

        // A grove-materialized share source: an untracked file in the trunk.
        std::fs::write(trunk.join(".env"), "SECRET").unwrap();

        // The remote moves ahead on a DIFFERENT (tracked) path.
        std::fs::write(src.join("NEW.md"), "ahead").unwrap();
        let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
        git(&src, &[&id[..], &["add", "."]].concat());
        git(&src, &[&id[..], &["commit", "-q", "-m", "ahead"]].concat());

        fetch(&bare, "origin", "main").unwrap();
        assert_eq!(
            fast_forward(&trunk, "origin/main").unwrap(),
            FastForward::Updated,
            "untracked file does not gate the ff"
        );
        assert!(trunk.join("NEW.md").exists(), "ff applied");
        assert_eq!(
            std::fs::read_to_string(trunk.join(".env")).unwrap(),
            "SECRET",
            "untracked file preserved"
        );
    }

    /// An untracked file the incoming commits WOULD overwrite yields `Dirty` and
    /// leaves the tree untouched — git refuses the ff, and grove maps that refusal to
    /// a reported dirty outcome, never a clobber and never a hard error.
    #[test]
    fn fast_forward_reports_dirty_when_an_incoming_commit_would_clobber_an_untracked_file() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fixture_repo(&src);
        let bare = tmp.path().join("repo.git");
        clone_bare(src.to_str().unwrap(), &bare).unwrap();
        let trunk = tmp.path().join("trunk");
        worktree_add(&bare, &trunk, "main", None).unwrap();

        // The remote adds a tracked file `CONFLICT.md`...
        std::fs::write(src.join("CONFLICT.md"), "from remote").unwrap();
        let id = ["-c", "user.email=t@grove", "-c", "user.name=grove"];
        git(&src, &[&id[..], &["add", "."]].concat());
        git(
            &src,
            &[&id[..], &["commit", "-q", "-m", "adds CONFLICT.md"]].concat(),
        );
        fetch(&bare, "origin", "main").unwrap();

        // ...while the trunk holds an UNtracked file at the same path.
        std::fs::write(trunk.join("CONFLICT.md"), "local untracked").unwrap();
        let before = rev_parse(&trunk, "HEAD").unwrap();

        assert_eq!(
            fast_forward(&trunk, "origin/main").unwrap(),
            FastForward::Dirty,
            "an ff that would overwrite an untracked file is Dirty, not an error"
        );
        assert_eq!(rev_parse(&trunk, "HEAD").unwrap(), before, "HEAD not moved");
        assert_eq!(
            std::fs::read_to_string(trunk.join("CONFLICT.md")).unwrap(),
            "local untracked",
            "untracked file left untouched"
        );
    }

    /// The byte-level porcelain parser must SKIP a worktree whose path is not valid
    /// UTF-8 — never lossy-decode it into a U+FFFD path that would slip past
    /// `adoptable_name`'s `to_str` guard and get adopted under a name mapping to no
    /// real dir. APFS refuses invalid-UTF-8 names, so the parser is tested directly
    /// with a crafted porcelain byte string.
    #[test]
    fn parse_worktree_list_skips_a_non_utf8_path() {
        let mut bytes = Vec::new();
        // The bare entry (excluded), a valid worktree, and a non-UTF-8-path one.
        bytes.extend_from_slice(b"worktree /home/u/code/o/r/.bare\nbare\n\n");
        bytes.extend_from_slice(b"worktree /home/u/code/o/r/feat\nbranch refs/heads/feature/x\n\n");
        bytes.extend_from_slice(b"worktree /home/u/code/o/r/bad\xff\nbranch refs/heads/bad\n\n");

        let wts = parse_worktree_list(&bytes);
        assert_eq!(wts.len(), 1, "bare excluded, non-UTF-8 skipped: {wts:?}");
        assert_eq!(wts[0].path, PathBuf::from("/home/u/code/o/r/feat"));
        assert_eq!(wts[0].branch.as_deref(), Some("feature/x"));
    }

    /// A detached worktree parses with `branch: None` (its presence is downstream's
    /// to interpret) — the parser must retain it, not drop it.
    #[test]
    fn parse_worktree_list_retains_a_detached_worktree() {
        let bytes = b"worktree /home/u/code/o/r/slot\ndetached\n\n";
        let wts = parse_worktree_list(bytes);
        assert_eq!(wts.len(), 1);
        assert_eq!(wts[0].path, PathBuf::from("/home/u/code/o/r/slot"));
        assert!(wts[0].branch.is_none(), "detached → no branch");
    }
}
