# scripts

```stele
kind: container
purpose: >-
  Install, uninstall, and the release line: release.sh packs this box's bundle,
  publish-canary.sh ships a host-only prerelease, install.sh stages a release and hands off to `grove up`.
commands:
  smoke: mise run smoke
invariants:
  - claim: "install.sh is a thin bootstrap — platform detect, resolve the release, stage the bundle and its sha256 sidecar into a directory — and then hands off to `grove up`, which owns every mutation of the versioned-dir layout"
    anchor: lm:install-hands-off-to-grove-up
    enforced_by: test/install_smoke.sh
```

<!-- stele:begin router -->
<!-- stele:end -->
