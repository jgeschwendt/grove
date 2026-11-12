//! grove — the CLI, and the single shipped binary. `grove serve` runs the daemon
//! in-process; every other subcommand declares into `manifest.toml` and either nudges
//! a reachable server or realizes inline through grove-ops.
//!
//! This file is the command *surface* and nothing else: the clap tree, the dispatch,
//! and `serve`. The behaviour behind each command lives beside its own reasoning —
//! [`commands`] holds the single-realizer gate, [`render`] what gets printed and the
//! exit verdicts, [`api`] the HTTP client, [`server`] process custody, [`setup`] the
//! install and upgrade verbs, and [`timeouts`] every budget any of them spends.

use std::fs::File;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use clap::{Parser, Subcommand};
use grove_daemon::logs::DEFAULT_LEVEL;
use grove_daemon::{Config, Daemon, LogRing};
use grove_ops::clock::{Clock, SystemClock};
use tokio::signal::unix::{SignalKind, signal};

mod api;
mod commands;
mod error;
mod render;
mod server;
mod setup;
mod timeouts;

pub use api::{ApiClient, Reachability};
pub use error::CliError;
pub use server::ServerControl;

/// The version `grove --version` and `grove version` report: the release's `YYYY.M.N`,
/// stamped through `GROVE_VERSION` by the release workflow, else the crate's.
pub const VERSION: &str = grove_daemon::VERSION;

#[derive(Debug, Parser)]
#[command(name = "grove", version = VERSION, about = "Cultivate git worktrees")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Realize the manifest: clone any declared-but-missing repository.
    Apply,
    /// Manage cloned repositories.
    Clone {
        #[command(subcommand)]
        action: CloneAction,
    },
    /// Diagnose + safe-fix the worktree environment (shares); report conflicts.
    Doctor {
        /// Repository slug (`owner/name`); default: all declared roots.
        slug: Option<String>,
        /// Diagnose only — mutate nothing.
        #[arg(long)]
        dry_run: bool,
        /// Resolve conflicts aggressively (back up the real file, then link).
        #[arg(long)]
        fix: bool,
    },
    /// Stop the running server.
    Off,
    /// Check server health.
    Ok,
    /// Start the server in the background.
    On,
    /// Restart the server.
    Reboot,
    /// Run the server in the foreground (the launcher contract: `grove serve`).
    Serve,
    /// Install the launchd agents and link `~/.local/bin/grove` to mise's shim.
    Setup {
        /// Undo it: boot the agents out, delete their plists and the link.
        #[arg(long)]
        remove: bool,
    },
    /// Fetch a root and fast-forward its trunk checkout (never forced).
    Sync {
        /// Repository slug (`owner/name`).
        slug: String,
    },
    /// Manage worktrees under a root.
    Tree {
        #[command(subcommand)]
        action: TreeAction,
    },
    /// Move to mise's latest release: drain, swap, smoke, settle or roll back.
    #[command(group(clap::ArgGroup::new("mode").multiple(false)))]
    Upgrade {
        /// Report what would happen; touch nothing.
        #[arg(long, group = "mode")]
        check: bool,
        /// Roll back to the previous version.
        #[arg(long, group = "mode")]
        back: bool,
        /// Upgrade past a red hold.
        #[arg(long, group = "mode")]
        retry: bool,
    },
    /// Print the grove version.
    Version,
    /// Write a VS Code workspace over every checkout grove holds.
    Workspace {
        /// Where to write it. Default: `$GROVE_HOME/grove.code-workspace`.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

/// `grove clone <action>`.
#[derive(Debug, PartialEq, Eq, Subcommand)]
enum CloneAction {
    /// Clone and track a repository.
    Add {
        /// Repository URL, or GitHub `owner/name` (cloned over ssh); a local path that
        /// shape needs a leading `./`.
        repo: String,
    },
    /// Stop tracking and remove a repository.
    Remove {
        /// Repository slug (`owner/name`).
        repo: String,
        /// Delete even when the root holds uncommitted, unpushed, stashed or foreign work.
        #[arg(long)]
        force: bool,
    },
}

/// `grove tree <action>`.
#[derive(Debug, PartialEq, Eq, Subcommand)]
enum TreeAction {
    /// Create a worktree (branch checkout) under a root.
    Add {
        /// Repository slug (`owner/name`).
        repo: String,
        /// Branch to check out — created from `--base` if it doesn't exist.
        branch: String,
        /// Base to fork a new branch from (default: the root's default branch).
        #[arg(long)]
        base: Option<String>,
    },
    /// List worktrees under a root.
    List {
        /// Repository slug (`owner/name`).
        repo: String,
        /// Also print the root's own directory (`roots/<slug>`) under the trunk.
        #[arg(long)]
        verbose: bool,
    },
    /// Remove a worktree.
    Remove {
        /// Repository slug (`owner/name`).
        repo: String,
        /// Worktree name.
        name: String,
    },
}

/// The CLI entry point, split from `main` so it is reachable from tests.
#[must_use]
pub fn run() -> ExitCode {
    match dispatch(&Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("grove: {e}");
            ExitCode::from(e.exit_code())
        }
    }
}

fn dispatch(cli: &Cli) -> Result<(), CliError> {
    let home = || grove_home();
    let api = ApiClient::from_env;

    match &cli.command {
        Some(Command::Apply) => commands::apply(&home()),
        Some(Command::Clone { action }) => match action {
            CloneAction::Add { repo } => commands::clone_add(&home(), &api()?, repo),
            CloneAction::Remove { repo, force } => {
                commands::clone_remove(&home(), &api()?, repo, *force)
            }
        },
        Some(Command::Doctor { slug, dry_run, fix }) => {
            commands::doctor(&home(), &api()?, slug.as_deref(), *dry_run, *fix)
        }
        Some(Command::Off) => ServerControl::from_env()?.off(),
        Some(Command::Ok) => api()?.ok().map(|line| println!("{line}")),
        Some(Command::On) => ServerControl::from_env()?.on(),
        Some(Command::Reboot) => ServerControl::from_env()?.reboot(),
        Some(Command::Serve) => serve(),
        Some(Command::Setup { remove }) => setup::setup(*remove),
        Some(Command::Sync { slug }) => commands::sync(&home(), &api()?, slug),
        Some(Command::Tree { action }) => match action {
            TreeAction::Add { repo, branch, base } => {
                commands::tree_add(&home(), &api()?, repo, branch, base.as_deref())
            }
            TreeAction::List { repo, verbose } => {
                commands::tree_list(&home(), &api()?, repo, *verbose)
            }
            TreeAction::Remove { repo, name } => {
                commands::tree_remove(&home(), &api()?, repo, name)
            }
        },
        Some(Command::Upgrade { check, back, retry }) => {
            setup::upgrade(&[("--check", *check), ("--back", *back), ("--retry", *retry)])
        }
        Some(Command::Version) => {
            println!("grove {VERSION}");
            Ok(())
        }
        Some(Command::Workspace { out }) => commands::workspace(&home(), out.as_deref()),
        None => {
            println!("grove {VERSION} — run `grove --help` for commands.");
            Ok(())
        }
    }
}

/// The data dir every command reads and writes: `GROVE_HOME` → `~/.grove`.
///
/// Resolved by [`grove_ops::home`] — the one implementation of the rule, shared with
/// the launcher and the daemon, because three copies of it are three chances for
/// `grove serve`, `grove on` and the offline realizer to disagree about which home
/// they are for.
fn grove_home() -> std::path::PathBuf {
    grove_ops::home()
}

/// How long the process waits, after the daemon's own bounded drain, for blocking
/// work to leave the runtime.
///
/// Dropping a tokio runtime blocks until every **started** blocking task finishes —
/// unbounded, silent, and up to `GROVE_CLONE_TIMEOUT_SECS` (an hour) for a clone. An
/// operator watching `grove serve` refuse to exit has nothing to read and nothing to
/// wait for. `shutdown_timeout` turns that into a bound the process can log; a task
/// still running when it expires is left to the process exit that follows, which
/// carried law 11 makes recoverable.
///
/// Short, because `Daemon::serve` has already spent its own drain budget on exactly
/// this work: the two together stay inside `grove off`'s 10 s grace and the server
/// agent's `ExitTimeOut`, which `grove setup` writes from that same grace.
const RUNTIME_DRAIN: Duration = Duration::from_secs(2);

/// The size `serve.log` reaches before it is renamed to `serve.log.1`, replacing the
/// one generation kept.
const LOG_CAP: u64 = 10 << 20;

/// Run the daemon in this process until it drains.
///
/// Bind (which is where a non-loopback address is refused, before anything is
/// exposed), then serve until the shutdown route or a stop signal drains the accept
/// loop; health reads `booting` until the watcher's first roots read. Every way out —
/// a drain, a startup error, a panic — leaves a line in `$GROVE_HOME/serve.log`. No
/// `exit` anywhere: the process ends by returning from here.
// ※ stop-signals-drain
fn serve() -> Result<(), CliError> {
    let config = Config::from_env();
    // Before the subscriber, because the subscriber is what feeds it — and before the
    // daemon, because a ring created at bind time would miss everything said during
    // boot, which is exactly what an operator opening a dashboard after a bad start
    // needs to see.
    let level = config.as_ref().map_or(DEFAULT_LEVEL, |c| c.log_level);
    let logs = Arc::new(LogRing::new(Arc::new(SystemClock), level));
    init_logging(&grove_home(), &logs)?;
    tracing::info!(
        version = VERSION,
        pid = std::process::id(),
        "grove server starting"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Daemon(format!("build runtime: {e}")))?;
    grove_ops::git::supervise();

    let outcome = runtime.block_on(async {
        // registered before the bind, so a stop sent while booting is held, not fatal
        let on = |kind| signal(kind).map_err(|e| CliError::Daemon(format!("signal: {e}")));
        let (mut term, mut int, mut hup) = (
            on(SignalKind::terminate())?,
            on(SignalKind::interrupt())?,
            on(SignalKind::hangup())?,
        );
        let daemon = Daemon::bind_with_logs(config?, Arc::new(SystemClock), logs).await?;
        let state = daemon.state().clone();
        tracing::info!(
            bind = %daemon.local_addr()?,
            home = %state.config.home.display(),
            "grove server listening"
        );
        // the shutdown route's own drain, minus its grace: nobody waits on an answer
        let stop = tokio::spawn(async move {
            let name = tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = int.recv() => "SIGINT",
                _ = hup.recv() => "SIGHUP",
            };
            state.boot.mark_stopping();
            state.shutdown.fire();
            name
        });
        daemon.serve().await?;
        Ok::<_, CliError>(if stop.is_finished() {
            stop.await.unwrap_or("signal")
        } else {
            "shutdown request"
        })
    });

    grove_ops::git::kill_children();
    // Bounded and *said out loud*, rather than the silent unbounded block a bare
    // `drop(runtime)` performs — see [`RUNTIME_DRAIN`].
    let clock = SystemClock;
    let deadline = clock.deadline(RUNTIME_DRAIN);
    runtime.shutdown_timeout(RUNTIME_DRAIN);
    if deadline.expired(&clock) {
        tracing::warn!(
            reason = "runtime drain budget expired",
            "left blocking work running at exit"
        );
    }
    match &outcome {
        Ok(reason) => tracing::info!(%reason, "grove server stopped"),
        Err(reason) => tracing::error!(%reason, "grove server stopped"),
    }
    outcome.map(drop)
}

/// Three sinks, one subscriber, and a panic hook that writes to them first.
///
/// `$GROVE_HOME/serve.log` is the daemon's own ([`LogFile`]); **stderr** carries the
/// same lines to a terminal or a pipe (colour only on a terminal), never to a file.
/// `GROVE_LOG` takes an `EnvFilter` directive (`grove_daemon=debug`); the default is
/// `info`.
///
/// The ring is the third sink — what `GET /api/events` streams to a UI — and it
/// carries its **own** level (`GROVE_LOG_RING`, `info` by default), so turning the
/// file log up to debug does not flood every attached dashboard. Each layer filters
/// itself, which is why the writers' filters are per-layer rather than global.
fn init_logging(home: &std::path::Path, logs: &Arc<LogRing>) -> Result<(), CliError> {
    use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
    use tracing_subscriber::util::SubscriberInitExt as _;

    let path = home.join("serve.log");
    let file = std::fs::create_dir_all(home)
        .and_then(|()| LogFile::open(path.clone(), LOG_CAP))
        .map_err(|e| CliError::Daemon(format!("open {}: {e}", path.display())))?;
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_env("GROVE_LOG")
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };
    // a stderr that is a file is the agent's `serve.err`, which must not copy the log
    let stderr = rustix::fs::fstat(std::io::stderr())
        .is_ok_and(|st| !rustix::fs::FileType::from_raw_mode(st.st_mode).is_file());
    let _ = tracing_subscriber::registry()
        .with(stderr.then(|| {
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
                .with_filter(filter())
        }))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(Arc::new(file))
                .with_ansi(false)
                .with_filter(filter()),
        )
        .with(logs.layer())
        .try_init();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "grove server panicked");
        default(info);
    }));
    Ok(())
}

/// `serve.log`, rotated to `serve.log.1` once it reaches its cap. The check runs
/// before each write, and the formatter writes a whole line at a time, so a rotation
/// never splits one.
struct LogFile {
    cap: u64,
    file: Mutex<(File, u64)>,
    path: PathBuf,
}

impl LogFile {
    fn open(path: PathBuf, cap: u64) -> std::io::Result<Self> {
        let file = append(&path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            cap,
            file: Mutex::new((file, size)),
            path,
        })
    }
}

fn append(path: &std::path::Path) -> std::io::Result<File> {
    File::options().append(true).create(true).open(path)
}

impl std::io::Write for &LogFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        if file.1 >= self.cap {
            std::fs::rename(&self.path, self.path.with_extension("log.1"))?;
            *file = (append(&self.path)?, 0);
        }
        let n = file.0.write(buf)?;
        file.1 += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, CloneAction, Command, TreeAction, dispatch};
    use clap::{CommandFactory, Parser};

    fn parse(args: &[&str]) -> Option<Command> {
        Cli::parse_from([&["grove"], args].concat()).command
    }

    /// The launcher contract is a *published* string: `grove on` spawns
    /// `<binary> serve`, and the launchd agent `grove setup` writes spells it the same way,
    /// and a rename here would strand every one of them.
    #[test]
    fn serve_is_the_launcher_subcommand() {
        assert_eq!(parse(&["serve"]), Some(Command::Serve));
    }

    #[test]
    fn the_custody_trio_parses() {
        assert_eq!(parse(&["on"]), Some(Command::On));
        assert_eq!(parse(&["off"]), Some(Command::Off));
        assert_eq!(parse(&["reboot"]), Some(Command::Reboot));
        assert_eq!(parse(&["version"]), Some(Command::Version));
        assert_eq!(parse(&["ok"]), Some(Command::Ok));
        assert_eq!(parse(&["apply"]), Some(Command::Apply));
    }

    /// A slug is positional and required — `grove sync` with no root is a clap error,
    /// not a whole-home fetch. Syncing every root at once is an operation nothing in
    /// the contract offers, and a command that quietly invented it would fetch N
    /// remotes on a typo.
    #[test]
    fn sync_takes_one_required_slug() {
        assert_eq!(
            parse(&["sync", "o/r"]),
            Some(Command::Sync { slug: "o/r".into() })
        );
        assert!(Cli::try_parse_from(["grove", "sync"]).is_err());
    }

    #[test]
    fn parses_clone_add_and_remove() {
        assert_eq!(
            parse(&["clone", "add", "o/r"]),
            Some(Command::Clone {
                action: CloneAction::Add { repo: "o/r".into() }
            })
        );
        assert_eq!(
            parse(&["clone", "remove", "o/r"]),
            Some(Command::Clone {
                action: CloneAction::Remove {
                    repo: "o/r".into(),
                    force: false
                }
            })
        );
        assert!(Cli::try_parse_from(["grove", "clone"]).is_err());
    }

    #[test]
    fn parses_tree_commands() {
        assert_eq!(
            parse(&["tree", "add", "o/r", "feature/x", "--base", "main"]),
            Some(Command::Tree {
                action: TreeAction::Add {
                    repo: "o/r".into(),
                    branch: "feature/x".into(),
                    base: Some("main".into())
                }
            })
        );
        assert_eq!(
            parse(&["tree", "list", "o/r"]),
            Some(Command::Tree {
                action: TreeAction::List {
                    repo: "o/r".into(),
                    verbose: false
                }
            })
        );
        assert_eq!(
            parse(&["tree", "list", "o/r", "--verbose"]),
            Some(Command::Tree {
                action: TreeAction::List {
                    repo: "o/r".into(),
                    verbose: true
                }
            })
        );
        assert_eq!(
            parse(&["tree", "remove", "o/r", "feat"]),
            Some(Command::Tree {
                action: TreeAction::Remove {
                    repo: "o/r".into(),
                    name: "feat".into()
                }
            })
        );
    }

    /// `--out` is optional and the default is the home's own file, so the command an
    /// operator actually types is the bare one.
    #[test]
    fn parses_workspace_with_an_optional_out() {
        assert_eq!(
            parse(&["workspace"]),
            Some(Command::Workspace { out: None })
        );
        assert_eq!(
            parse(&["workspace", "--out", "/tmp/w.code-workspace"]),
            Some(Command::Workspace {
                out: Some("/tmp/w.code-workspace".into())
            })
        );
    }

    #[test]
    fn parses_doctor_with_flags() {
        assert_eq!(
            parse(&["doctor"]),
            Some(Command::Doctor {
                slug: None,
                dry_run: false,
                fix: false
            })
        );
        assert_eq!(
            parse(&["doctor", "o/r", "--dry-run"]),
            Some(Command::Doctor {
                slug: Some("o/r".into()),
                dry_run: true,
                fix: false
            })
        );
        assert_eq!(
            parse(&["doctor", "--fix"]),
            Some(Command::Doctor {
                slug: None,
                dry_run: false,
                fix: true
            })
        );
    }

    #[test]
    fn parses_setup_and_upgrade() {
        assert_eq!(parse(&["setup"]), Some(Command::Setup { remove: false }));
        assert_eq!(
            parse(&["setup", "--remove"]),
            Some(Command::Setup { remove: true })
        );
        assert_eq!(
            parse(&["upgrade", "--back"]),
            Some(Command::Upgrade {
                check: false,
                back: true,
                retry: false
            })
        );
        assert!(Cli::try_parse_from(["grove", "upgrade", "--check", "--retry"]).is_err());
    }

    /// A bare invocation is not an error, and an unknown subcommand is — clap's own
    /// exit 2, which is why the CLI's own taxonomy skips that code.
    #[test]
    fn a_bare_invocation_parses_and_an_unknown_one_does_not() {
        assert!(Cli::parse_from(["grove"]).command.is_none());
        assert!(Cli::try_parse_from(["grove", "no-such-command"]).is_err());
    }

    #[test]
    fn the_clap_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// The commands that touch neither disk nor network dispatch cleanly — the
    /// smoke test that the match arms are wired at all.
    #[test]
    fn pure_commands_dispatch_ok() {
        for command in [None, Some(Command::Version)] {
            assert!(dispatch(&Cli { command }).is_ok());
        }
    }

    /// The log keeps one generation: at the cap, the file moves to `serve.log.1` and the
    /// next line starts a fresh one.
    #[test]
    fn the_log_rotates_one_generation_at_its_cap() {
        use std::io::Write as _;
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("serve.log");
        std::fs::write(&path, "old\n").unwrap();
        let log = super::LogFile::open(path.clone(), 4).unwrap();
        (&log).write_all(b"new\n").unwrap();
        (&log).flush().unwrap();
        let read = |p| std::fs::read_to_string(p).unwrap();
        assert_eq!(read(tmp.path().join("serve.log.1")), "old\n");
        assert_eq!(read(path), "new\n");
    }

    /// A panic reaches the log before the default hook runs, so a death leaves a line.
    #[test]
    fn a_panic_is_logged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ring = std::sync::Arc::new(grove_daemon::LogRing::new(
            std::sync::Arc::new(grove_ops::clock::SystemClock),
            grove_daemon::logs::DEFAULT_LEVEL,
        ));
        super::init_logging(tmp.path(), &ring).unwrap();
        assert!(std::thread::spawn(|| panic!("boom")).join().is_err());
        let log = std::fs::read_to_string(tmp.path().join("serve.log")).unwrap();
        assert!(
            log.contains("grove server panicked") && log.contains("boom"),
            "{log}"
        );
    }
}
