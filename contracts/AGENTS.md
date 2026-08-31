# contracts

```stele
kind: container
purpose: >-
  wire-vocab.json: every status string, error code, event name and payload field name on
  the HTTP surface, generated from the producing Rust types. Grove-side drift detection
  only: no consumer reads it.
invariants:
  - claim: "wire-vocab.json is a generated projection of the producing types (grove-ops for the operation vocabularies, grove-api for the HTTP ones), never hand-edited; the snapshot test fails on any drift, forcing a deliberate re-bless (BLESS_WIRE=1) plus a regen on each consuming side"
    anchor: crates/grove-api/tests/wire_vocab.rs#wire_vocab_matches_fixture
    enforced_by: crates/grove-api/tests/wire_vocab.rs
```

<!-- stele:begin router -->
<!-- stele:end -->
