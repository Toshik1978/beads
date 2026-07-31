# Changelog

What changed in each `br` release and why. Highlights are written by hand; the
commit lists under them are generated with [git-cliff](https://git-cliff.org)
via `TAG=vX.Y.Z task changelog`.

Versions follow [semver](https://semver.org). Commits follow
[Conventional Commits](https://www.conventionalcommits.org).

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

