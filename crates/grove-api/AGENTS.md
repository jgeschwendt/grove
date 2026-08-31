# grove-api

```stele
kind: component
purpose: >-
  The shared HTTP contract both ends compile against: the one envelope serializer, every
  route body, the typed vocabularies, and the engine policy curations. Snapshotted to contracts/wire-vocab.json.
commands:
  test: mise exec -- cargo nextest run -p grove-api
  bless-wire: BLESS_WIRE=1 mise exec -- cargo test -p grove-api
invariants:
  - claim: "the envelope is tagged on its `ok` boolean, never on which of data/error is present, and error.data survives the round trip typed — a body that disagrees with its own tag is a decode error"
    anchor: lm:api-envelope
  - claim: "the terminal/transient partition over grove_ops::Error is a total match with no catch-all, so a new error category stops the build until it is classified"
    anchor: crates/grove-api/src/policy.rs#classify_error
edges:
  depends: [crates/grove-ops]
```

<!-- stele:begin router -->

## Anchors in this territory

- lm:api-envelope → src/envelope.rs:30

<!-- stele:end -->
