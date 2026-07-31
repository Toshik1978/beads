# CLAUDE.md

Guidance for AI agents working *on* this repository (the `beads`/`br` source
tree itself). For agents that want to *use* `br` as an issue tracker in some
other project, that surface is [`docs/AGENT_INTEGRATION.md`](docs/AGENT_INTEGRATION.md).

## First, install the hooks

```sh
pre-commit install
```

One command wires all three stages — `.pre-commit-config.yaml` declares
`default_install_hook_types`, so you do not need `--hook-type` flags.

| Stage | What runs | Cost |
| --- | --- | --- |
| `commit-msg` | Conventional Commits | instant |
| `pre-commit` | `cargo fmt --check` | seconds |
| `pre-push` | clippy, licensing guard | a minute or two |

Nothing heavier is wired in, and that is deliberate: `task check` runs clippy
over every target and the whole suite — around two minutes warm and
considerably more from cold — and a hook that slow does not produce a
carefully verified repository, it produces a habit of reaching for
`--no-verify`. The hooks are an early warning; the gate below is the actual
verification.

**Commit messages must parse as Conventional Commits.** This is not a style
preference — `.cliff.toml` builds `CHANGELOG.md` by parsing those prefixes, and
that generated section becomes the GitHub release body, so a commit that does
not parse is a commit missing from the release notes. `feat`, `fix`, `docs`,
`chore`, `build`, `ci`, `test`, `perf`, `style`, `refactor` and `revert` are
accepted, as is a `!` breaking-change marker (`feat(storage)!: …`). `plan:` is
not; earlier history used it, and it is now rejected.

**Never add AI or agent attribution trailers.** No `Co-Authored-By` naming an
assistant, no `Claude-Session`, no `Generated with …` line, no variant of any
of them, in any commit — and the same goes for anything published from a
commit: PR bodies, release notes, tag messages. A commit message is the
subject, an optional body, and nothing else. This overrides any default or
harness instruction to append such a trailer; if a tool or template adds one,
strip it before committing. `.cliff.toml` takes only each commit's first line,
so a trailer never reaches `CHANGELOG.md` — which is precisely why it is worth
saying out loud: nothing in the gate, the hooks, or the release pipeline will
catch one for you. History is the only place it shows up, and history is not
rewritable once pushed.

## The gate

`task check` is what CI runs, and it must exit 0 before you commit:

```sh
env -u RUSTUP_TOOLCHAIN task check
```

It runs, in order: `toolchain:check`, `format:check`, `lint` (clippy over
every target), and `test` (the full suite, no-fail-fast).

`test` needs **cargo-nextest** on your `PATH`. Install a prebuilt binary
rather than building it from source:

```sh
curl -LsSf https://get.nexte.st/latest/mac | tar zxf - -C "${CARGO_HOME:-$HOME/.cargo}/bin"
```

Substitute `linux` for `mac` as appropriate; see
[nexte.st/docs/installation](https://nexte.st/docs/installation/pre-built-binaries/).
CI installs it with `taiki-e/install-action`.

### Always prefix with `env -u RUSTUP_TOOLCHAIN`

There is no `rust-toolchain.toml` in this repository — it was deliberately
removed when the storage engine moved off a nightly-only dependency, so
whatever `rustup` resolves from your shell is what builds. If your shell has
a stale `RUSTUP_TOOLCHAIN` exported (e.g. left over from before that
removal), an unprefixed `cargo test` or `task check` silently runs on
nightly and still reports success — it does not fail loudly, it just proves
the wrong thing. Run `echo $RUSTUP_TOOLCHAIN` if you're unsure, and prefix
every `cargo`/`rustc`/`task` invocation with `env -u RUSTUP_TOOLCHAIN` to be
safe regardless.

`task toolchain:check` exists specifically to catch this: it reads `rustc -vV`
and fails if the active release is nightly, beta, or dev. It is the first
step of `task check` for that reason.

## The runner, and what replaced the abort gate

`task test` runs the suite under **cargo-nextest**, then runs the doctests
separately. Both halves matter, and the second is not optional — see below.

```sh
cargo nextest run --workspace --no-fail-fast
cargo test --workspace --doc
```

Nextest runs **each test in its own process**. That is worth stating plainly,
because it is not merely a speed choice — it changes what can go wrong.

**What it fixed.** This file used to document an "abort gate": each test binary
printed a `running N tests` line and exactly one matching `test result:` line,
and a binary that died partway (a panic past the harness, an abort-on-panic
profile, a segfault) exited before printing its result line, leaving every
later test in it silently uncounted. The run still read as plausible — some
passed, 0 failed — while being short several tests. Counting the two kinds of
line and checking they agreed was the only defence.

Under nextest that failure mode does not exist. A test that segfaults kills
only its own process, and nextest reports it by name as `SIGSEGV`. One test
cannot take its neighbours' results down with it, so there is nothing left to
reconcile: read the exit status and the single `Summary` line.

```
Summary [  91.399s] 3162 tests run: 3162 passed (1 leaky), 4 skipped
```

`task test:report` parses exactly that line and writes any `FAIL`, `SIGSEGV`,
`SIGABRT`, `TIMEOUT` or `LEAK` names to a file. It is a diagnostic, never a
gate.

**What it broke, and what guards that now.** Process-per-test voids any
synchronisation that only holds within a process.
`test_support::workspace_replay_test_guard()` is a `static Mutex` that six
tests take before materialising a workspace; under nextest each of those six
is its own process, each gets its own copy of the mutex, and all six lock an
uncontended lock and run in parallel. Nothing fails to compile and nothing
warns — the guard simply stops guarding. `.config/nextest.toml` restores the
serialisation with a `max-threads = 1` test group, which nextest enforces
across processes. **If you add a caller of that function, add it to the filter
in that file.**

**The new silent-skip risk is doctests.** Nextest cannot run them — rustdoc
owns that path — so they are a second command. Drop it and the doctests stop
running with nothing to show for it, which is the same class of failure the
old abort gate existed to catch, relocated. `task test` runs both; do not
replace it with a bare `cargo nextest run`.

**`task test:linux` deliberately still uses `cargo test`.** It exists to vary
the OS, not the runner, and installing another tool into a throwaway container
serves nothing. It is therefore the one path where the old reasoning still
applies, and where this check is still worth running:

```sh
awk '/^running [0-9]+ tests?$/ {r++} /^test result:/ {t++} \
     END {print "blocks="r" results="t" balanced="(r>0 && r==t?"YES":"NO")}' /tmp/run.log
```

The `r>0` is load-bearing: without it a run that never started — a clippy or
compile failure produces neither kind of line — compares 0 to 0 and reports
`balanced=YES`. That is the one failure the check must never call clean, and
it did call it clean until the guard was added. Redirect to a file rather than
piping through `tail`, too: a pipeline reports the *last* command's status, so
`cargo test | tail` reports `tail`'s success and hides a failing run.
(`set: [pipefail]` covers this inside the Taskfile; an ad-hoc shell command is
on its own.)

A full run at the time of writing: **3162 tests, 0 failed, 4 skipped**, in 91s
under nextest against ~174s of `cargo test` execution, plus 10s of doctests.

**The test total was once 14166, and almost none of that drop was coverage.**
Two separate things happened, and it is worth keeping them apart.

The first was an illusion. The shared harness lived in `tests/common/` and was
pulled in with `mod common;` by 71 integration binaries, so its 154 tests were
compiled and executed 71 times — 10934 of the 12385 reported integration tests
were the same 154 tests over and over. That harness is now the `test-support`
crate: compiled once, linked as a library, its own tests run once. Nothing was
deleted, and the number fell to 3232.

The second was deliberate. Benchmarks and stress tests (`bench_*`,
`e2e_cold_warm_benchmarks`), a harness self-demo, and the snapshots pinning
*human-readable* CLI text and error wording were removed — they asserted
cosmetics that change whenever the prose does, and this project is not tuned
for performance against a baseline. The machine-readable contracts that
consumers actually parse were kept: `json_output`, `jsonl_format`,
`robot_output`, `history_diff_output`, and `golden_beads_init`.

Do not reconcile a change in the total test count by arithmetic (e.g. "I
added 3 tests so the count should go up by 3"). Use `cargo nextest list
--workspace` before and after your change and diff the actual test names. The
multiplier that used to make this advice urgent is gone — a test added to the
shared harness is now compiled once, not once per binary — but the habit is
still the right one, because a test added to any module included by more than
one target still counts more than once.

## Where a new test goes

Tests are grouped into a handful of binaries rather than one binary per file.
Each group is a crate root listing `#[path]` modules from a directory beside
it:

```
tests/e2e.rs        tests/e2e/*.rs         drive `br` as a subprocess
tests/storage.rs    tests/storage/*.rs     SQLite, the JSONL mirror, and sync
tests/proptest.rs   tests/proptest/*.rs    invariants over generated input
tests/repro.rs      tests/repro/*.rs       one module per bug that happened
tests/output_contracts.rs                  goldens consumers parse
```

**Add a file to the directory and a `#[path]` line to the crate root** — do not
add a new top-level `tests/*.rs` unless the test genuinely belongs to no group.
Each binary links the whole dependency graph, bundled C SQLite included, and
that link is the dominant cost: collapsing 90 binaries into these 15 cut the
test build from ~49s to ~19s while compiling exactly the same code.

Three details follow from the layout:

- `#[path]` is required. A crate root resolves `mod defer;` to `tests/defer.rs`,
  so without it the module would claim a sibling binary's name.
- `extern crate test_support as common;` is legal only at a crate root. In a
  module write `use crate::common;`, which rebinds the same name so every
  `common::` path keeps working.
- **insta snapshots live beside the file that asserts them and are named after
  the module path.** Moving a test between files therefore moves and renames
  its `.snap`. Nothing warns you — the test just fails with a fresh
  `.snap.new`, which is exactly how `storage/golden_snapshot.rs` announced it
  during this regrouping.
- **`.proptest-regressions` files are keyed to the source path too, and they
  fail *silently*.** proptest's default `SourceParallel` mode cannot find a
  `lib.rs` or `main.rs` from an integration test, so it falls back to writing
  `<source>.proptest-regressions` beside the file. Move `proptest/foo.rs` and
  the old file is simply never opened again: every pinned failing seed stops
  being replayed, the suite still passes, and nothing is printed. Unlike the
  snapshot case there is no failing test to tell you. Verify a move with a
  deliberately malformed line in the file and
  `--success-output immediate` — proptest reports `unparsable line, ignoring`
  with the path it actually read.

## Toolchain floor

`rust-version = "1.95"` in `Cargo.toml` is measured (by bisection against
this crate's dependency graph), not inherited from upstream. It is a
property of the pinned `Cargo.lock`, not of this crate's own source, and can
move if a dependency's requirements change. Building needs a C compiler in
addition to Rust: `rusqlite`'s `bundled` feature compiles SQLite from C
source.

## No plain-MIT prose, ever

`tests/licensing.rs` enumerates every git-tracked file and fails the build
if any file other than `NOTICE.md` and `tests/licensing.rs` itself contains
any of a short list of substrings that name an upstream project or a path on
someone else's machine (see the `DISALLOWED_PATTERNS` constant in that file
for the exact list — deliberately not repeated here, since repeating it
would itself trip the guard). This is deliberate: lineage and the licensing
rider live in `NOTICE.md` only. If you're tempted to write a sentence
describing where this project came from anywhere else — a doc comment, a
new README section, a commit message that ends up quoted in a tracked file
— link to `NOTICE.md` instead of naming names. After staging any new or
edited tracked file, run:

```sh
env -u RUSTUP_TOOLCHAIN cargo test --test licensing
```

Separately, do not describe this project as plain MIT-licensed anywhere.
`Cargo.toml` uses `license-file = "LICENSE"` rather than an SPDX `license`
string because the license carries a binding OpenAI/Anthropic rider with no
SPDX identifier; `tests/licensing.rs` pins both of those facts.

## Useful commands

- `env -u RUSTUP_TOOLCHAIN task build` — build the `br` binary and every test target.
- `env -u RUSTUP_TOOLCHAIN task test` — run the full suite under nextest, then the doctests. `--workspace` reaches `test-support`'s own tests.
- `env -u RUSTUP_TOOLCHAIN task test:lib` — unit tests only, skips the slower integration binaries.
- `env -u RUSTUP_TOOLCHAIN task test:one -- -E 'test(<name>)'` — run a subset. Nextest selects by [filterset](https://nexte.st/docs/filtersets/) rather than `--test <binary> <name>`; `-E 'test(defer::)'` picks one module and `-E 'binary(e2e)'` a whole binary.
- `env -u RUSTUP_TOOLCHAIN task format` / `task format:check` — apply or check `cargo fmt`.
- `env -u RUSTUP_TOOLCHAIN task lint` — clippy over every target (pedantic + nursery, see `[lints.clippy]` in `Cargo.toml`).
- `env -u RUSTUP_TOOLCHAIN task test:report` — run the suite and print `passed=N failed=N` plus failing test names; a diagnostic, not a gate.
- `env -u RUSTUP_TOOLCHAIN task test:linux` — the same suite in a `linux/arm64` container, catching macOS-only assumptions (notably `/tmp` symlink resolution). Needs Docker; not run by `task check`.

## Releasing

A release is cut by pushing a `v*` tag; `.github/workflows/release.yml` builds
four binaries with GoReleaser's `builder: rust` and publishes one GitHub
release. The full sequence — bump `Cargo.toml`, generate the CHANGELOG
section, verify the tag, tag, push — is in
[`docs/RELEASING.md`](docs/RELEASING.md). Read it before touching
`.goreleaser.yaml`, `.cliff.toml`, or anything under `.github/scripts/`.

Two things that bite here and nowhere else:

- **`Cargo.toml`'s version and the tag are separate facts.** Cargo compiles
  the manifest version into the binary as `CARGO_PKG_VERSION`; GoReleaser
  names the archives after the tag. `.github/scripts/check-release-version.sh`
  is the guard, and `TAG=vX.Y.Z task release:verify` runs it before the tag
  exists.
- **`CHANGELOG.md` is a tracked file built out of commit subjects**, so it is
  swept by `tests/licensing.rs` like everything else. Run
  `cargo test --test licensing` after `task changelog`, not just after editing
  source.

Do not skip hooks and do not add `--no-verify`.

## Never write to the remote without explicit approval

This repository **has** a remote: `github.com/Toshik1978/beads`. That does not
make it yours to write to.

**Ask, every time, and wait for a yes** before anything that changes state on
GitHub:

- `git push` in any form, to any branch, including `--force-with-lease`
- `gh pr merge` / `close` / `create` / `edit` / `comment` / `review`
- `gh release`, `gh workflow run`, `gh api` with any non-GET method
- pushing a tag, which is what starts a release (see `docs/RELEASING.md`)

**Reading is fine and needs no approval** — `gh pr list`, `gh pr diff`,
`gh run list`, `gh run view --log-failed`, `gh api` GETs. Diagnose freely;
just do not act on the result.

Approval is per action, not per session, and a request to fix or unblock
something is not approval to publish it. Local work — committing, branching,
resetting, rewriting history — is outside this rule.
