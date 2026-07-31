# Contributing to beads

Thanks for looking. This guide covers the essentials; [`CLAUDE.md`](CLAUDE.md)
is the full development and verification workflow, and it is written for
whoever is doing the work — human or agent.

## Before you start

beads is a personal project. It is maintained in whatever time is left over
after everything else, and that shapes what you can expect:

- **Pull requests may not be reviewed.** Not because they are unwelcome —
  there simply may not be time. Please do not read silence as a judgement on
  your work.
- **Issues may not get a response,** and there is no target response time.
- **No support is offered.** [`docs/`](docs/) and `br --help` are thorough;
  they are the support.

If you need a change on a schedule you control, **fork the project**. That is
a first-class outcome here, not a fallback — the licence permits it and it
will serve you better than waiting. Bug reports with a clear reproduction are
still genuinely useful even when they go unanswered for a while.

## Prerequisites

- **Rust 1.95+**. That floor is measured against the pinned `Cargo.lock`, not
  inherited from upstream, and it can move if a dependency's requirements
  change.
- **A C compiler.** `rusqlite`'s `bundled` feature compiles SQLite from C
  source, which is what makes a release binary need no system libsqlite3.
- **[go-task](https://taskfile.dev)** — all automation is driven through
  `Taskfile.yml`.
- **[pre-commit](https://pre-commit.com)** — the hooks in
  `.pre-commit-config.yaml`.
- **[cargo-nextest](https://nexte.st)** — the test runner. Install a prebuilt
  binary rather than building from source:
  ```sh
  curl -LsSf https://get.nexte.st/latest/mac | tar zxf - -C "${CARGO_HOME:-$HOME/.cargo}/bin"
  ```
  Substitute `linux` for `mac` as appropriate.
- **[git-cliff](https://git-cliff.org)** — only to cut a release.

There is deliberately no `rust-toolchain.toml`, so whatever `rustup` resolves
from your shell is what builds.

## Getting started

```sh
pre-commit install                       # commit-msg, pre-commit and pre-push hooks
env -u RUSTUP_TOOLCHAIN task build       # the br binary and every test target
```

One `pre-commit install` wires all three stages — `.pre-commit-config.yaml`
declares `default_install_hook_types`, so no `--hook-type` flags are needed.

### Always prefix with `env -u RUSTUP_TOOLCHAIN`

If your shell exports a stale `RUSTUP_TOOLCHAIN`, an unprefixed `cargo test`
or `task check` silently runs on a different toolchain and still reports
success — it does not fail loudly, it just proves the wrong thing. Prefix
every `cargo`, `rustc` and `task` invocation. `task toolchain:check` exists to
catch this and is the first step of the gate.

## Before you open a pull request

Run the gate. It is exactly what CI runs, and it must exit 0:

```sh
env -u RUSTUP_TOOLCHAIN task check
```

That is `toolchain:check`, `format:check`, `lint` (clippy over every target,
pedantic + nursery) and `test` (the full suite under nextest, then the
doctests). It takes a couple of minutes.

The pre-push hook runs clippy and the licensing guard, so a push will catch
those two even if you forget. Do not skip hooks and do not reach for
`--no-verify`.

## Conventions

- **Commits follow [Conventional Commits](https://www.conventionalcommits.org/)**,
  enforced by a `commit-msg` hook. This is not a style preference:
  `.cliff.toml` builds `CHANGELOG.md` by parsing those prefixes and that
  section becomes the GitHub release body, so a commit that does not parse is
  a commit missing from the release notes. `feat`, `fix`, `docs`, `chore`,
  `build`, `ci`, `test`, `perf`, `style`, `refactor` and `revert` are
  accepted, as is a `!` breaking-change marker. Write the subject for someone
  who was not there.
- **Tests are grouped into a handful of binaries**, not one per file. Add a
  file to `tests/<group>/` and a `#[path]` line to `tests/<group>.rs` rather
  than a new top-level `tests/*.rs`. See "Where a new test goes" in
  [`CLAUDE.md`](CLAUDE.md) — it also records the two traps that bite when a
  test moves between files, both of which are quiet.
- **Do not name upstream projects or describe this project as plain
  MIT-licensed** in any tracked file. Lineage and the licensing rider live in
  [`NOTICE.md`](NOTICE.md) only; link there instead.
  `tests/licensing.rs` enforces both and will fail the build.
- **New dependencies** need a reason: what it solves, and why the standard
  library or an existing dependency is insufficient.

## Releases

Releases are cut by hand, by the maintainer, by pushing a `v*` tag. The full
sequence — bump `Cargo.toml`, generate the changelog section, verify, tag,
push — is in [`docs/RELEASING.md`](docs/RELEASING.md). Read it before touching
`.goreleaser.yaml`, `.cliff.toml`, or anything under `.github/scripts/`.

## Core constraints

beads is a **single binary with SQLite statically linked**, storing issues in
SQLite for fast queries and mirroring them to JSONL so they version-control
and merge alongside your code. The JSONL format and the machine-readable
command output are contracts other tools parse — see
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and
[`docs/AGENT_INTEGRATION.md`](docs/AGENT_INTEGRATION.md). Changes that break
either need a deliberate reason.
