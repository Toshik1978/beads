# Architecture

This is the shape a reader needs before touching the code. For the command
surface (flags, exit codes, `--json` schemas) see
[`CLI_REFERENCE.md`](CLI_REFERENCE.md). For the development and verification
workflow — the gate, the toolchain trap, the abort gate — see
[`../CLAUDE.md`](../CLAUDE.md); this document does not repeat that.

## `issues.jsonl` is the source of truth, not the database

Each workspace is a `.beads/` directory. It holds a SQLite database and an
`issues.jsonl` export, but only one of the two is durable: `.beads/*.db` (and
its `-shm`/`-wal`/`.lock` siblings) is listed in `.gitignore`, while
`issues.jsonl` and `attachments/` are tracked. This one fact explains most of
the rest of the design:

- The database is a **derived cache**. `br sync --import-only --rebuild`
  treats JSONL as authoritative and deletes any DB row absent from it; `br
  sync --flush-only` goes the other way (DB → JSONL). A fresh clone has no
  database at all until a command opens one and imports.
- Every mutating command auto-flushes to `issues.jsonl` after it runs (see
  `--no-auto-flush`), and auto-imports a newer JSONL before it runs (see
  `--no-auto-import`), so the two stay converged across `git pull`/`git push`
  without an explicit sync step in the common case.
- `br sync --merge` exists because both sides *can* diverge (concurrent
  processes, a `git pull` that lands mid-session) — it does a three-way merge
  against `.beads/beads.base.jsonl` as the common ancestor, with
  `--force-db`/`--force-jsonl`/`--force` to resolve conflicts explicitly.
- This is why the database can simply be deleted and rebuilt: it is
  disposable in a way the JSONL file is not. See `src/sync/mod.rs` for the
  import/export implementation and `src/sync/path.rs` for the path-allowlist
  rules that keep sync from writing outside `.beads/`.

Import is last-write-wins on `updated_at`, with one rule that follows from
the durability asymmetry above rather than from the timestamps: **when both
sides carry the same `updated_at` but disagree on content, the JSONL wins.**
Equal timestamps mean both sides claim to be the same revision, and a
difference at the same revision is not a conflict to arbitrate — it is a
hand edit to the file that never bumped the timestamp. Since every local
write advances `updated_at` strictly, a database row that differs from the
file at an identical timestamp cannot be unflushed local work. Editing
`.beads/issues.jsonl` by hand and running `br sync --import-only` therefore
does what it looks like it does; it used to report the record as
"up-to-date" and then write the unedited row back over the file.

## Storage engine: `rusqlite` with `bundled` C SQLite

The storage engine is `rusqlite` (see the `[dependencies]` block in
`Cargo.toml`), built with the `bundled` feature. `bundled` compiles the SQLite
C amalgamation from source as part of the build, rather than linking a system
`libsqlite3`. Two consequences follow, both measured rather than assumed:

- **Building from source needs a C compiler.** `cargo install --path .` or
  `cargo build` requires a working `cc` in addition to `rustc` — the previous
  pure-Rust engine this replaced did not need one. `task test:linux` checks
  for `cc` explicitly for this reason (see its comment block in
  `Taskfile.yml`).
- **Every shipped artifact is statically linked.** Because the C amalgamation
  is compiled into the binary rather than dynamically linked, no system
  `libsqlite3` participates at runtime — a release binary needs no C compiler
  *to run*, only to build. `task build:cross` is the standing proof of this
  across every release target: it builds through `cargo-zigbuild` in a
  container and, for several of them, actually runs a `br init` + `br
  create` + `br list` round-trip to confirm the bundled SQLite links and
  works rather than merely compiles. On the musl targets the result is a
  fully static binary with no dynamic dependencies at all.

## `src/storage/conn.rs`: a compatibility shim, not incidental code

`beads` was originally written against a different SQLite implementation.
Porting to `rusqlite` touched a narrow API surface — about a dozen
`Connection` methods, two `Row` accessors, a `SqliteValue` enum and an error
type — but that surface is called from roughly 1,600 call sites across 26
files, about 1,500 of which name `Connection`, `Row`, or `SqliteValue`
directly. `src/storage/sqlite.rs` alone, the file containing most of those
call sites, is on the order of 25,000 lines.

Rewriting those call sites to rusqlite's own idioms in the same change would
have put a huge, high-risk diff through the file that matters most, and
folded an engine swap and a large refactor into one unreviewable commit.
Instead, `src/storage/conn.rs` reproduces the previous engine's call shape on
top of `rusqlite`, so the engine swap is an import-only change at every call
site and every behavioral difference between the two engines is concentrated
in this one file, where it is unit-tested against a real database (see the
module-level doc comment in `src/storage/conn.rs` for the specific coercion
and `execute`-vs-`query_row` differences that were measured).

This is meant to be a **permanent, documented adapter boundary**, not a
temporary shim to be unwound later — new code is expected to keep using it
for consistency, though a future task may narrow its surface.

## Schema versioning

The schema version lives at `CURRENT_SCHEMA_VERSION` in
`src/storage/schema.rs`; as of this writing it is **19**. It is stamped into
SQLite's `PRAGMA user_version` on a freshly created database, and an existing
database is brought forward by a sequence of migrations gated on the stored
`user_version` (`if user_version < N { ... }`), run automatically whenever
the database is opened — there is no separate migration command.

## The `Issue` field set is a published interface

`Issue` (see `src/model/mod.rs`) serializes to both `issues.jsonl` lines and
every command's `--json` output. That serialized field set — currently 26
keys, enumerated in `EXPECTED_JSONL_KEYS` in `tests/storage/schema_shape.rs`,
which is the authority — is not free to change: an external consumer parses
`issues.jsonl`
directly, so adding, removing, or renaming a key is a breaking change to that
consumer, not just an internal refactor.

`tests/storage/schema_shape.rs` pins this from three angles: the full
serialized field set of a fully-populated `Issue`, the subset that must
always be present even on a default/empty `Issue`, and — as an end-to-end
check — that a real `br create` actually writes only declared keys into
`.beads/issues.jsonl`, and no more: every key in the file is a key of `Issue`.
The same file also pins `Dependency` and `Comment`'s wire field sets, and the
database's DDL shape (table/index/foreign-key definitions) independent of
`Issue`, so that a schema or serialization regression fails a targeted test
instead of surviving on a green suite. See that file's module doc comment for
how the expected values were derived (from a database this project actually
produced, not from source text).
