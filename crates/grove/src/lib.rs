//! grove — the CLI, and the single shipped binary. `grove serve` runs the daemon
//! in-process; every other subcommand declares into `manifest.toml` and either nudges
//! a reachable server or realizes inline through grove-ops.
//!
//! This file is the command *surface* and nothing else: the clap tree, the dispatch,
//! and `serve`. The behaviour behind each command lives beside its own reasoning —
//! [`commands`] holds the single-realizer gate, [`render`] what gets printed and the
//! exit verdicts, [`api`] the HTTP client, [`server`] process custody, and
//! [`timeouts`] every budget any of them spends.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use grove_daemon::{Config, Daemon, LogRing};
use grove_ops::clock::{Clock, SystemClock};

mod api;
mod commands;
mod error;
mod render;
mod server;
mod timeouts;
mod update;

pub use api::{ApiClient, Reachability};
pub use error::CliError;
pub use server::ServerControl;
pub use update::{Bounce, BundleSource, Layout, Updater, host_target};

/// The version `grove --version` and `grove version` report, and the one the
/// self-update health gate expects a freshly flipped daemon to answer with.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Parser)]
#[command(name = "grove", version, about = "Cultivate git worktrees")]
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
    /// Update grove via the versioned-dir symlink flip (auto-rolls-back on failure).
    Up {
        /// Pin a specific version instead of resolving the channel.
        #[arg(long)]
        version: Option<String>,
        /// Release channel to follow (stable, canary). Default: the box's persisted
        /// channel, else stable.
        #[arg(long)]
        channel: Option<String>,
        /// Flip back to the previous version and restart.
        #[arg(long)]
        rollback: bool,
    },
    /// Print the grove version.
    Version,
}

/// `grove clone <action>`.
#[derive(Debug, PartialEq, Eq, Subcommand)]
enum CloneAction {
    /// Clone and track a repository.
    Add {
        /// Repository URL or `owner/name`.
        repo: String,
    },
    /// Stop tracking and remove a repository.
    Remove {
        /// Repository slug (`owner/name`).
        repo: String,
        /// Delete even when a worktree holds uncommitted or unpushed work.
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
            CloneAction::Add { repo } => commands::clone_add(&home(), &api(), repo),
            CloneAction::Remove { repo, force } => {
                commands::clone_remove(&home(), &api(), repo, *force)
            }
        },
        Some(Command::Doctor { slug, dry_run, fix }) => {
            commands::doctor(&home(), &api(), slug.as_deref(), *dry_run, *fix)
        }
        Some(Command::Off) => ServerControl::from_env()?.off(),
        Some(Command::Ok) => api().ok().map(|line| println!("{line}")),
        Some(Command::On) => ServerControl::from_env()?.on(),
        Some(Command::Reboot) => ServerControl::from_env()?.reboot(),
        Some(Command::Serve) => serve(),
        Some(Command::Sync { slug }) => commands::sync(&home(), &api(), slug),
        Some(Command::Tree { action }) => match action {
            TreeAction::Add { repo, branch, base } => {
                commands::tree_add(&home(), &api(), repo, branch, base.as_deref())
            }
            TreeAction::List { repo } => commands::tree_list(&home(), &api(), repo),
            TreeAction::Remove { repo, name } => commands::tree_remove(&home(), &api(), repo, name),
        },
        // `--rollback` ignores `--version`/`--channel` by construction: it flips to
        // whatever `previous` names, which is the one version a rollback can mean.
        Some(Command::Up {
            version,
            channel,
            rollback,
        }) => {
            let updater = Updater::from_env()?.with_channel(channel.as_deref());
            if *rollback {
                updater.rollback()
            } else {
                updater.up(version.as_deref())
            }
        }
        Some(Command::Version) => {
            println!("grove {VERSION}");
            Ok(())
        }
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

/// The install root every command launches from and `grove up` flips:
/// `GROVE_INSTALL` → `~/.local/share/grove`.
///
/// Resolved by [`grove_ops::install_home`] — the one implementation of that rule,
/// shared with the launcher and the updater, because the install and the workspace
/// have opposite lifecycles and only one of the two may ever be thrown away.
// `allow`, not `expect`: stage 2 wires the launcher and the updater to this, and
// an unfulfilled expectation would then fail the same `-D warnings` gate. Drop the
// attribute with the first caller.
#[allow(
    dead_code,
    reason = "the callers land in a later stage of the install/workspace split"
)]
fn grove_install_home() -> std::path::PathBuf {
    grove_ops::install_home()
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
/// this work: the two together stay inside `grove off`'s 10 s grace.
const RUNTIME_DRAIN: Duration = Duration::from_secs(2);

/// Run the daemon in this process until it drains.
///
/// The three steps are deliberate and in this order: bind (which is where a
/// non-loopback address is refused, before anything is exposed), signal readiness
/// (nothing else in the process may do it — a route that marked itself ready would
/// make the boot state a decoration), then serve until the shutdown route drains the
/// accept loop. No `exit` anywhere: the process ends by returning from here.
fn serve() -> Result<(), CliError> {
    let config = Config::from_env()?;
    // Before the subscriber, because the subscriber is what feeds it — and before the
    // daemon, because a ring created at bind time would miss everything said during
    // boot, which is exactly what an operator opening a dashboard after a bad start
    // needs to see.
    let logs = Arc::new(LogRing::new(Arc::new(SystemClock), config.log_level));
    init_tracing(&logs);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Daemon(format!("build runtime: {e}")))?;

    let outcome = runtime.block_on(async {
        let daemon = Daemon::bind_with_logs(config, Arc::new(SystemClock), logs).await?;
        let addr = daemon.local_addr()?;
        daemon.state().boot.mark_ready();
        tracing::info!(
            bind = %addr,
            home = %daemon.state().config.home.display(),
            version = %daemon.state().config.version,
            "grove server listening"
        );
        daemon.serve().await?;
        tracing::info!("grove server stopped");
        Ok(())
    });

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
    outcome
}

/// Two sinks, one subscriber.
///
/// Logs go to **stderr**: the launcher redirects both streams into `grove.log`, and
/// keeping stdout clean leaves it free for the commands' own output. `GROVE_LOG`
/// takes an `EnvFilter` directive (`grove_daemon=debug`); the default is `info`.
///
/// The ring is the second sink — what `GET /api/events` streams to a UI — and it
/// carries its **own** level (`GROVE_LOG_RING`, `info` by default), so turning the
/// file log up to debug does not flood every attached dashboard. Each layer filters
/// itself, which is why the writer's filter is per-layer rather than global.
fn init_tracing(logs: &Arc<LogRing>) {
    use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
    use tracing_subscriber::util::SubscriberInitExt as _;

    let filter = tracing_subscriber::EnvFilter::try_from_env("GROVE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                // Colour for a human running `grove serve` in a terminal; plain text
                // everywhere else. tracing-subscriber's ansi default does NOT consult
                // tty-ness, and `grove on` redirects this stream straight into
                // `grove.log` — the file the operator greps and any log shipper
                // reads — so without this every line there carries escape codes.
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
                .with_filter(filter),
        )
        .with(logs.layer())
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::{Cli, CloneAction, Command, TreeAction, dispatch};
    use clap::{CommandFactory, Parser};

    fn parse(args: &[&str]) -> Option<Command> {
        Cli::parse_from([&["grove"], args].concat()).command
    }

    /// The launcher contract is a *published* string: `grove on` spawns
    /// `<binary> serve`, `install.sh` and any supervisor unit spell it the same way,
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
                action: TreeAction::List { repo: "o/r".into() }
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

    /// `up`'s flags are the contract `install.sh` and the release flow are written
    /// against — `install.sh` hands off with `up --version <v>`, and a canary box
    /// follows `--channel` — so the surface is pinned here, separately from the
    /// updater's own tests.
    #[test]
    fn parses_up_with_flags() {
        assert_eq!(
            parse(&["up"]),
            Some(Command::Up {
                version: None,
                channel: None,
                rollback: false
            })
        );
        assert_eq!(
            parse(&["up", "--version", "0.4.0"]),
            Some(Command::Up {
                version: Some("0.4.0".into()),
                channel: None,
                rollback: false
            })
        );
        assert_eq!(
            parse(&["up", "--channel", "canary"]),
            Some(Command::Up {
                version: None,
                channel: Some("canary".into()),
                rollback: false
            })
        );
        assert_eq!(
            parse(&["up", "--rollback"]),
            Some(Command::Up {
                version: None,
                channel: None,
                rollback: true
            })
        );
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
}
