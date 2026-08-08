#!/usr/bin/env bash
#
# Report items in `src/` that nothing reachable from the `br` binary uses.
#
# Why this exists: `cargo check` sees none of them. `src/lib.rs` declares every
# module `pub`, and `dead_code` only fires on items rustc can prove unreachable
# from *outside* the crate too — a `pub fn` in a `pub mod` of a lib target is
# reachable by hypothesis, so the lint correctly declines to fire and stays
# silent forever. The hypothesis is false here: `Cargo.toml` sets
# `publish = false` and the only consumers are the `br` binary in the same
# package and this repository's own tests.
#
# So this makes the hypothesis true for the length of two compiles. It rewrites
# a scratch copy of the tree so the library's modules are `pub(crate)` and the
# binary is compiled *into* the library as a module, giving rustc a real root
# set, then reads the `dead_code` diagnostics back out. Because that is
# reachability analysis rather than identifier counting, it has none of the
# blind spots an ad-hoc grep sweep has: a wrapper that forwards to an
# identically-named inner function inflates both names' occurrence counts and
# hides the pair, while rustc reports the whole cluster. `--self-check`
# demonstrates exactly that property on a synthetic pair.
#
# ADVISORY, NOT A GATE. It is deliberately not wired into `task check`, for the
# same reason `task test:report` is not: what it measures is real, and what to
# do about each hit is a judgement. A large and legitimate share of "unreachable
# from main" is code reached only from `tests/` — read-back accessors whose
# whole job is to check that a write was correct. Deleting those deletes the
# check. So this prints a classified list for a human and exits 0 unless the
# machinery itself failed.
#
# The three buckets, narrowest first:
#
#   referenced nowhere   Not from main, not from a unit test, not from tests/.
#                        This is the actionable bucket.
#   unit-test-only       Reached only from a `#[cfg(test)]` block inside src/.
#                        Dead product code with passing tests attached; deleting
#                        it means deleting tests that currently pass, so each
#                        one wants a written reason.
#   test-facing          Reached from tests/ or test-support/. Usually correct
#                        as it stands and usually not to be swept.
#
# The first two are exact — they come from rustc's own reachability, run once
# with only `main` as a root and once with the unit tests added. The third is a
# word-boundary grep over `tests/` and `test-support/`, because those targets
# cannot link a `pub(crate)` library at all and so cannot take part in the
# instrumented compile. That grep can only over-report a reference, never miss
# one, so an item in "referenced nowhere" is genuinely unreferenced.
#
# The rewrite happens in a scratch copy under `target/`; `src/` is never
# touched, so an interrupted run cannot leave the tree modified.
#
# Scope: `src/` only. `test-support` is a separate lib crate and is blind to
# `dead_code` for exactly the same structural reason, but it has no equivalent
# of `main` to act as a root — its callers are the integration binaries, which
# cannot take part in this compile — so nothing here would be meaningful about
# it.
#
# Usage:
#   scripts/dead_code_report.sh              classified report
#   scripts/dead_code_report.sh --quiet      one line: the three bucket counts
#   scripts/dead_code_report.sh --self-check prove the wrapper-pair case is seen
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$repo_root/target/dead-code-report"
mode="${1:-}"

case "$mode" in
  "" | --quiet | --self-check) ;;
  *)
    echo "usage: $(basename "$0") [--quiet|--self-check]" >&2
    exit 2
    ;;
esac

rm -rf "$work"
mkdir -p "$work"

# Two identical scratch trees rather than one. The second run adds the lib's
# own `#[cfg(test)]` blocks as roots, and cargo would otherwise replay the
# first run's cached diagnostics alongside the second's, making the two sets
# identical by construction and the difference between them always empty.
# Separate package directories get separate fingerprints; a shared
# CARGO_TARGET_DIR still means the dependency graph is checked only once.

# Only what the lib target needs to compile. `tests/` and `test-support/` are
# deliberately excluded: neither can link a `pub(crate)` library, and including
# them would defeat the purpose anyway by making every test-only helper a root.
for tree in "$work/a" "$work/b"; do
  mkdir -p "$tree"
  cp "$repo_root/Cargo.toml" "$repo_root/Cargo.lock" "$tree/"
  cp -R "$repo_root/src" "$tree/src"
  # `src/cli` reaches these with `include_str!` from inside `#[cfg(test)]`,
  # which the second run compiles.
  cp -R "$repo_root/docs" "$tree/docs"
  cp -R "$repo_root/skills" "$tree/skills"

  python3 - "$tree" "$mode" <<'PY'
import re
import sys
from pathlib import Path

work = Path(sys.argv[1])
self_check = sys.argv[2] == "--self-check"

lib = work / "src" / "lib.rs"
text = lib.read_text()

# Every `pub mod` becomes `pub(crate) mod`, which is what they already are in
# practice, so rustc stops treating "some downstream crate might call it" as a
# reason to keep an item alive.
text, mods = re.subn(r"^pub mod ", "pub(crate) mod ", text, flags=re.MULTILINE)
if mods == 0:
    sys.exit("dead_code_report: no `pub mod` lines found in src/lib.rs")

# A `pub use` of a now-private module is E0365, an error rather than a warning.
text = re.sub(r"^pub use ", "pub(crate) use ", text, flags=re.MULTILINE)

# Compile the binary as part of the library so `main` can act as the root.
# Without a root, rustc reports the entire crate dead (~1700 warnings) rather
# than the real answer.
text += (
    "\n// Injected by scripts/dead_code_report.sh — see that file.\n"
    "extern crate self as beads;\n"
    '#[path = "main.rs"]\n'
    "pub mod bin_root;\n"
)

if self_check:
    # The blind spot this tool exists to cover: an occurrence-counting sweep
    # sees `probe` twice defined and twice referenced and calls the pair live,
    # because the wrapper's call to the inner function inflates both counts.
    # Reachability sees that nothing calls the wrapper, so the whole cluster is
    # dead. Both lines must be reported.
    text += (
        "\npub(crate) mod wrapper_pair_probe {\n"
        "    pub fn probe() -> usize { inner::probe() }\n"
        "    mod inner {\n"
        "        pub fn probe() -> usize { 0 }\n"
        "    }\n"
        "}\n"
    )

lib.write_text(text)

main = work / "src" / "main.rs"
main_text = main.read_text()
# `fn main` is private, so as a module member it would not be a root either.
main_text, count = re.subn(
    r"^fn main\(\)", "pub fn main()", main_text, flags=re.MULTILINE
)
if count != 1:
    sys.exit(f"dead_code_report: expected one `fn main()` in src/main.rs, found {count}")

# As a module of the library, `main.rs` reaches its own crate. `beads::x` still
# resolves through `extern crate self as beads`, but only for `pub` items —
# every module is `pub(crate)` now, so the binary's own `#[cfg(test)]` blocks
# (compiled in the second run, not the first) would fail with E0603.
main_text = main_text.replace("beads::", "crate::")

# One `#[global_allocator]` per crate. The library is the crate now, and the
# stub binary linking it would be a second one.
main_text = re.sub(
    r"#\[cfg\(not\(windows\)\)\]\n#\[global_allocator\]\nstatic [^;]+;\n",
    "",
    main_text,
)
main.write_text(main_text)

# `src/main.rs` is now a library module. Leaving it as a binary target too
# would compile it a second time as its own crate root, where every
# `crate::`-qualified path in it fails to resolve.
manifest = work / "Cargo.toml"
manifest_text = manifest.read_text()
# `test = false` keeps the second run (`cargo check --tests`) from compiling
# the plain lib target at all, so its diagnostics come only from the
# unit-test compilation instead of being mixed with a replay of the first
# run's.
manifest_text, bins = re.subn(
    r'\[\[bin\]\]\nname = "br"\npath = "src/main.rs"\n',
    '[[bin]]\nname = "br"\npath = "src/bin_stub.rs"\ntest = false\n',
    manifest_text,
)
if bins != 1:
    sys.exit("dead_code_report: could not find the `br` [[bin]] target in Cargo.toml")
# Redirecting the declared target is not enough: autodiscovery would pick
# `src/main.rs` up again as a second, package-named binary.
manifest_text = manifest_text.replace(
    "[package]\n", "[package]\nautobins = false\n", 1
)
# `test-support` is not copied, so neither the workspace member nor the
# dev-dependency on it can resolve. Nothing under `src/` uses it — the lib's
# own `#[cfg(test)]` blocks are self-contained — so dropping both is enough to
# let the second run compile the unit tests.
manifest_text = manifest_text.replace('members = ["test-support"]', "members = []")
manifest_text = re.sub(
    r'^test-support = \{ path = "test-support" \}\n', "", manifest_text, flags=re.MULTILINE
)
manifest.write_text(manifest_text)
(work / "src" / "bin_stub.rs").write_text("fn main() {}\n")
PY
done

export CARGO_TARGET_DIR="$work/target"

# Run A: only `main` is a root.
# Run B: the lib's own `#[cfg(test)]` blocks are roots too. What run A reports
# and run B does not is reached from a unit test and nothing else, exactly,
# with no brace-counting guesswork.
(cd "$work/a" && cargo check --lib --message-format=json 2>/dev/null || true) >"$work/main.json"
(cd "$work/b" && cargo check --tests --message-format=json 2>/dev/null || true) >"$work/tests.json"

for stream in main tests; do
  if grep -q '"level":"error"' "$work/$stream.json"; then
    echo "dead_code_report: the instrumented build failed; the report would be meaningless." >&2
    python3 -c '
import json, sys
for line in open(sys.argv[1], encoding="utf-8"):
    try:
        record = json.loads(line)
    except ValueError:
        continue
    if record.get("reason") == "compiler-message" and record["message"].get("level") == "error":
        print(record["message"]["rendered"], file=sys.stderr)
' "$work/$stream.json" >&2
    exit 1
  fi
done

python3 - "$work" "$repo_root" "$mode" <<'PY'
import json
import re
import subprocess
import sys
from pathlib import Path

work, repo_root, mode = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]


def dead_items(stream):
    """(file, line, source text) for every primary dead_code span."""
    found = set()
    for line in (work / stream).read_text(encoding="utf-8").splitlines():
        try:
            record = json.loads(line)
        except ValueError:
            continue
        if record.get("reason") != "compiler-message":
            continue
        diagnostic = record["message"]
        if (diagnostic.get("code") or {}).get("code") != "dead_code":
            continue
        for span in diagnostic["spans"]:
            # The non-primary span is the enclosing impl or enum, not an item.
            if not span.get("is_primary"):
                continue
            text = (span.get("text") or [{}])[0].get("text", "").strip()
            found.add((span["file_name"], span["line_start"], text))
    return found


from_main = dead_items("main.json")
from_tests = dead_items("tests.json")

IDENT = re.compile(r"\b(?:fn|struct|enum|trait|type|const|static)\s+([A-Za-z_][A-Za-z0-9_]*)")


def item_name(text):
    match = IDENT.search(text)
    if match:
        return match.group(1)
    # Enum variants and tuple-struct fields have no keyword.
    bare = re.match(r"([A-Za-z_][A-Za-z0-9_]*)", text)
    return bare.group(1) if bare else None


def referenced_by_integration_tests(name):
    if name is None:
        return True  # unknown shape: assume referenced rather than accuse it
    targets = [str(repo_root / "tests"), str(repo_root / "test-support" / "src")]
    result = subprocess.run(
        ["grep", "-rqw", "--", name, *targets],
        capture_output=True,
        check=False,
    )
    return result.returncode == 0


nowhere, unit_only, test_facing = [], [], []
for item in sorted(from_main):
    if referenced_by_integration_tests(item_name(item[2])):
        test_facing.append(item)
    elif item not in from_tests:
        # Run B found it live, so a `#[cfg(test)]` block inside src/ reaches it.
        unit_only.append(item)
    else:
        nowhere.append(item)

if mode == "--quiet":
    print(
        f"referenced_nowhere={len(nowhere)} "
        f"unit_test_only={len(unit_only)} "
        f"test_facing={len(test_facing)}"
    )
    sys.exit(0)

if mode == "--self-check":
    probes = [item for item in nowhere + unit_only + test_facing if "probe" in item[2]]
    for item in probes:
        print(f"  {item[0]}:{item[1]}: {item[2]}")
    if len(probes) == 2:
        print(
            "\nself-check passed: both halves of the wrapper pair are reported.\n"
            "An occurrence-counting sweep sees each `probe` referenced once and\n"
            "calls the pair live; reachability sees the cluster."
        )
        sys.exit(0)
    print(f"\nself-check FAILED: expected 2 probe items, got {len(probes)}")
    sys.exit(1)


def section(title, items, note):
    print(f"== {title} ({len(items)}) ==")
    print(f"   {note}")
    for file_name, line_no, text in items:
        print(f"   {file_name}:{line_no}: {text}")
    print()


section(
    "referenced nowhere",
    nowhere,
    "Actionable: not reached from main, from a unit test, or from tests/.",
)
section(
    "unit-test-only",
    unit_only,
    "Reached only from a #[cfg(test)] block inside src/. Each wants a reason.",
)
section(
    "test-facing",
    test_facing,
    "Reached from tests/ or test-support/. Usually correct as it stands.",
)
print("Advisory only. See the header of scripts/dead_code_report.sh.")
PY
