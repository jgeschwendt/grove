//! `grove setup [--remove]`, `grove upgrade` and the launchd-owned `off`/`reboot` on a
//! fabricated home: stub `launchctl` and `curl` first on `$PATH`, so nothing reaches the
//! network or the real launchd domain, and hotswap's setup and smoke steps run the real
//! `grove` — against a closed `GROVE_BIND`, never the operator's server.

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

/// Writes `text` to `path`, executable, its parents made.
fn put(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    fs::write(path, text).expect("write");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// `curl` as hotswap and the smoke call it: the releases/latest redirect lands on
/// v2026.10.1, every other call only logs — a packslip bootstrap fetches nothing.
const CURL: &str = "#!/bin/sh\necho \"curl $*\" >> \"$HOME/log\"\ncase \"$*\" in *releases/latest) printf https://github.com/jgeschwendt/grove/releases/tag/v2026.10.1 ;; esac";

/// `bin args…` under the scratch home `h`: exit code, then stdout, stderr and the stubs'
/// log, which it clears.
fn launch(bin: &Path, h: &str, bind: &str, args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(bin)
        .args(args)
        // not env_clear: llvm-cov's profile path must reach the binary
        .env_remove("GROVE_HOME")
        .env_remove("MISE_DATA_DIR")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XPC_SERVICE_NAME")
        .env("GROVE_BIND", bind)
        .env("HOME", h)
        // the scratch HOME stands in for the account's, so hotswap loads the upgrade agent
        .env("HOTSWAP_TEST_HOME", h)
        .env("PATH", format!("{h}/stub:/usr/bin:/bin"))
        .output()
        .expect("grove");
    let log = fs::read_to_string(format!("{h}/log")).unwrap_or_default();
    drop(fs::remove_file(format!("{h}/log")));
    let text = format!(
        "{}{}{log}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code(), text)
}

/// A loopback address nothing listens on.
fn closed() -> String {
    let bind = TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("port");
    bind.to_string()
}

/// `gui/<uid>`, the launchd domain every call names.
fn gui() -> String {
    let uid = Command::new("id").arg("-u").output().expect("id").stdout;
    format!("gui/{}", String::from_utf8_lossy(&uid).trim())
}

#[test]
fn setup_installs_both_agents_upgrade_swaps_and_remove_undoes_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = tmp.path().display().to_string();
    let at = |rel: &str| tmp.path().join(rel);
    let put = |rel: &str, text: &str| put(&at(rel), text);
    let gone = |rel: &str| drop(fs::remove_file(at(rel)));
    // stubs carry a shebang: Linux's spawn, unlike macOS's, never falls back to sh
    // `print` answers loaded, serving `$HOME/.grove`, while `loaded` exists; `print-disabled`
    // prints `disabled`; `fail` fails the bootstrap of the agent it names; a bootout leaves the
    // agent draining for one more `print`, and its bootstrap fails until a `print` sees it gone
    put(
        "stub/launchctl",
        "#!/bin/sh\necho \"$*\" >> \"$HOME/log\"\nd=\"$HOME/draining.${2##*/}\"\n[ \"$1\" = bootout ] && { echo 1 > \"$d\"; exit 0; }\n[ \"$1\" = print ] && [ -e \"$d\" ] && { [ \"$(cat \"$d\")\" = 0 ] && { rm \"$d\"; exit 1; }; echo 0 > \"$d\"; exit 0; }\n[ \"$1\" = bootstrap ] && [ -e \"$HOME/draining.$(basename \"$3\" .plist)\" ] && exit 5\n[ \"$1\" = print ] && { [ -e \"$HOME/loaded\" ] && printf '\\tGROVE_HOME => %s\\n' \"$HOME/.grove\"; exit; }\n[ \"$1\" = print-disabled ] && { cat \"$HOME/disabled\"; exit 0; }\n[ \"$1\" = bootstrap ] && [ -e \"$HOME/fail\" ] && case $3 in *$(cat \"$HOME/fail\").plist) exit 5 ;; esac\nexit 0",
    );
    put("stub/curl", CURL);
    put("disabled", "");
    let bind = closed();
    let grove = Path::new(env!("CARGO_BIN_EXE_grove"));
    let run = |args: &[&str], code: i32| {
        let (got, text) = launch(grove, &h, &bind, args);
        assert_eq!(got, Some(code), "{args:?}: {text}");
        text
    };
    let gui = gui();
    let agents = format!("{h}/Library/LaunchAgents");
    let [server, upgrade] = ["dev.gigantic.grove", "dev.gigantic.grove.upgrade"];
    let boot = |label: &str| format!("bootstrap {gui} {agents}/{label}.plist\n");
    // hotswap's ensure tends the upgrade agent: an unloaded one it loads, a loaded one whose
    // plist it did not write it leaves; grove's setup looks only at the server
    let tended = format!(
        "print-disabled {gui}\nprint {gui}/{upgrade}\nprint-disabled {gui}\n{}",
        boot(upgrade)
    );
    let kept = format!("print-disabled {gui}\nprint {gui}/{upgrade}\n");
    let linked = format!(
        "linked {h}/.local/bin/grove → {}\n",
        fs::canonicalize(grove).expect("binary").display()
    );

    // nothing active and no mise: ensure fetches the latest, here offline — the stub curl
    // hands the pinned packslip's bootstrap nothing, so setup fails before any agent
    let bare = run(&["setup"], 1);
    assert!(
        bare.starts_with("grove: packslip 1.11.4 bootstrap: ")
            && bare.contains("curl -q -fsSL -o ")
            && !at("Library/LaunchAgents").exists(),
        "{bare}"
    );
    // the layout as hotswap leaves it, its one version this very binary
    let layout = at(".local/share/hotswap/grove");
    fs::create_dir_all(layout.join("2026.10.1")).expect("layout");
    symlink(grove, layout.join("2026.10.1/grove")).expect("version");
    symlink("2026.10.1", layout.join("current")).expect("current");
    put(".local/share/grove/current", "");
    fs::create_dir_all(at(&format!("Library/LaunchAgents/{server}.plist"))).expect("in the way");
    assert!(run(&["setup"], 1).contains(&format!("grove: {agents}/{server}.plist: ")));
    fs::remove_dir(at(&format!("Library/LaunchAgents/{server}.plist"))).expect("out of the way");
    assert_eq!(
        run(&["setup"], 0),
        format!(
            "{linked}{h}/.local/share/grove/current is no longer read: grove runs from hotswap's layout; remove {h}/.local/share/grove when ready\nloaded {server}\n{tended}print {gui}/{server}\n{}",
            boot(server)
        )
    );
    assert_eq!(
        fs::read_link(at(".local/bin/grove")).expect("link"),
        layout.join("current/grove")
    );
    let plists = [server, upgrade].map(|label| format!("{agents}/{label}.plist"));
    let check = "import plistlib, sys; [plistlib.load(open(p, 'rb')) for p in sys.argv[1:]]";
    let python = Command::new("python3")
        .args(["-c", check])
        .args(&plists)
        .status();
    assert!(python.expect("python3").success());
    let agent = fs::read_to_string(&plists[0]).expect("plist");
    // no mise shims dir, so none on the server's PATH
    assert!(
        agent.contains(&format!("<string>{h}/.local/bin/grove</string>"))
            && agent.contains(&format!(
                "<key>PATH</key><string>{h}/.local/bin:/usr/local/bin:/usr/bin:/bin</string>"
            ))
            && !agent.contains("mise"),
        "{agent}"
    );
    gone(".local/share/grove/current");

    // loaded: a changed server plist reloads; the upgrade agent's plist is hotswap's to rewrite,
    // and grove never boots it out
    put("loaded", "");
    for plist in &plists {
        fs::write(plist, "old\n").expect("stale");
    }
    assert_eq!(
        run(&["setup"], 0),
        format!(
            "{linked}reloaded {server}\n{kept}print {gui}/{server}\nbootout {gui}/{server}\nprint {gui}/{server}\nprint {gui}/{server}\n{}",
            boot(server)
        )
    );
    assert!(
        fs::read_to_string(&plists[1])
            .expect("plist")
            .contains("<string>upgrade</string>")
    );
    // a failed bootstrap fails the run, the upgrade agent still loaded
    gone("loaded");
    put("fail", server);
    assert_eq!(
        run(&["setup"], 1),
        format!(
            "{linked}grove: launchctl bootstrap {server} failed\n{tended}print {gui}/{server}\n{}",
            boot(server)
        )
    );
    gone("fail");
    // a disabled server is held: its plist and a hint, no bootstrap, and setup is green
    put("disabled", &format!("\t\"{server}\" => disabled\n"));
    assert_eq!(
        run(&["setup"], 0),
        format!(
            "{linked}{server}: disabled by launchctl; launchctl enable {gui}/{server} && grove setup loads it\n{tended}"
        )
    );
    put("disabled", "");

    // the swap: latest is the active version, so setup, the server's kickstart and both probes
    // run once onto it, and no drain races the kickstart — a `print` of the server by setup, a
    // second by the kick deciding between reload and kickstart; this stub names no plist path
    let swapped = run(&["upgrade"], 0);
    assert!(
        swapped.starts_with("grove 2026.10.1: green, 2 smoke probes\ncurl -q -fsSIL ")
            && swapped.contains(&format!("kickstart -k {gui}/{server}\ncurl -fsS "))
            && swapped.matches(&format!("print {gui}/{server}\n")).count() == 2,
        "{swapped}"
    );
    assert!(at(".local/state/hotswap/grove.json").exists());
    assert!(run(&["upgrade", "--check"], 0).starts_with("grove 2026.10.1: up to date\n"));
    assert!(run(&["upgrade", "--check", "--back"], 2).contains("cannot be used with"));

    // launchd owns the server: `off` boots it out, `reboot` is one kickstart
    put("loaded", "");
    assert_eq!(
        run(&["off"], 0),
        format!(
            "grove server stopped (booted out {gui}/{server}; `grove setup` loads it again)\nprint {gui}/{server}\nbootout {gui}/{server}\nprint {gui}/{server}\nprint {gui}/{server}\n"
        )
    );
    assert_eq!(
        run(&["reboot"], 0),
        format!(
            "grove server restarting under launchd ({gui}/{server})\nprint {gui}/{server}\nkickstart -k {gui}/{server}\n"
        )
    );
    // a swap holding hotswap's lock refuses the remove before anything is booted out
    let lock = fs::File::create(at(".local/state/hotswap/grove.lock")).expect("lock");
    lock.try_lock().expect("held");
    assert_eq!(
        run(&["setup", "--remove"], 1),
        format!("grove: another hotswap is running\nprint-disabled {gui}\n")
    );
    drop(lock);
    assert_eq!(
        run(&["setup", "--remove"], 0),
        format!(
            "booted out {server}\nremoved {}\nremoved {h}/.local/bin/grove\n{kept}bootout {gui}/{server}\nprint {gui}/{server}\nprint {gui}/{server}\n",
            plists[0]
        )
    );
    // forget took the upgrade agent's plist, the record and the layout
    assert!(
        !Path::new(&plists[1]).exists()
            && !at(".local/state/hotswap/grove.json").exists()
            && !layout.exists()
    );
}

/// The first release on hotswap, as every host meets it: mise installed grove, its shim
/// behind `~/.local/bin/grove` and both agents, and the old release's tick runs the new
/// binary's `setup` through the shim. Setup moves mise's versions into the layout, links
/// `~/.local/bin/grove` through `current`, puts the server on the link and leaves the upgrade
/// agent to hotswap's waiter, which reloads it on the link; mise's own dirs are left alone.
/// The next tick then runs from the link, with no mise in its path.
#[test]
fn setup_through_mise_moves_the_host_into_the_layout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = tmp.path().display().to_string();
    let real = fs::canonicalize(tmp.path()).expect("canonical");
    let at = |rel: &str| tmp.path().join(rel);
    let [server, upgrade] = ["dev.gigantic.grove", "dev.gigantic.grove.upgrade"];
    let plist = |label: &str| at(&format!("Library/LaunchAgents/{label}.plist"));
    // `on.<label>` is a loaded agent; the upgrade agent's `print` names its plist, its state
    // and the program its last bootstrap read, `program`
    put(
        &at("stub/launchctl"),
        &format!(
            "#!/bin/sh\nlog=log; case \"$(ps -ww -o command= -p $PPID)\" in *bootout*) log=waiter.log ;; esac\necho \"$*\" >> \"$HOME/$log\"\nl=${{2##*/}}\ncase $1 in\nprint) [ -e \"$HOME/on.$l\" ] || exit 113; [ \"$l\" = {upgrade} ] && printf 'path = %s\\nstate = not running\\nprogram = %s\\n' \"$HOME/Library/LaunchAgents/$l.plist\" \"$(cat \"$HOME/program\")\" ;;\nbootout) rm \"$HOME/on.$l\" ;;\nbootstrap) l=$(basename \"$3\" .plist); touch \"$HOME/on.$l\"; [ \"$l\" = {upgrade} ] && echo \"$HOME/.local/bin/grove\" > \"$HOME/program\" ;;\nesac\nexit 0"
        ),
    );
    put(&at("stub/curl"), CURL);
    // mise's layout: an older version, this binary as the active one, a version alias, and
    // a shim that execs the active version as mise's does
    let installs = at(".local/share/mise/installs/packslip-jgeschwendt-grove");
    put(&installs.join("2026.10.0/grove"), "#!/bin/sh\n");
    fs::create_dir_all(installs.join("2026.10.1")).expect("installs");
    fs::copy(
        env!("CARGO_BIN_EXE_grove"),
        installs.join("2026.10.1/grove"),
    )
    .expect("copy");
    symlink("./2026.10.1", installs.join("latest")).expect("alias");
    let shim = at(".local/share/mise/shims/grove");
    let exec = format!(
        "#!/bin/sh\nexec '{}/2026.10.1/grove' \"$@\"\n",
        installs.display()
    );
    put(&shim, &exec);
    fs::create_dir_all(at(".local/bin")).expect("bin");
    symlink(&shim, at(".local/bin/grove")).expect("link");
    let mise_tree = || {
        let mut v = (fs::read_dir(&installs).expect("installs").flatten())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        v.sort();
        v
    };
    let before = mise_tree();
    for label in [server, upgrade] {
        put(&plist(label), "on mise's shim\n");
        put(&at(&format!("on.{label}")), "");
    }
    fs::write(at("program"), shim.display().to_string()).expect("program");
    // the old release's last green record, which hotswap reads as it is
    put(
        &at(".local/state/hotswap/grove.json"),
        r#"{"at":0,"ok":true,"smoke":[],"version":"2026.10.0"}"#,
    );

    let bind = closed();
    let gui = gui();
    let (code, moved) = launch(&shim, &h, &bind, &["setup"]);
    let layout = at(".local/share/hotswap/grove");
    assert_eq!(code, Some(0), "{moved}");
    assert_eq!(
        moved,
        format!(
            "linked {h}/.local/bin/grove → {}/.local/share/hotswap/grove/2026.10.1/grove\nreloaded {server}\nprint-disabled {gui}\nprint {gui}/{upgrade}\nprint {gui}/{server}\nbootout {gui}/{server}\nprint {gui}/{server}\nbootstrap {gui} {}\n",
            real.display(),
            plist(server).display()
        )
    );
    // the waiter, detached, reloads the upgrade agent from the plist ensure rewrote
    let agent_log = at(&format!("Library/Logs/{upgrade}.log"));
    let mut reloaded = String::new();
    for _ in 0..200 {
        reloaded = fs::read_to_string(&agent_log).unwrap_or_default();
        if !reloaded.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        reloaded,
        format!(
            "hotswap: {gui}/{upgrade} reloaded from {}\n",
            plist(upgrade).display()
        )
    );
    let waited = fs::read_to_string(at("waiter.log")).unwrap_or_default();
    assert!(
        waited.ends_with(&format!(
            "bootout {gui}/{upgrade}\nprint {gui}/{upgrade}\nbootstrap {gui} {}\n",
            plist(upgrade).display()
        )),
        "{waited}"
    );
    drop(fs::remove_file(at("waiter.log")));
    // the end state: both versions in the layout, the running one current; the link through
    // `current`; both agents on the link, loaded, no shim or mise in either plist
    let mut versions = (fs::read_dir(&layout).expect("layout").flatten())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    versions.sort();
    assert_eq!(versions, ["2026.10.0", "2026.10.1", "current"]);
    assert_eq!(
        fs::read_link(layout.join("current")).expect("current"),
        Path::new("2026.10.1")
    );
    assert_eq!(
        fs::read(layout.join("2026.10.1/grove")).expect("moved"),
        fs::read(env!("CARGO_BIN_EXE_grove")).expect("binary")
    );
    assert_eq!(
        fs::read_link(at(".local/bin/grove")).expect("link"),
        layout.join("current/grove")
    );
    // both run the link; the server's PATH keeps mise's shims dir, which exists here, after
    // `~/.local/bin`, for what git runs; the upgrade agent's names no mise
    let link = format!("<string>{h}/.local/bin/grove</string>");
    let path = format!(
        "<key>PATH</key><string>{h}/.local/bin:{h}/.local/share/mise/shims:/usr/local/bin:/usr/bin:/bin</string>"
    );
    for label in [server, upgrade] {
        let text = fs::read_to_string(plist(label)).expect("plist");
        let on_shims = text.contains(&path);
        let mise = text.matches("mise").count();
        assert!(
            text.contains(&link) && on_shims == (label == server),
            "{text}"
        );
        assert_eq!(mise, usize::from(label == server), "{text}");
        assert!(at(&format!("on.{label}")).exists());
    }
    // mise's dirs as mise left them
    assert_eq!(mise_tree(), before);
    assert_eq!(fs::read_to_string(&shim).expect("shim"), exec);
    assert!(installs.join("2026.10.1/grove").exists());

    // the next tick, from the link: ensure recorded 2026.10.1, so it is up to date and nothing
    // reloads; 2026.10.0, mise's copy of a release before this crate, stays as the rollback
    // target, so mise's copy is still needed and the `mise unuse` hint waits for the swap that
    // prunes it
    let (code, tick) = launch(&at(".local/bin/grove"), &h, &bind, &["upgrade"]);
    assert_eq!(code, Some(0), "{tick}");
    assert!(
        tick.starts_with("grove 2026.10.1: up to date\n")
            && !tick.contains(&format!("bootout {gui}/{upgrade}"))
            && !tick.contains("needs mise"),
        "{tick}"
    );
    assert!(layout.join("2026.10.0").is_dir());
}

/// A server agent loaded at login carries a managed launch constraint, which a `kickstart -k`
/// after a swap trips: the swap reloads it instead, bootout then bootstrap from its own plist,
/// and never kickstarts it.
#[test]
fn upgrade_reloads_a_server_under_a_managed_launch_constraint() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = tmp.path().display().to_string();
    let server = "dev.gigantic.grove";
    // the server's `print` names its plist and the constraint; a bootout makes the next
    // `print` fail once, as a job leaving launchd does
    put(
        &tmp.path().join("stub/launchctl"),
        &format!(
            "#!/bin/sh\necho \"$*\" >> \"$HOME/log\"\nl=${{2##*/}}\ncase $1 in\nbootout) touch \"$HOME/gone\" ;;\nprint) [ \"$l\" = {server} ] || exit 0; [ -e \"$HOME/gone\" ] && {{ rm \"$HOME/gone\"; exit 1; }}; printf 'path = %s/Library/LaunchAgents/%s.plist\\nproperties = managed LWCR | has LWCR\\n' \"$HOME\" \"$l\" ;;\nesac\nexit 0"
        ),
    );
    put(&tmp.path().join("stub/curl"), CURL);
    let grove = Path::new(env!("CARGO_BIN_EXE_grove"));
    let layout = tmp.path().join(".local/share/hotswap/grove");
    fs::create_dir_all(layout.join("2026.10.1")).expect("layout");
    symlink(grove, layout.join("2026.10.1/grove")).expect("version");
    symlink("2026.10.1", layout.join("current")).expect("current");
    let bind = closed();
    // setup writes the plists, so the swap's own setup step leaves the server alone
    assert_eq!(launch(grove, &h, &bind, &["setup"]).0, Some(0));
    let (code, swapped) = launch(grove, &h, &bind, &["upgrade"]);
    let (gui, label) = (gui(), format!("{server}.plist"));
    assert_eq!(code, Some(0), "{swapped}");
    assert!(
        swapped.contains(&format!(
            "bootout {gui}/{server}\nprint {gui}/{server}\nbootstrap {gui} {h}/Library/LaunchAgents/{label}\n"
        )) && !swapped.contains("kickstart"),
        "{swapped}"
    );
}
