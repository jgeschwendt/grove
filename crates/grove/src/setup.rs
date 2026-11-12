//! `grove setup [--remove]` and `grove upgrade`, the two verbs of hotswap's tool contract
//! (<https://gigantic.dev/hotswap/>): mise installs and moves the version, setup does the
//! install duties, upgrade the swap.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use std::thread;

use grove_ops::clock::{Clock as _, SystemClock};

use crate::error::CliError;
use crate::timeouts::{KILL, POLL_INTERVAL, STOP_GRACE};

/// The server agent: `grove serve` through mise's shim, kept alive by launchd.
pub(crate) const SERVER: &str = "dev.gigantic.grove";

/// The upgrade agent; hotswap renders its plist.
const UPGRADE: &str = "dev.gigantic.grove.upgrade";

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

/// `setup [--remove]`: link `~/.local/bin/grove` to mise's shim, write and bootstrap the
/// server and upgrade agents; `--remove` undoes it and forgets hotswap's record, leaving
/// `GROVE_HOME` alone. A loaded agent whose plist changed is reloaded, the upgrade agent's
/// never (its bootout would kill the swap running this setup); an agent launchctl holds
/// disabled gets its plist and a hint, never a bootstrap or bootout; a failed bootstrap
/// fails the run only after every other agent is loaded.
pub(crate) fn setup(remove: bool) -> Result<(), CliError> {
    let shim = hotswap::shims().join("grove");
    if !remove && !shim.exists() {
        return Err(CliError::Api(format!(
            "no mise shim at {}: install with `mise use -g packslip:jgeschwendt/grove` first",
            shim.display()
        )));
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let gui = gui();
    let disabled = Command::new("launchctl")
        .args(["print-disabled", &gui])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let held = |label: &str| disabled.contains(&format!("\"{label}\" => disabled"));
    let agents = home.join("Library/LaunchAgents");
    let link = home.join(".local/bin/grove");
    let make = |path: &Path, op: &dyn Fn() -> std::io::Result<()>| {
        fs::create_dir_all(path.parent().unwrap_or(path))
            .and_then(|()| op())
            .map_err(|e| CliError::Api(format!("{}: {e}", path.display())))
    };
    if remove {
        for label in [SERVER, UPGRADE] {
            if !held(label) && bootout(&format!("{gui}/{label}")) {
                println!("booted out {label}");
            }
        }
        for path in [SERVER, UPGRADE]
            .map(|label| agents.join(format!("{label}.plist")))
            .iter()
            .chain([&link])
        {
            if fs::remove_file(path).is_ok() {
                println!("removed {}", path.display());
            }
        }
        hotswap::forget("grove");
        return Ok(());
    }
    fs::remove_file(&link).ok();
    make(&link, &|| symlink(&shim, &link))?;
    println!("linked {} → {}", link.display(), shim.display());
    let old = home.join(".local/share/grove/current");
    if old.exists() {
        println!(
            "{} is no longer read: grove runs from mise; remove {} when ready",
            old.display(),
            old.parent().unwrap_or(&old).display()
        );
    }
    let mut failed = Vec::new();
    for (label, text) in [
        (SERVER, server_plist(&home)),
        (UPGRADE, hotswap::agent_plist("grove", "grove")),
    ] {
        let path = agents.join(format!("{label}.plist"));
        let fresh = fs::read_to_string(&path).ok() != Some(text.clone());
        make(&path, &|| fs::write(&path, &text))?;
        let target = format!("{gui}/{label}");
        if held(label) {
            eprintln!(
                "{label}: disabled by launchctl; launchctl enable {target} && grove setup loads it"
            );
            continue;
        }
        let loaded = launchctl(&["print", &target]);
        // ※ upgrade-agent-kept
        let reload = loaded && fresh && label != UPGRADE;
        if reload {
            bootout(&target);
        }
        if (!loaded || reload) && !launchctl(&["bootstrap", &gui, &path.to_string_lossy()]) {
            failed.push(label);
            continue;
        }
        let state = match (loaded, reload) {
            (false, _) => "loaded",
            (true, true) => "reloaded",
            (true, false) => "current",
        };
        println!("{state} {label}");
    }
    if failed.is_empty() {
        return Ok(());
    }
    Err(CliError::Api(format!(
        "launchctl bootstrap {} failed",
        failed.join(", ")
    )))
}

/// The server agent's plist: `<shims>/grove serve`, so launchd resolves mise's active
/// version on every start, kept alive. The daemon writes and rotates
/// `$GROVE_HOME/serve.log` itself; stderr goes to `serve.err`, for what fails before the
/// log is open (mise's shim, a panic's default message). `ExitTimeOut` is `grove off`'s
/// stop grace, which the daemon's whole drain fits inside: without it the agent printed
/// `exit timeout = 5` (observed 2026-11-11).
fn server_plist(home: &Path) -> String {
    let esc = |p: &Path| {
        let p = p.display().to_string().replace('&', "&amp;");
        p.replace('<', "&lt;").replace('>', "&gt;")
    };
    let (grove, home, shims) = (esc(&grove_ops::home()), esc(home), esc(&hotswap::shims()));
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
    <key>PATH</key><string>{shims}:{home}/.local/bin:/usr/local/bin:/usr/bin:/bin</string>
  </dict>
  <key>ExitTimeOut</key><integer>{exit}</integer>
  <key>KeepAlive</key><true/>
  <key>Label</key><string>{SERVER}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{shims}/grove</string>
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
/// return the exit code. No drain step: the swap is hotswap's one `kickstart -k`, whose
/// SIGTERM runs the daemon's own bounded drain before launchd restarts it through the
/// shim on the version mise just made active — a separate drain would race `KeepAlive`'s
/// respawn. The smoke is the binary's version and the restarted daemon's health,
/// retried while it boots.
pub(crate) fn upgrade(flags: &[(&str, bool)]) -> Result<(), CliError> {
    let owned = |words: &[&str]| words.iter().map(|&w| w.to_owned()).collect::<Vec<_>>();
    let health = format!("http://{}/api/health", grove_daemon::DEFAULT_BIND);
    let manifest = hotswap::Manifest {
        agents: owned(&[SERVER]),
        setup: owned(&["grove", "setup"]),
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
    };
    let args = flags
        .iter()
        .filter(|(_, on)| *on)
        .map(|(f, _)| (*f).to_owned());
    exit(hotswap::upgrade(
        "grove",
        &manifest,
        &args.collect::<Vec<_>>(),
    ))
}
