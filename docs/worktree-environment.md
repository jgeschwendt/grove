# The worktree environment

A root's worktrees are separate checkouts of one repository, so each starts without the
untracked files a developer's `.trunk` accumulated — `.env`, a local config, a credentials
file. **Shares** close that gap: a root declares files it wants present in every worktree,
and grove materializes them.

`grove_ops::env` is the sole owner of share filesystem mutation. Nothing else in the tree
creates, repoints, or removes a share.

## Declaring

```toml
[roots."owner/name".env]
_.symlink = [".env", "config/app.json"]   # live: a link to the .trunk source
_.copy    = ["config/local.json"]         # seeded once: an independent real file
```

Two modes, and the asymmetry between them is the whole design:

| | `_.symlink` | `_.copy` |
|---|---|---|
| what lands in the worktree | a relative symlink to `../…/.trunk/<p>` | a real file, seeded from `.trunk/<p>` |
| edits | shared — one file, every worktree sees it | independent per worktree |
| re-run behaviour | self-heals: a stale grove link is repointed | seed-once: never overwritten |
| `--fix` | backs a conflict up, then links | no bearing — a copy has no wrong state to heal |
| on undeclare | GC'd (it holds no data) | left in place (it is a real file the worktree owns) |

A symlink is identifiable as grove's own — its target points under `.trunk` — so it can be
healed and collected. A seeded copy is indistinguishable from a file the user wrote, so it
is never overwritten and never collected. `_.setup`, a per-worktree setup command, is
reserved in the schema and not implemented.

A path declared in both lists is materialized as a symlink: `manifest::list_shares` gives
`_.symlink` precedence.

## The source is `.trunk`

Every share's source is `<root>/.trunk/<p>` — the default-branch checkout. That is what
makes a symlink share *live*: the worktree link and the operator's own `.trunk` are the
same file.

A source pass runs first, per declared share, and creates `.trunk/<p>` empty when it is
missing. A real file or a pre-made directory already there **is** the source and is left
alone.

**D5 — a user symlink at the source is read through, never replaced.** A symlink where the
source belongs is the user's own choice of source (`.trunk/.env → ~/secrets/env`), and
grove respects it: a `_.symlink` share reads *through* it (worktree → `.trunk/<p>` → the
user's target), and a `_.copy` share's `NOFOLLOW` source open refuses it downstream, so the
copy is reported per worktree rather than silently following a redirect. Only a non-file,
non-dir, non-symlink oddity (a fifo, a socket) is replaced, and only because it sits inside
`.trunk`, which is grove's domain.

## Detect → fix

Per declared share `<p>`, per present worktree. `materialize` acts; `diagnose` classifies
identically and **mutates nothing** — a byte-for-byte filesystem snapshot is identical
before and after, which is what `--dry-run` promises and
`diagnose_mutates_nothing`/`copy_diagnose_mutates_nothing` prove.

### `_.symlink`

| state at `<wt>/<p>` | verdict | `materialize` (Safe) | `materialize` (Force) |
|---|---|---|---|
| absent | `linked` | create the relative link | same |
| symlink → exactly `../…/.trunk/<p>` | `ok` | nothing | nothing |
| symlink pointing *under* `.trunk`, wrong path | `repointed` | repoint (temp + same-dir `rename(2)`) | same |
| symlink pointing elsewhere (foreign) | `conflict` | **never touched** | back up, then link → `created` |
| real file / directory / other | `conflict` | **never touched** | back up, then link → `created` |

### `_.copy`

| state at `<wt>/<p>` | verdict | action |
|---|---|---|
| absent | `copied` | seed from `.trunk/<p>` |
| symlink pointing under `.trunk` (a prior `_.symlink` for this path) | `copied` | replace — the link is data-free; this is the symlink → copy migration |
| foreign symlink / real file / directory | `ok` | leave it: the worktree owns it |

`--fix` (`Fix::Force`) is not in that table because it has no bearing on a copy. Seed-once
means there is no wrong state to heal.

### GC on undeclare

A worktree top-level entry that is a symlink to `../.trunk/<name>` for a `<name>` no longer
declared is an orphan and is removed (`gc`). `unlinkat` removes the *link*, never its
target. A real file — including a `_.copy`'s seeded file — or a symlink pointing anywhere
else is left alone. GC is top-level only; nested-share GC is not implemented.

### Statuses

`ShareStatus` is `ok | created | linked | copied | repointed | conflict | gc | error`,
pinned in `contracts/wire-vocab.json` as `share_status`. The would/did distinction is not
carried in the status — `diagnose` reuses `linked`/`repointed`/`created`/`gc` as
"would-link", "would-repoint", and so on — because the renderer and the exit code branch
only on `conflict` and `error`.

The whole pass is best-effort: one bad share or one unopenable worktree pushes an `error`
row and never aborts the batch.

## Never clobber

Only a symlink that is grove's *own* is ever touched. "Grove's own" is a purely lexical
test on the link target — after any leading `..` components, the first segment is `.trunk`
— with no I/O and no canonicalization, so it cannot be steered by what is on disk.

A real file, a directory, or a foreign symlink is reported as `conflict` and left exactly
as it was. `grove doctor` exits **5** on any unresolved conflict, which makes it a usable
CI gate even under `--dry-run`.

`--fix` is the operator's explicit override, and it still does not destroy anything: the
real file is `renameat`'d to the first free `<leaf>.grove-bak`, `<leaf>.grove-bak-1`, …
(never over a prior backup; no timestamp, because there is no clock at this layer and a
stable suffix is more legible), and only then is the link created.

The backup rename runs unlocked, so two concurrent realizers could race the backup name.
In practice mutations for a root are serialized on that root's lane — distinct roots run in
parallel but never share a worktree — and `Force` is an explicit operator action.

## Security posture

Two independent gates, string and filesystem.

**The string gate** is `manifest::validate_share_path`, applied at `list_shares`, so only
canonical, traversal-free paths reach the materializer: relative, `Normal` components only,
no segment beginning with `-`, no `.`/empty/trailing-`/` segments, and a first segment that
is not one of `.git`/`.trunk`/`.pool`. Canonicality matters beyond traversal: the link pass
and the GC pass derive paths differently, so a non-canonical stored string would let them
disagree about the same share.

**The filesystem gate** is a dirfd-pinned `O_NOFOLLOW` descent. Every component of a share
path *within* the worktree is opened with `openat(NOFOLLOW)` from a pinned dirfd, so a
symlinked parent (`config → /etc`) trips `ELOOP` and can never redirect a write outside the
worktree. `open_dir_nofollow` is the only place a parent component is opened — the one place
`NOFOLLOW` could be forgotten. Classify and act share one pinned dirfd, which closes the
lstat → create TOCTOU, and the leaf write is a relative-target temp plus a same-dir
`rename(2)` on that fd. The base worktree and `.trunk` directories themselves sit under the
grove-controlled `$GROVE_HOME` and are opened `NOFOLLOW` on their final component; their
ancestors are grove's own, not attacker-influenced.

No `unsafe`, no `libc` — the syscalls come from `rustix`, and the workspace forbids
`unsafe_code`.

`materialize_refuses_a_destination_under_a_symlinked_parent`,
`copy_refuses_a_destination_under_a_symlinked_parent` and
`ensure_source_refuses_a_symlink_redirect_in_the_trunk` are the regression guards.

## Depth

Shares materialize **only at canonical depth** — a worktree that is a direct sibling of
`.trunk`. The link target is built as one `..` per path component plus `.trunk/<p>`, which
is correct exactly one level under the root.

Warm-pool slots therefore get no shares while they are slots: `<root>/.pool/slot-N` is one
level deeper, the link would dangle at `.pool/.trunk/…`, and the promote move would
invalidate it anyway. `pool::promote` materializes *after* the move, when the worktree has
reached canonical depth. `fill_materializes_no_shares_in_slots` pins the negative half.

## Where materialization is wired

| call site | when | mode |
|---|---|---|
| `worktrees::create` | a cold worktree checkout, right after `git worktree add` | `Fix::Safe`, best-effort |
| `roots::reconcile_one` | after a root's worktrees converge | `Fix::Safe`, best-effort |
| `pool::promote` | after the slot is moved to canonical depth and declared | `Fix::Safe` |
| `doctor::run` | the operator's explicit pass | `diagnose` under `--dry-run`; `Fix::Force` under `--fix`, else `Fix::Safe` |

The first three are best-effort: a share hiccup must never fail a checkout or a root's
reconcile, because doctor and the next reconcile retry it. Doctor is the pass whose result
an operator reads, so its report is the answer.

Only **present** worktrees are visited (`worktrees::list`, filtered on `present`), and
`.trunk` and the reserved namespaces are never treated as worktrees. A root whose `.trunk`
is missing contributes a single `error` row — "trunk missing — clone/realize the root
first" — rather than a per-share pile.

## The report

`ShareOutcome { slug, worktree?, path, status, reason? }` is one row per share per
location; `worktree` is absent for a source-level row. It is the `report` array of
`POST /api/doctor` and of the offline `grove doctor`, and `reason` carries the prose for a
conflict or an error.

`grove doctor`'s verdict comes from this report and nothing else: any `error` row exits 1,
otherwise N unresolved conflicts exit 5, otherwise success. Doctor's git-plumbing checks
(`docs/api.md` § Doctor) are report-only and exit-neutral.
