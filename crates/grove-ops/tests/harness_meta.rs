//! The meta-check: every test in this tree is reachable from `mise run test`.
//!
//! v1 grew tests no gate ever ran — a TS wire test in neither `mise run test` nor CI,
//! a release smoke script that was local-only — so they rotted unnoticed. The rule
//! here is that a test file must be a target Cargo itself discovers (`crates/<member>/
//! tests/*.rs`, or an inline `mod tests`), the member must be in the workspace, and
//! `mise run test` must sweep the whole workspace. A test parked anywhere else fails
//! this file with instructions, rather than quietly running nowhere.
//!
//! Paired with the no-`#[ignore]` rule: an ignored test runs under neither nextest
//! profile, so it is the same defect one attribute deeper — and a reason string is
//! not a gate.

use std::path::{Path, PathBuf};

const WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// This file's own path, relative to the workspace root — the one entry every honest
/// inventory contains.
const SELF: &str = "crates/grove-ops/tests/harness_meta.rs";

/// Directories that hold no source of ours. Only the fallback walker consults these —
/// git already knows what it does not track.
const PRUNED: &[&str] = &[".git", ".stele", "node_modules", "target"];

/// Every file git tracks under the workspace root, as paths relative to that root
/// with `/` separators.
///
/// git, not a directory walk: these rules judge the *committed* tree, which is what
/// CI runs and what a reviewer reads. A walk answers a different question and gets
/// both halves wrong — a developer's scratch `smoke-notes.md` turns the suite red on
/// their machine and nowhere else, while a genuinely stranded test that is gitignored
/// stays invisible to CI. The walk survives only as the fallback for a tree with no
/// git available (a source tarball), where a stale answer beats no answer.
fn tracked_files() -> Vec<String> {
    let root = std::fs::canonicalize(WORKSPACE).expect("workspace root resolves");
    let mut out = git_ls_files(&root).unwrap_or_else(|| {
        let mut walked = Vec::new();
        walk(&root, &root, &mut walked);
        walked
    });
    out.sort();
    // An empty inventory is never a real answer: this file is itself tracked, so
    // zero files means the listing was redirected (see [`git_ls_files`]) or the walk
    // found nothing. Every rule below iterates the list, so silence here would pass
    // four of them having examined nothing — the "gate that runs nowhere" class this
    // file exists to close, wearing the gate's own uniform.
    assert!(
        out.iter().any(|rel| rel == SELF),
        "the file inventory under {} does not contain {SELF} — the listing was \
         redirected (an inherited GIT_DIR/GIT_INDEX_FILE overrides `-C`) or the tree \
         is not a checkout. Every rule below iterates this list, so an empty or \
         foreign one would pass them having examined nothing.",
        root.display()
    );
    out
}

/// `git ls-files` under `root`, or `None` when git can't answer (absent, or the tree
/// is not a checkout). A tracked path git lists but the tree no longer holds — a
/// deletion staged in the index — is dropped: nothing here can read it.
///
/// [`grove_ops::git::git_command`], not a bare `Command::new("git")`: git's
/// local-repo environment (`GIT_DIR`, `GIT_INDEX_FILE`, …) **overrides `-C`**, and a
/// suite run from inside a git hook, `rebase --exec` or `bisect run` inherits it — so
/// a raw spawn lists a foreign repo's files, every one of which fails the `is_file`
/// filter below, and the inventory comes back empty.
fn git_ls_files(root: &Path) -> Option<Vec<String>> {
    let out = grove_ops::git::git_command()
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8(out.stdout)
            .ok()?
            .split('\0')
            .filter(|rel| !rel.is_empty() && root.join(rel).is_file())
            .map(str::to_owned)
            .collect(),
    )
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !PRUNED.contains(&name.as_str()) {
                walk(&path, root, out);
            }
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// A Rust source file (case-insensitively, as Cargo sees it).
fn is_rust(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
}

fn read(rel: &str) -> String {
    let path = PathBuf::from(WORKSPACE).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Rust files that look like tests to a human: anything under a `test`/`tests`
/// directory, or named in one of the conventional test spellings.
///
/// Rust-only by construction. The rule this feeds is about Cargo *test targets*, and
/// its remedy — "move it to `crates/<member>/tests/<name>.rs`" — is only possible for
/// a `.rs` file: a `docs/smoke-testing.md` or an `install_smoke.sh` matched the
/// spellings and could never satisfy the rule, so the gate had one permanent
/// unfixable failure waiting for the first such file. A script's reachability is a
/// different claim, enforced where scripts are wired in — the CI/mise assertions in
/// [`ci_runs_the_same_gate_as_local`].
fn is_test_shaped(rel: &str) -> bool {
    if !is_rust(rel) {
        return false;
    }
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let stem = name.split('.').next().unwrap_or(name);
    rel.split('/').any(|c| c == "test" || c == "tests")
        || stem.ends_with("_test")
        || stem.starts_with("test_")
        || name.contains(".test.")
        || name.contains("_test.")
        || name.contains("smoke")
}

/// A Cargo-discovered integration target: `crates/<member>/tests/<name>.rs`, with no
/// intervening directory (nested files there are helper modules, not targets).
fn integration_target(rel: &str) -> Option<&str> {
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.as_slice() {
        ["crates", member, "tests", file] if is_rust(file) => Some(member),
        _ => None,
    }
}

#[test]
// stele:landmark test-reachability
fn every_test_file_is_reachable_from_mise_run_test() {
    let files = tracked_files();

    // 1. No test-shaped file sits outside a Cargo-discovered integration target.
    let stranded: Vec<&String> = files
        .iter()
        .filter(|rel| is_test_shaped(rel) && integration_target(rel).is_none())
        .collect();
    assert!(
        stranded.is_empty(),
        "test-shaped files no gate runs: {stranded:?}\n\
         Every test must be an inline `mod tests` or `crates/<member>/tests/<name>.rs`. \
         If one of these is genuinely a test, move it there; if it is a fixture, keep \
         it out of a `test`/`tests` directory and off the test-name spellings."
    );

    // 2. Every crate holding an integration target is a workspace member, so
    //    `--workspace` actually sweeps it.
    let root_manifest = read("Cargo.toml");
    for member in files.iter().filter_map(|rel| integration_target(rel)) {
        assert!(
            root_manifest.contains(&format!("\"crates/{member}\"")),
            "crates/{member} has integration tests but is not a workspace member"
        );
        let manifest = read(&format!("crates/{member}/Cargo.toml"));
        assert!(
            !manifest.contains("autotests"),
            "crates/{member} touches `autotests`, which can drop tests/ targets \
             from the run without any test failing"
        );
    }

    // 3. `mise run test` sweeps the workspace on both runners — nextest skips
    //    doctests entirely, so the doc pass is not optional.
    let mise = read("mise.toml");
    let test_task = mise
        .split("[tasks.test]")
        .nth(1)
        .expect("mise.toml defines a `test` task");
    assert!(
        test_task.contains("nextest run --workspace"),
        "`mise run test` must run nextest across the whole workspace"
    );
    assert!(
        test_task.contains("--doc --workspace"),
        "`mise run test` must run doctests across the whole workspace"
    );
}

/// The rule's second clause: **CI runs exactly `mise run test`**. Asserting only the
/// mise side leaves it half-enforced — CI could stop invoking the task, or drop the
/// profile that selects the slow tier, and every test in the tree would still pass.
#[test]
fn ci_runs_the_same_gate_as_local() {
    let ci = read(".github/workflows/ci.yml");

    assert!(
        ci.contains("mise run check"),
        "CI must run `mise run check` — the lint/format gate is not CI-specific"
    );
    let (_, after_test) = ci
        .split_once("run: mise run test")
        .expect("CI must run `mise run test`, not a hand-rolled cargo invocation");

    // The slow tier's entire execution hangs on this one line: `.config/nextest.toml`
    // excludes `slow_*` under the default profile, so without NEXTEST_PROFILE=ci the
    // tier runs NOWHERE, with a green build. Pinned to the `mise run test` step (the
    // text right after it) so moving the env elsewhere fails here.
    let step = after_test.split("\n      - ").next().unwrap_or(after_test);
    assert!(
        step.contains("NEXTEST_PROFILE: ci"),
        "the `mise run test` step must set NEXTEST_PROFILE: ci, or the `slow_*` tier \
         never runs anywhere"
    );
}

/// The rule's third clause, for the one test no Cargo target can reach.
///
/// `test/install_smoke.sh` drives `scripts/install.sh` and `scripts/uninstall.sh` as
/// an operator would — a shell script exercising the real binary, which
/// [`is_test_shaped`] deliberately does not judge (its remedy, "move it under
/// `crates/<member>/tests/`", is impossible for a `.sh`). That exemption is only safe
/// while something else pins the script's reachability, which is this: it must be a
/// `mise` task, that task must actually run the script, and CI must invoke the task.
/// v1's release smoke had all three missing and rotted unnoticed.
#[test]
fn the_install_smoke_is_reachable_from_a_gate() {
    let script = "test/install_smoke.sh";
    assert!(
        PathBuf::from(WORKSPACE).join(script).is_file(),
        "{script} is gone — remove this rule with it, or restore the script"
    );

    let mise = read("mise.toml");
    let smoke_task = mise
        .split("[tasks.smoke]")
        .nth(1)
        .expect("mise.toml defines a `smoke` task");
    assert!(
        smoke_task.contains(script),
        "`mise run smoke` must run {script}, not a hand-rolled copy of it"
    );

    let ci = read(".github/workflows/ci.yml");
    assert!(
        ci.contains("mise run smoke"),
        "CI must run `mise run smoke` — a smoke no gate runs is the exact hole this \
         repo closed"
    );
}

/// The smoke drives the real installer, and the real installer puts a symlink on
/// PATH. Its search takes the first *writable* candidate and `/usr/local/bin` is
/// first — a property of the host, not of the temp HOME the smoke redirects — so on
/// any box where the invoking user can write it, a "hermetic" run clobbers the
/// operator's own `grove` link and the uninstall step then deletes it. The escape is
/// closed by pinning the destination, and the assertion must not accept the escaping
/// outcome as a pass.
#[test]
fn the_install_smoke_links_only_inside_its_own_home() {
    // Line continuations first: the invocations wrap, and the environment sits on the
    // far side of the backslash from the script name.
    let smoke = read("test/install_smoke.sh").replace("\\\n", " ");
    let invocations: Vec<&str> = smoke
        .lines()
        .filter(|l| l.contains("scripts/install.sh") || l.contains("scripts/uninstall.sh"))
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();
    assert!(!invocations.is_empty(), "the smoke drives the two scripts");
    for line in &invocations {
        assert!(
            line.contains("GROVE_LINK_DIR="),
            "every install/uninstall invocation in test/install_smoke.sh must pin \
             GROVE_LINK_DIR, or the PATH-link step reaches outside the temp HOME the \
             smoke redirects: {line}"
        );
    }
    assert!(
        !smoke.contains("/usr/local/bin"),
        "test/install_smoke.sh must not name /usr/local/bin — asserting on it is how \
         the host-polluting outcome got ratified as a pass"
    );
    let install = read("scripts/install.sh");
    assert!(
        install.contains("GROVE_LINK_DIR"),
        "scripts/install.sh must honour GROVE_LINK_DIR for the smoke to pin anything"
    );
}

/// The slow tier is a naming convention plus two filter expressions; nothing else
/// enforces it. Pin the two expressions, and keep at least one `slow_*` test in the
/// tree (this file's own, below) so the tier is never silently empty.
#[test]
fn the_slow_tier_is_configured_and_inhabited() {
    let nextest = read(".config/nextest.toml");
    let default = nextest
        .split("[profile.default]")
        .nth(1)
        .and_then(|s| s.split("[profile.").next())
        .expect(".config/nextest.toml defines [profile.default]");
    assert!(
        default.contains("default-filter") && default.contains("slow_"),
        "the default profile must carry a default-filter excluding `slow_*`"
    );
    let ci = nextest
        .split("[profile.ci]")
        .nth(1)
        .expect(".config/nextest.toml defines [profile.ci]");
    assert!(
        ci.contains("default-filter = 'all()'"),
        "the ci profile must select every test, including the `slow_*` tier"
    );

    let inhabited = tracked_files()
        .iter()
        .filter(|rel| is_rust(rel))
        .any(|rel| read(rel).contains("fn slow_"));
    assert!(
        inhabited,
        "no `slow_*` test exists, so the tier ships unproven: a filter that matches \
         nothing (or everything) would pass under both profiles"
    );
}

/// The `slow_*` tier's inhabitant. Deliberately trivial: its job is to make the two
/// profiles' listings differ by exactly one test, so a filter expression that is
/// syntactically valid but semantically wrong is observable (`cargo nextest list`
/// under `default` omits it; under `ci` it appears). Named, not `#[ignore]`d — an
/// ignored test runs nowhere, this one runs in CI.
#[test]
fn slow_tier_is_selectable() {
    assert!(is_test_shaped(SELF));
}

/// Carried law: the clock seam is *the* place wall-clock time is read. A budget that
/// reaches `Instant::now()` directly is untestable without real waits — the flake
/// class the seam exists to kill — and silently un-fakes whatever ladder it belongs
/// to. Enforced mechanically rather than by the module doc's say-so: the moment a
/// daemon lane, a drain ladder, or a retry loop reads the wall clock inline, this
/// fails and names the file.
#[test]
// stele:landmark clock-seam
fn every_wall_clock_read_goes_through_the_clock_seam() {
    // Built at runtime, not written literally: a literal would match this file.
    let needle = format!("{}::now(", "Instant");
    let seam = "crates/grove-ops/src/clock.rs";

    // Line comments are prose about the rule, not a use of it — a doc comment that
    // names the call it forbids must not trip its own check.
    let offenders: Vec<String> = tracked_files()
        .into_iter()
        .filter(|rel| is_rust(rel) && rel.as_str() != seam)
        .filter(|rel| {
            read(rel)
                .lines()
                .any(|l| !l.trim_start().starts_with("//") && l.contains(&needle))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "wall-clock reads outside {seam}: {offenders:?}\n\
         Take a `&dyn Clock` (or a `Deadline` computed by the caller) and read time \
         through it — `clock.deadline(budget)` / `deadline.expired(&clock)`. \
         `SystemClock` is wired at the production call site; tests drive `TestClock`."
    );
}

/// Hygiene: a scratch directory a test writes must be reclaimed when the test ends.
///
/// Rust never runs destructors for `static`s, so a `static` holding a temporary
/// directory is one whose cleanup can never fire — it is not "the process lifetime",
/// it is forever, and under nextest (one process per test) it is one leaked tree per
/// test. The class is invisible to `Drop`, invisible on a CI runner that is thrown
/// away, and paid for entirely on a developer's disk: the instance this rule replaces
/// had accumulated 2.9 GB across 12k directories before anyone counted.
///
/// The fix is always the same shape: build the scratch inside the caller's own
/// temporary directory, where ordinary `Drop` reclaims it.
#[test]
fn no_scratch_directory_is_held_in_a_static() {
    // Built at runtime, not written literally: a literal would match this file.
    let needle = format!("{}Dir", "Temp");
    let offenders: Vec<String> = tracked_files()
        .into_iter()
        .filter(|rel| is_rust(rel))
        .filter(|rel| {
            read(rel).lines().any(|line| {
                let decl = line.trim_start();
                (decl.starts_with("static ") || decl.starts_with("pub static "))
                    && line.contains(&needle)
            })
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "a temporary directory bound to a `static` in {offenders:?} — its `Drop` \
         never runs, so the directory is never removed. Build fixtures inside the \
         caller's own TempDir instead, where ordinary scope reclaims them."
    );
}

/// The reachability law, closed at its own back door.
///
/// The earlier form of this rule accepted `#[ignore = "reason"]` and rejected only the
/// bare attribute — which blessed exactly what the law forbids: a reasoned ignore is
/// still a test no gate runs, and the one this repo carried (a live-network probe
/// against api.github.com) was invisible to both nextest profiles while reading as
/// covered. A comment cannot make a test run, so there is no reason to accept: the
/// rule is now flat.
#[test]
fn no_test_is_parked_behind_ignore() {
    let offenders: Vec<String> = tracked_files()
        .iter()
        .filter(|rel| is_rust(rel))
        .filter(|rel| {
            read(rel)
                .lines()
                .any(|l| l.trim_start().starts_with("#[ignore"))
        })
        .cloned()
        .collect();
    assert!(
        offenders.is_empty(),
        "`#[ignore]` in {offenders:?} — an ignored test runs under neither nextest \
         profile, so it is a test no gate runs whatever its reason string says. For a \
         merely slow test, name it `slow_*`: the `ci` profile runs that tier. For one \
         that needs the network or an operator's machine, it is not a unit test — move \
         it to `scripts/` where `test/install_smoke.sh` already lives, or delete it."
    );
}
