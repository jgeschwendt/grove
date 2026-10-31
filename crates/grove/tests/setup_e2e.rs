//! `grove setup [--remove]` and `grove upgrade` on a fabricated home: stub `launchctl`, `mise`
//! and `curl` first on `$PATH`, mise's shim a link to the built binary, so hotswap's drain,
//! setup and smoke steps run the real `grove` — against a closed `GROVE_BIND`, never the
//! operator's server.

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;
use std::process::Command;

#[test]
fn setup_installs_both_agents_upgrade_swaps_and_remove_undoes_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = tmp.path().display().to_string();
    let put = |rel: &str, text: &str| {
        let path = tmp.path().join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        fs::write(&path, text).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    };
    let gone = |rel: &str| drop(fs::remove_file(tmp.path().join(rel)));
    // `print` answers loaded while `loaded` exists; `print-disabled` prints `disabled`; `fail`
    // fails the bootstrap of the agent it names
    put(
        "stub/launchctl",
        "echo \"$*\" >> \"$HOME/log\"\n[ \"$1\" = print ] && { [ -e \"$HOME/loaded\" ]; exit; }\n[ \"$1\" = print-disabled ] && { cat \"$HOME/disabled\"; exit 0; }\n[ \"$1\" = bootstrap ] && [ -e \"$HOME/fail\" ] && case $3 in *$(cat \"$HOME/fail\").plist) exit 5 ;; esac\nexit 0",
    );
    put(
        "stub/mise",
        "case $1 in ls) echo '{\"packslip:jgeschwendt/grove\":[{\"active\":true,\"installed\":true,\"version\":\"2026.10.1\"}]}' ;; latest) echo 2026.10.1 ;; esac",
    );
    put("stub/curl", "echo \"curl $*\" >> \"$HOME/log\"");
    put("disabled", "");
    let bind = TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("port");
    let run = |args: &[&str], code: i32| {
        let out = Command::new(env!("CARGO_BIN_EXE_grove"))
            .args(args)
            // not env_clear: llvm-cov's profile path must reach the binary
            .env_remove("GROVE_HOME")
            .env_remove("MISE_DATA_DIR")
            .env_remove("XDG_DATA_HOME")
            .env("GROVE_BIND", bind.to_string())
            .env("HOME", &h)
            .env("PATH", format!("{h}/stub:/usr/bin:/bin"))
            .output()
            .expect("grove");
        let log = fs::read_to_string(tmp.path().join("log")).unwrap_or_default();
        gone("log");
        let text = format!(
            "{}{}{log}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.status.code(), Some(code), "{args:?}: {text}");
        text
    };
    let uid = Command::new("id").arg("-u").output().expect("id").stdout;
    let gui = format!("gui/{}", String::from_utf8_lossy(&uid).trim());
    let agents = format!("{h}/Library/LaunchAgents");
    let shim = format!("{h}/.local/share/mise/shims/grove");
    let [server, upgrade] = ["dev.gigantic.grove", "dev.gigantic.grove.upgrade"];
    let boot =
        |label: &str| format!("print {gui}/{label}\nbootstrap {gui} {agents}/{label}.plist\n");
    let linked = format!("linked {h}/.local/bin/grove → {shim}\n");

    assert_eq!(
        run(&["setup"], 1),
        format!(
            "grove: no mise shim at {shim}: install with `mise use -g packslip:jgeschwendt/grove` first\nprint-disabled {gui}\n"
        )
    );
    fs::create_dir_all(Path::new(&shim).parent().expect("shims")).expect("shims");
    symlink(env!("CARGO_BIN_EXE_grove"), &shim).expect("shim");
    put(".local/share/grove/current", "");
    put("Library/LaunchAgents", "in the way");
    assert!(run(&["setup"], 1).contains(&format!("grove: {agents}/{server}.plist: ")));
    gone("Library/LaunchAgents");
    assert_eq!(
        run(&["setup"], 0),
        format!(
            "{linked}{h}/.local/share/grove/current is no longer read: grove runs from mise; remove {h}/.local/share/grove when ready\nloaded {server}\nloaded {upgrade}\nprint-disabled {gui}\n{}{}",
            boot(server),
            boot(upgrade)
        )
    );
    let plists = [server, upgrade].map(|label| format!("{agents}/{label}.plist"));
    let check = "import plistlib, sys; [plistlib.load(open(p, 'rb')) for p in sys.argv[1:]]";
    let python = Command::new("python3")
        .args(["-c", check])
        .args(&plists)
        .status();
    assert!(python.expect("python3").success());
    assert!(
        fs::read_to_string(&plists[0])
            .expect("plist")
            .contains(&format!("<string>{shim}</string>"))
    );
    gone(".local/share/grove/current");

    // loaded: a changed plist reloads, the upgrade agent's never
    put("loaded", "");
    for plist in &plists {
        fs::write(plist, "old\n").expect("stale");
    }
    assert_eq!(
        run(&["setup"], 0),
        format!(
            "{linked}reloaded {server}\ncurrent {upgrade}\nprint-disabled {gui}\nprint {gui}/{server}\nbootout {gui}/{server}\nbootstrap {gui} {agents}/{server}.plist\nprint {gui}/{upgrade}\n"
        )
    );
    // a failed bootstrap fails the run, the other agent still loaded
    gone("loaded");
    put("fail", server);
    assert!(run(&["setup"], 1).contains(&format!(
        "loaded {upgrade}\ngrove: launchctl bootstrap {server} failed\n"
    )));
    gone("fail");
    // a disabled agent is held: its plist and a hint, no bootstrap, and setup is green
    put("disabled", &format!("\t\"{server}\" => disabled\n"));
    assert!(run(&["setup"], 0).contains(&format!(
        "loaded {upgrade}\n{server}: disabled by launchctl; launchctl enable {gui}/{server} && grove setup loads it\nprint-disabled {gui}\n{}",
        boot(upgrade)
    )));
    put("disabled", "");

    // the swap: drain, setup, the server's kickstart and both probes run the shim's binary
    let swapped = run(&["upgrade"], 0);
    assert!(
        swapped.starts_with("grove 2026.10.1: green, 2 smoke probes\n")
            && swapped.contains(&format!("kickstart -k {gui}/{server}\ncurl -fsS ")),
        "{swapped}"
    );
    assert!(tmp.path().join(".local/state/hotswap/grove.json").exists());
    assert!(run(&["upgrade", "--check"], 0).contains("2026.10.1"));
    assert!(run(&["upgrade", "--check", "--back"], 2).contains("cannot be used with"));

    put("loaded", "");
    assert_eq!(
        run(&["setup", "--remove"], 0),
        format!(
            "booted out {server}\nbooted out {upgrade}\nremoved {}\nremoved {}\nremoved {h}/.local/bin/grove\nprint-disabled {gui}\nbootout {gui}/{server}\nbootout {gui}/{upgrade}\n",
            plists[0], plists[1]
        )
    );
    assert!(!tmp.path().join(".local/state/hotswap/grove.json").exists());
}
