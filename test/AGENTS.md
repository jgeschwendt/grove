# test

```stele
kind: container
purpose: >-
  The one test in this tree that is not a cargo target: install_smoke.sh drives install.sh
  and uninstall.sh as an operator would, hermetically, against a fixture release.
commands:
  smoke: mise run smoke
invariants:
  - claim: "every test file in this tree is reachable from `mise run test`, and CI runs exactly that gate — a test parked where no gate runs it fails the meta-check with instructions rather than rotting unnoticed"
    anchor: lm:test-reachability
    enforced_by: crates/grove-ops/tests/harness_meta.rs
  - claim: "the install smoke links only inside its own GROVE_HOME, so a run can never plant a symlink in the operator's real PATH directories"
    anchor: crates/grove-ops/tests/harness_meta.rs#the_install_smoke_links_only_inside_its_own_home
    enforced_by: crates/grove-ops/tests/harness_meta.rs
```

<!-- stele:begin router -->
<!-- stele:end -->
