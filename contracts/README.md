# contracts

Checked-in, cross-language contract fixtures. Machine-generated — do not hand-edit.

## `wire-vocab.json`

The grove **wire vocabulary**: every status string that leaves the process on the HTTP
API — the reconcile/adopt/worktree-outcome/fast-forward/cold-reason/promotion status
strings, share status, per-root engine status, the ops `Error` codes, the HTTP
`error.code` values — plus the **key sets**: the field names of each payload shape the
two read surfaces carry.

A key set is **shallow: one object, one level**. A nested shape gets its own group or it
is pinned by nothing — `log_line_keys` pins the string `"fields"`, not the `{name, value}`
inside it, so `log_field_keys` exists; `root_view_keys` pins `"trunk_status"`, not the nine
`git::Status` names under it, so `git_status_keys` does. A payload shape added under an
existing one needs a group of its own.

The types are the single source of truth. The flow, and where drift surfaces:

1. **Rust** — `crates/grove-ops/src/wire.rs` (+ the serde enums beside their producers
   and `Error::code()`) owns the operation vocabularies; `crates/grove-api/src/`
   (`ErrorCode`, `RootStatus`) owns the HTTP ones.
   `crates/grove-api/tests/wire_vocab.rs` snapshots all of them here — it lives in
   grove-api because that is the end of the dependency that can see every producer. A
   rename → `cargo test -p grove-api` fails. Re-bless:
   `BLESS_WIRE=1 cargo test -p grove-api`, which rewrites this file byte-for-byte in
   the form checked in.
2. **the consuming end** — nothing reads this fixture today. The dashboard decodes the
   wire by hand (`root["status"] || "unknown"`), so a re-blessed rename reaches it as a
   blank badge at runtime, not a broken build.

So the guard is one-sided, and knowing which side matters: a rename cannot happen *here*
by accident — the snapshot test fails until someone re-blesses it — but nothing downstream
fails with it. Closing the loop takes a test on the consuming end that reads this file and
asserts every vocabulary it renders is still spelled this way.
