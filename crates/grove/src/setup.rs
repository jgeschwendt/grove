//! `grove setup [--remove]` and `grove upgrade`, the two verbs of hotswap's tool contract
//! (<https://gigantic.dev/hotswap/>): hotswap installs and moves the version, setup does the
//! install duties, upgrade the swap.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use std::thread;

use grove_ops::clock::{Clock as _, SystemClock};

use crate::error::CliError;
use crate::timeouts::{KILL, POLL_INTERVAL, STOP_GRACE};

/// The server agent: `grove serve` through `~/.local/bin/grove`, kept alive by launchd.
pub(crate) const SERVER: &str = "dev.gigantic.grove";

/// This user's launchd domain, `gui/<uid>`.
pub(crate) fn gui() -> String {
    format!("gui/{}", nix::unistd::getuid())
}

/// `launchctl <args>`, its output discarded: whether it succeeded.
pub(crate) fn launchctl(args: &[&str]) -> bool {
    Command::new("launchctl")
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `launchctl bootout <target>`, returning once launchd has let go of the service:
/// bootout returns while the daemon still drains its SIGTERM, and a `bootstrap` in that
/// window fails with an I/O error. Bounded by the stop grace the plist's `ExitTimeOut`
/// gives the drain, plus the kill wait.
pub(crate) fn bootout(target: &str) -> bool {
    let deadline = SystemClock.deadline(STOP_GRACE + KILL);
    launchctl(&["bootout", target])
        && loop {
            if !launchctl(&["print", target]) {
                break true;
            }
            if deadline.expired(&SystemClock) {
                break false;
            }
            thread::sleep(POLL_INTERVAL);
        }
}

/// grove's hotswap manifest: where its releases come from and what runs around a swap. No
/// drain step: the swap is hotswap's one `kickstart -k`, whose SIGTERM runs the daemon's own
/// bounded drain before launchd restarts it through `~/.local/bin/grove`, by then the new
/// version — a separate drain would race `KeepAlive`'s respawn. An agent loaded at login under
/// a managed launch constraint is reloaded from its plist instead (bootout drains it the same
/// way); hotswap decides at swap time, grove does nothing for it. The smoke is the binary's
/// version and the restarted daemon's health, retried while it boots.
fn manifest() -> hotswap::Manifest {
    let owned = |words: &[&str]| words.iter().map(|&w| w.to_owned()).collect::<Vec<_>>();
    let health = format!("http://{}/api/health", grove_daemon::DEFAULT_BIND);
    hotswap::Manifest {
        agents: owned(&[SERVER]),
        repo: "jgeschwendt/grove".into(),
        setup: owned(&["grove", "setup"]),
        signer: "ps1_rahbrmrvyrwx2q75yvkfebgfzy".into(),
        smoke: vec![
            owned(&["grove", "--version"]),
            owned(&[
                "curl",
                "-fsS",
                "--retry",
                "30",
                "--retry-all-errors",
                "--retry-delay",
                "1",
                &health,
            ]),
        ],
        ..hotswap::Manifest::default()
    }
}

/// hotswap's one-line error without its `grove: `, which `run` prints itself.
fn swap_error(e: &str) -> CliError {
    CliError::Api(e.trim_start_matches("grove: ").to_owned())
}

/// `setup [--remove]`: `hotswap::ensure` brings grove into hotswap's layout — a mise install
/// moved in, else the latest release fetched when none is active — links `~/.local/bin/grove`
/// and tends the upgrade agent, which is hotswap's alone; setup then writes and bootstraps the
/// server agent on that link. Run from a checkout (`cargo run -- setup`) it is the same: the
/// layout's release, or the latest fetched, is linked and run, never the checkout's binary.
/// `--remove` boots the server out and removes its plist and the link, and `hotswap::forget`
/// the upgrade agent, layout and record, leaving `GROVE_HOME` alone. A loaded server whose plist
/// changed is reloaded; one launchctl holds disabled gets its plist and a hint, never a bootstrap
/// or bootout.
pub(crate) fn setup(remove: bool) -> Result<(), CliError> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let (gui, link) = (gui(), home.join(".local/bin/grove"));
    let target = format!("{gui}/{SERVER}");
    let path = home.join(format!("Library/LaunchAgents/{SERVER}.plist"));
    let held = Command::new("launchctl")
        .args(["print-disabled", &gui])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout).contains(&format!("\"{SERVER}\" => disabled"))
        });
    if remove {
        // first: it refuses while a swap runs, before anything is booted out
        hotswap::forget("grove").map_err(|e| swap_error(&e))?;
        if !held && bootout(&target) {
            println!("booted out {SERVER}");
        }
        for path in [&path, &link] {
            if fs::remove_file(path).is_ok() {
                println!("removed {}", path.display());
            }
        }
        return Ok(());
    }
    let exe = hotswap::ensure("grove", &manifest()).map_err(|e| swap_error(&e))?;
    let to = fs::canonicalize(&exe).unwrap_or_default();
    println!("linked {} → {}", exe.display(), to.display());
    let old = home.join(".local/share/grove/current");
    if old.exists() {
        println!(
            "{} is no longer read: grove runs from hotswap's layout; remove {} when ready",
            old.display(),
            old.parent().unwrap_or(&old).display()
        );
    }
    // ※ upgrade-agent-kept
    let text = server_plist(&home, &exe);
    let fresh = fs::read_to_string(&path).ok() != Some(text.clone());
    fs::create_dir_all(path.parent().unwrap_or(&path))
        .and_then(|()| fs::write(&path, &text))
        .map_err(|e| CliError::Api(format!("{}: {e}", path.display())))?;
    if held {
        eprintln!(
            "{SERVER}: disabled by launchctl; launchctl enable {target} && grove setup loads it"
        );
        return Ok(());
    }
    let loaded = launchctl(&["print", &target]);
    let reload = loaded && fresh;
    if reload {
        bootout(&target);
    }
    if (!loaded || reload) && !launchctl(&["bootstrap", &gui, &path.to_string_lossy()]) {
        return Err(CliError::Api(format!(
            "launchctl bootstrap {SERVER} failed"
        )));
    }
    let state = match (loaded, reload) {
        (false, _) => "loaded",
        (true, true) => "reloaded",
        (true, false) => "current",
    };
    println!("{state} {SERVER}");
    Ok(())
}

/// The server agent's plist: `<exe> serve`, `exe` being `~/.local/bin/grove`, which resolves
/// through hotswap's `current` on every start, kept alive. Its PATH carries mise's default
/// shims dir when one exists, after `~/.local/bin`: not for grove, which needs no mise, but
/// for what git runs on the user's behalf — hooks, filters (`git-lfs`), credential helpers —
/// from the toolchain the user manages; `~/.local/bin` leads, so a tool hotswap links there
/// beats mise's stale shim of it. The daemon writes and rotates `$GROVE_HOME/serve.log` itself; stderr goes to
/// `serve.err`, for what fails before the log is open (a panic's default message).
/// `ExitTimeOut` is `grove off`'s stop grace, which the daemon's whole drain fits inside:
/// without it the agent printed `exit timeout = 5` (observed 2026-11-11).
fn server_plist(home: &Path, exe: &Path) -> String {
    let esc = |p: &Path| {
        let p = p.display().to_string().replace('&', "&amp;");
        p.replace('<', "&lt;").replace('>', "&gt;")
    };
    let shims = home.join(".local/share/mise/shims");
    let shims = if shims.is_dir() {
        format!("{}:", esc(&shims))
    } else {
        String::new()
    };
    let (grove, home, exe) = (esc(&grove_ops::home()), esc(home), esc(exe));
    let exit = crate::timeouts::STOP_GRACE.as_secs();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>EnvironmentVariables</key>
  <dict>
    <key>GROVE_HOME</key><string>{grove}</string>
    <key>HOME</key><string>{home}</string>
    <key>LANG</key><string>en_US.UTF-8</string>
    <key>PATH</key><string>{home}/.local/bin:{shims}/usr/local/bin:/usr/bin:/bin</string>
  </dict>
  <key>ExitTimeOut</key><integer>{exit}</integer>
  <key>KeepAlive</key><true/>
  <key>Label</key><string>{SERVER}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>StandardErrorPath</key><string>{grove}/serve.err</string>
  <key>ThrottleInterval</key><integer>10</integer>
</dict>
</plist>
"#
    )
}

/// `upgrade [--check | --back | --retry]`: hotswap's swap under grove's manifest, its
/// return the exit code.
pub(crate) fn upgrade(flags: &[(&str, bool)]) -> Result<(), CliError> {
    let args = flags
        .iter()
        .filter(|(_, on)| *on)
        .map(|(f, _)| (*f).to_owned());
    exit(hotswap::upgrade(
        "grove",
        &manifest(),
        &args.collect::<Vec<_>>(),
    ))
}
