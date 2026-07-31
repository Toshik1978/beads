# Testing

This document is about test *mechanics* — how to run the suite, how to trust
the result, and the specific traps that have already cost time in this
repository. For the gate itself (`task check`), the `RUSTUP_TOOLCHAIN` trap,
and the toolchain floor, see [`../CLAUDE.md`](../CLAUDE.md) — this document
goes deeper rather than repeating it.

Everything below was measured against this tree, not assumed:
`env -u RUSTUP_TOOLCHAIN cargo test --no-fail-fast` currently produces **113**
test binaries, **14560 passed**, **0 failed**, **32 ignored**.

## The abort gate, and how to actually check it

Each test binary prints `running N tests`, runs them, then prints exactly one
matching `test result: ... N passed; N failed; ...` line. If a binary aborts
partway through — a panic that unwinds past the harness, an `abort`-on-panic
setting, a segfault — the process can exit before printing its `test result:`
line. Every test after the crash in that binary is then silently uncounted,
and the run can still *look* healthy (some passed, 0 failed) while actually
being short several tests. A bare "tests passed" summary or exit code cannot
catch this; only comparing the two counts can.

The check: the number of `running N tests` blocks must equal the number of
`test result:` lines, and — in this tree, today — both must equal **113**.

```sh
env -u RUSTUP_TOOLCHAIN cargo test --no-fail-fast > /tmp/beads-test.log 2>&1
grep -c '^running [0-9]* test' /tmp/beads-test.log   # -> 113
grep -c '^test result:' /tmp/beads-test.log            # -> 113
```

Two things about that grep pattern that are not obvious and have already
caused a miscount:

- **`test` is singular when N is 1.** Rust's harness prints `running 1 test`
  (no trailing `s`), not `running 1 tests`. A pattern anchored on the plural
  form (`running [0-9]* tests`) silently skips every binary whose run happens
  to have exactly one test selected, which under-counts without erroring.
  `grep -c '^running [0-9]* test'` (no trailing `s` in the pattern) matches
  both forms.
- **113 includes the doctest binary.** `cargo test` builds one binary per
  integration test file under `tests/` plus one `unittests` binary for
  `src/`, and separately runs everything under `Doc-tests beads` as one more
  `running N tests` / `test result:` pair at the very end of the log. `cargo
  test --no-run` only lists the compiled `Executable` targets (112 in this
  tree) and will not include that doctest pass — don't use its count as the
  abort-gate total.

Do not trust `task test:report`'s `passed=N failed=N` line as a substitute for
this check either — it sums the `test result:` lines the same way, so a
binary that aborted before printing one is invisible to it too. It is a
diagnostic for *which* tests failed, not a replacement for the abort gate.

## The `mod common;` multiplier

Most integration test binaries under `tests/` compile a shared helper module
with `mod common;`. Every test function defined inside `tests/common/` (or
whatever that binary's `common` module re-exports) is therefore compiled into
— and counted once per — every binary that includes it, not compiled once
total. Adding one test to `common` does not add one test to the suite total;
it adds one *per binary that pulls it in*, which in this tree is most of
them.

The consequence: **never reconcile a change in the total test count by
arithmetic** ("I added 3 tests so the total should go up by 3"). Instead, list
test names before and after and diff them by name:

```sh
env -u RUSTUP_TOOLCHAIN cargo test -- --list > /tmp/before.txt   # before your change
# ... make the change ...
env -u RUSTUP_TOOLCHAIN cargo test -- --list > /tmp/after.txt
diff /tmp/before.txt /tmp/after.txt
```

This is also the only reliable way to confirm a test you *removed* didn't
also silently remove others that depended on `common` machinery it touched.

## Failing-set comparisons must cover the same binaries

When comparing a "before" and "after" failing-test list (for example, while
driving a regression to zero), both runs must invoke the same set of test
binaries. A narrower "after" run — a single `--test` target, or a run that
skips binaries the "before" run included — will show fewer failures than the
baseline simply because it looked at less, not because anything was fixed.
That manufactures phantom progress. Use the same command (ideally bare `task
test` / `cargo test --no-fail-fast` with no `--test` filter) on both sides of
any before/after comparison, and reconcile by test name (per the `--list`
diff above), never by count.

## `RUSTUP_TOOLCHAIN`

There is no `rust-toolchain.toml` in this repository (see
[`../CLAUDE.md`](../CLAUDE.md) for why it was removed and what replaced it).
If your shell has a stale `RUSTUP_TOOLCHAIN` exported, an unprefixed `cargo
test` or `task check` silently runs the entire suite on that toolchain instead
of your default stable one, and still reports success — it proves the wrong
thing without failing loudly. `task toolchain:check` (the first step of `task
check`) catches this by refusing any pre-release (`nightly`/`beta`/`dev`)
release channel. The local workaround, and the one to use for every command in
this document, is prefixing with `env -u RUSTUP_TOOLCHAIN`.

## The task surface

`Taskfile.yml` is the authority here — several of its tasks carry comment
blocks recording *why* they're built the way they are, which this section
summarizes but does not replace. Current tasks, from `env -u RUSTUP_TOOLCHAIN
task --list`:

| Task | What it does |
|---|---|
| `task test` | The full suite: bare `cargo test --no-fail-fast`. This is the one `task check` runs, and the one whose exit code is meaningful — use it for any before/after comparison. |
| `task test:lib` | Unit tests only (`cargo test --lib`), skipping the integration binaries. Fast, but does not exercise the abort gate's 113-binary total. |
| `task test:one` | Run a subset, e.g. `task test:one -- --test e2e_defer defer_until_invalid_error`. |
| `task test:hermetic` | The full suite under `TZ=UTC LANG=C`, to catch timezone/locale-dependent test assumptions. |
| `task test:report` | Runs the suite and prints `passed=N failed=N` plus the sorted failing test names to `/tmp/beads-failing.txt`. A **diagnostic**, never a gate — `task check` does not call it, and (as above) it cannot detect an aborted binary on its own. |
| `task test:linux` | The full suite inside a `linux/arm64` Docker container, proving the suite is not macOS-specific — notably `/tmp`'s symlink-to-`/private/tmp` resolution, which caused a real batch of macOS-only failures earlier in this project's history. Needs Docker; not run by `task check`. |
| `task build:cross` | Builds every release target through `cargo-zigbuild` in a container and proves the bundled C SQLite links for each — several also get a real `br init`/`create`/`list` round-trip, not just a successful link. Not run by `task check`; run it when the release pipeline or the storage engine changes. |

`task toolchain:check`, `task format`/`task format:check`, and `task lint`
round out `task check` and are covered in `CLAUDE.md`.

### `task test:linux` proves OS divergence, not architecture divergence

Both the development host and the `test:linux` container are aarch64 (Apple
silicon runs the container natively at that architecture); GitHub's own
runners are x86_64. So this task proves the suite is not macOS-specific — it
already caught one real batch of macOS-`/tmp`-symlink-shaped failures — but it
proves nothing about x86_64-specific behavior (byte ordering, floating-point
rounding, and similar). That gap is tracked as `bds-028` and is **not**
closed by this task; do not describe `task test:linux` as full Linux parity.
`task build:cross` is a separate, narrower check: it proves the release
*targets* build and link, including real x86_64/aarch64 round-trips under
emulation or native execution, but it is not a full test-suite run on those
targets and does not substitute for closing `bds-028`.

`task test:linux:amd64` exists to move that architecture axis, and it does
produce a genuinely x86_64 container — but **it cannot complete on an Apple
silicon host without Rosetta.** With Rosetta off, OrbStack falls back to QEMU,
and QEMU segfaults inside itself while linking the first debuginfo-heavy test
binary (`ld terminated with signal 11`), after building the dependency tree
successfully. That is an emulator limitation and tells you nothing about the
suite, so it is not evidence in either direction.

Native x86_64 coverage therefore comes from CI, which is what `bds-028` always
called the better route: `.github/workflows/ci.yml` runs the full suite on
`ubuntu-latest` — unemulated, where a failure would be unambiguous. Until that
has actually run, **no x86_64 evidence for this suite exists at all**, and
`bds-028` stays open.

## The licensing guard

`tests/licensing.rs` enumerates every git-tracked file and fails the build if
any file other than `NOTICE.md` and `tests/licensing.rs` itself contains any
of a short list of substrings — the upstream project names and machine-local
paths listed in that file's `DISALLOWED_PATTERNS` constant. This is
deliberately not repeated here: quoting the forbidden strings while
explaining the rule is exactly how this guard has been tripped before (it
happened while writing `CLAUDE.md`). If you add or edit a tracked file — this
one included — run it after staging:

```sh
env -u RUSTUP_TOOLCHAIN cargo test --test licensing
```
