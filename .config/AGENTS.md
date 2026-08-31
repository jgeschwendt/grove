# .config

```stele
kind: container
purpose: >-
  nextest profiles. `default` (local + pre-commit) excludes the slow tier; `ci`
  (NEXTEST_PROFILE=ci) runs everything and reports slow tests. The tier is a `slow_` name prefix, not an attribute.
commands:
  test: mise run test
invariants:
  - claim: "the slow tier is a NAMING convention enforced by a nextest filter — a test named slow_* is excluded by the default profile and included by ci — so a tier is visible at the call site and needs no registration; #[ignore] is forbidden, since an ignored test runs under neither profile"
    anchor: crates/grove-ops/tests/harness_meta.rs#the_slow_tier_is_configured_and_inhabited
    enforced_by: crates/grove-ops/tests/harness_meta.rs
```

<!-- stele:begin router -->
<!-- stele:end -->
