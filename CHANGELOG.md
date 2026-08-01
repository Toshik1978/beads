# Changelog

What changed in each `br` release and why. Highlights are written by hand; the
commit lists under them are generated with [git-cliff](https://git-cliff.org)
via `TAG=vX.Y.Z task changelog`.

Versions follow [semver](https://semver.org). Commits follow
[Conventional Commits](https://www.conventionalcommits.org).

---

## v1.1.0 — 2026-08-01

Issue bodies now render as markdown when `br` writes to a terminal, a hang
that could leave a repository locked is fixed, and two redundant spellings of
existing features are gone.

### Why this is 1.1.0 and not 2.0.0

Both changes under **Breaking Changes** remove a documented part of the
command surface, and strict [semver](https://semver.org) would make that a
major release. This one stays minor deliberately.

`br` is a personal tool, adapted as its own needs change rather than
stabilised for a userbase. There is nobody on the other end of a
deprecate-then-remove cycle, so a spelling that turns out to be redundant goes
when it is noticed — and both removals here are that: aliases doing nothing
their surviving spellings do not. Spending a major version on each would turn
the number into a running total of housekeeping.

So read the major version as the shape of the tool, not as a promise about
every flag. The **Breaking Changes** list at the top of each release names
what stops working and what to use instead; that list is the contract, not the
version number.

This revises what v1.0.0 said about the command surface being an interface
under semver. It is not one, and continuing to claim it would be the less
useful of the two options.

### Highlights

- **Markdown renders as markdown.** `br show` and `br comments` on a terminal
  render headings, emphasis, inline code, lists, task checkboxes, blockquotes
  and tables instead of printing their source. Piped output, `--no-color` and
  `--json` are byte-identical to before, so `br show <id> | glow -` and
  anything parsing `--json` are unaffected.
- **A hang that could lock a repository is fixed.** Closing a terminal or
  dropping an ssh session while `br` was running could leave the process alive
  forever holding `.beads/.write.lock`, after which every later `br` in that
  repository timed out waiting for it.
- **Two aliases removed.** `br status` (use `br stats`) and `--robot` (use
  `--json`). Details and migration in **Breaking Changes** below.

### ⚠ Breaking Changes

- [4966c58](https://github.com/Toshik1978/beads/commit/4966c58e6989b308b1a9a28d0153232edad2f258) `br status` no longer exists. Use `br stats`, which it was an alias for and which is otherwise unchanged -- same arguments, same output. Clap suggests it by name, so an existing caller gets "unrecognized subcommand 'status' ... tip: some similar subcommands exist: 'stale', 'stats'" rather than a bare failure.
- [e84fdd9](https://github.com/Toshik1978/beads/commit/e84fdd910fe9c7c9389822a737512245473988fc) `--robot` no longer exists on any command. Use `--json`, which is a global flag and so is accepted everywhere `--robot` was. The two produced byte-identical output, verified by diffing them before the change.

### Features

- [4c070c8](https://github.com/Toshik1978/beads/commit/4c070c88499a0c374ee1a6841f09f799a147e04b) feat(format): render markdown into styled, width-aware text
- [af61152](https://github.com/Toshik1978/beads/commit/af6115241fea7c0bdc5ba14e18aed2ea315aa8ff) feat(output): render issue prose and comment bodies as markdown
- [4a33acf](https://github.com/Toshik1978/beads/commit/4a33acfca11a9c85edd4f807bf5c73cb1bd415cc) feat(output): strip markdown from search context snippets
- [4966c58](https://github.com/Toshik1978/beads/commit/4966c58e6989b308b1a9a28d0153232edad2f258) feat(cli)!: remove the status alias, leaving stats
- [e84fdd9](https://github.com/Toshik1978/beads/commit/e84fdd910fe9c7c9389822a737512245473988fc) feat(cli)!: remove the --robot flag, leaving --json

### Bug Fixes

- [4a2a5d6](https://github.com/Toshik1978/beads/commit/4a2a5d6a2ac55d359df2939c7a8b05d3f455b122) fix(release): stop changelog.disable from discarding the release notes
- [bcb8551](https://github.com/Toshik1978/beads/commit/bcb8551decd078f5867d00c2da1ef533368aeb97) fix(format): correct markdown rendering defects found in review
- [75c8ce9](https://github.com/Toshik1978/beads/commit/75c8ce97a1493536f8c155cc62fb990de6825850) fix(output): stop br hanging forever when its terminal hangs up

### Documentation

- [d1e422a](https://github.com/Toshik1978/beads/commit/d1e422a06c5de4f1eb26014156c12b8960010c48) docs: forbid AI attribution trailers in commit messages

### Others

- [97c3288](https://github.com/Toshik1978/beads/commit/97c3288a9308cda5c744762f5f4a0921689204e2) refactor(format): delete the unreachable rich module
- [d5a06f4](https://github.com/Toshik1978/beads/commit/d5a06f4ae1830a394af85dcc7bd7aad68363cc3d) ci: skip the suite for prose-only changes
- [3c94b69](https://github.com/Toshik1978/beads/commit/3c94b69a4dc67f53ed95a5d1e80c50488d426583) ci(changelog): add a Breaking Changes group

---

## v1.0.0 — 2026-07-31

The first release. `br` is a personal, agent-friendly issue tracker: one
SQLite-backed binary with a JSONL export for portability and version control.

It starts at 1.0.0 rather than 0.1.0 on purpose. The `br` binary this project
descends from is already in the wild at 0.2.x, so any 0.x number here would
sort *below* a binary someone may already have installed and would read as a
downgrade. Starting at 1.0.0 also states the intent plainly: the command
surface and the `issues.jsonl` field set are treated as interfaces under
[semver](https://semver.org), not as something still being sketched.

This section has no generated commit list, and that is deliberate rather than
an omission. Every subsequent release lists the commits since its predecessor,
which is a useful thing to read; v1.0.0 has no predecessor, so the same
mechanism would emit the project's entire development history — mostly changes
to code that never shipped in any release. The highlights below are what a
first-time reader actually needs.

### Highlights

- **24 top-level commands.** `br init`, `br create`, `br ready`, `br list`,
  `br show`, `br update`, `br close`, and the rest. Every command's flags,
  exit codes, and `--json` output schema are documented in
  [`docs/CLI_REFERENCE.md`](docs/CLI_REFERENCE.md).
- **A `.beads/` workspace per project**, holding a SQLite database plus an
  `issues.jsonl` export. The JSONL file's serialized field set is a tested
  interface (`tests/storage_schema_shape.rs`), so tools built against it keep
  working across releases.
- **SQLite is statically linked into every published binary.** A downloaded
  release archive needs no C compiler and no system libsqlite3, and a
  libsqlite3 upgrade on the host cannot affect `br` or require a rebuild.
- **Four binaries per release**, covering Linux and macOS on x86_64 and
  aarch64. The Linux builds are musl and fully static — no glibc floor
  anywhere in the support matrix, and the same binary runs on any
  distribution including Alpine.

### Known limits

- No Windows binary. `cargo-zigbuild` does not cover `*-pc-windows-msvc`, and
  the test suite runs on Linux and macOS only; shipping an artifact no test
  has executed would be worse than shipping none.
- Not published to crates.io. GitHub Releases is the only binary distribution
  channel. Building from source with `cargo install --path .` works and needs
  a C compiler locally, because `rusqlite`'s `bundled` feature compiles the
  SQLite amalgamation.

---

