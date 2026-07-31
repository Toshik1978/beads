#!/usr/bin/env python3
"""Rewrite slug-shaped issue IDs in a beads workspace back to `<prefix>-<hash>`.

THIS IS AN OPERATOR TOOL, NOT PART OF `br`.

Nothing here is compiled into the `br` binary, invoked by it, or exercised by
`task check`. It exists because `br` is dropping every notion of a slug --
both writing one and recognising one on import -- and a workspace whose
`issues.jsonl` still contains slug-shaped IDs cannot be imported afterwards.
The script can know what a slug is precisely because it is not part of the
product. Run it by hand, once, against each affected workspace, BEFORE
upgrading to a `br` that has dropped slug support.

It operates on `.beads/issues.jsonl` directly rather than through `br`,
because the `br` that needs this fix is exactly the one that will refuse to
read the file.

What it does
------------
A slug-shaped root ID carries something between the configured prefix and the
trailing hash: `em-split-transaction-viewmodel-ih4`. The replacement strips
the slug and keeps the existing hash: `em-ih4`. The hash is already the
uniquifier, so stripping is deterministic, reversible and auditable --
regenerating the ID through `br`'s generator would produce a value with no
relationship to the original, and is deliberately not what happens here.

Known limits -- read these before running
-----------------------------------------
* **Git history is not rewritten.** Commit messages that name an old ID keep
  naming it. That is immutable history, and it is fine, but it means a
  `git log --grep` for a rewritten ID will come up empty afterwards.
* **Source code is not scanned.** A bead ID can end up in a source comment or
  a lint suppression. This script prints every old ID it replaced so you can
  grep your own tree; it will not edit source itself.
* **The detector reads the `id` field only**, and cannot distinguish a slug
  from a configured prefix that itself contains a hyphen. If your prefix has
  a hyphen in it, do not use this script.

Usage
-----
    # Dry run (the default) -- reports, touches nothing:
    python3 scripts/deslug_workspace.py path/to/.beads

    # Apply, after taking a backup of issues.jsonl:
    python3 scripts/deslug_workspace.py path/to/.beads --write

    # When config.yaml does not carry an uncommented `issue_prefix:` (the
    # common case -- `br init` writes it commented out and keeps the real
    # value in the database), supply it explicitly:
    python3 scripts/deslug_workspace.py path/to/.beads --prefix em --write
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
import time
from pathlib import Path

# `br init` writes `issue_prefix` into config.yaml as a comment and keeps the
# authoritative value in the database's config table, so an uncommented match
# is the exception rather than the rule. Only an uncommented line counts; a
# commented one is a template, not configuration.
PREFIX_LINE = re.compile(r"^\s*issue_prefix\s*:\s*(?P<value>[^#\s]+)")


def fail(message: str) -> "NoReturn":  # noqa: F821 - quoted for 3.8 compat
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(2)


def read_prefix(beads_dir: Path) -> "str | None":
    config = beads_dir / "config.yaml"
    if not config.is_file():
        return None
    for line in config.read_text(encoding="utf-8").splitlines():
        match = PREFIX_LINE.match(line)
        if match:
            return match.group("value").strip().strip("\"'")
    return None


def load_jsonl(path: Path) -> "list[dict]":
    records = []
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError as err:
            fail(f"{path}:{lineno}: not valid JSON: {err}")
        if not isinstance(record, dict):
            fail(f"{path}:{lineno}: expected a JSON object, got {type(record).__name__}")
        records.append(record)
    return records


def dump_jsonl(records: "list[dict]") -> str:
    # Matches `br`'s own export byte-for-byte: compact separators, no ASCII
    # escaping, one object per line, trailing newline. Verified by
    # round-tripping an untouched export through this function.
    lines = [json.dumps(r, separators=(",", ":"), ensure_ascii=False) for r in records]
    return "".join(line + "\n" for line in lines)


def is_slug_shaped(issue_id: str, prefix: str) -> bool:
    """True for a root ID carrying anything between the prefix and the hash.

    Root only: an ID with a `.` is a hierarchical child and inherits whatever
    shape its root has, so it is never independently slugged.
    """
    if "." in issue_id or not issue_id.startswith(prefix + "-"):
        return False
    return "-" in issue_id[len(prefix) + 1 :]


def deslug(issue_id: str, prefix: str) -> str:
    """`em-some-long-slug-ih4` -> `em-ih4`. Keep the hash, drop the slug."""
    return f"{prefix}-{issue_id.rsplit('-', 1)[1]}"


def rewrite_strings(value, replacements: "list[tuple[str, str]]"):
    """Substring-replace every old root ID inside every string in the record.

    Two things are deliberate here and look like accidents otherwise.

    First, this is a *substring* replacement and it is applied to the root ID
    only. Hierarchical children embed the root verbatim (`<root>.1.2`), so
    replacing the root substring rewrites the entire descendant tree in one
    pass. Do not "fix" this into a per-ID loop over every ID in the file --
    that reintroduces the possibility of missing a descendant.

    Second, it walks every string in the record rather than a list of known
    fields. Dependency rows (`issue_id`, `depends_on_id`) matter for
    correctness, but IDs also appear as free text in `description`, `design`,
    `acceptance_criteria`, `notes`, comment bodies and titles. A field list
    goes stale the moment the schema grows a field; a walk does not, and a
    half-migration that leaves stale IDs in prose is invisible.
    """
    if isinstance(value, str):
        for old, new in replacements:
            value = value.replace(old, new)
        return value
    if isinstance(value, list):
        return [rewrite_strings(item, replacements) for item in value]
    if isinstance(value, dict):
        return {key: rewrite_strings(item, replacements) for key, item in value.items()}
    return value


def count_edges(records: "list[dict]") -> int:
    return sum(len(r.get("dependencies") or []) for r in records)


def dangling_edges(records: "list[dict]") -> "list[str]":
    known = {r.get("id") for r in records}
    dangling = []
    for record in records:
        for dep in record.get("dependencies") or []:
            for field in ("issue_id", "depends_on_id"):
                target = dep.get(field)
                if target is not None and target not in known:
                    dangling.append(f"{record.get('id')}: {field}={target}")
    return dangling


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Rewrite slug-shaped issue IDs in a beads workspace "
        "back to <prefix>-<hash>. Dry run unless --write is given.",
    )
    parser.add_argument("beads_dir", type=Path, help="path to a .beads directory")
    parser.add_argument(
        "--prefix",
        help="issue prefix, when config.yaml does not carry an uncommented "
        "issue_prefix: line. Never guessed -- the detection rule depends on it.",
    )
    parser.add_argument(
        "--write",
        action="store_true",
        help="apply the rewrite (takes a backup first). Without it, nothing is touched.",
    )
    args = parser.parse_args()

    beads_dir: Path = args.beads_dir
    if not beads_dir.is_dir():
        fail(f"{beads_dir} is not a directory")
    jsonl_path = beads_dir / "issues.jsonl"
    if not jsonl_path.is_file():
        fail(f"{jsonl_path} does not exist -- is that a .beads directory?")

    prefix = args.prefix or read_prefix(beads_dir)
    if not prefix:
        fail(
            f"no issue prefix: {beads_dir / 'config.yaml'} has no uncommented "
            "`issue_prefix:` line. `br init` writes that line commented out and "
            "keeps the real value in the database, so this is normal -- pass "
            "--prefix explicitly. It is not guessed, because the whole "
            "detection rule depends on it."
        )
    if "-" in prefix:
        fail(
            f"prefix {prefix!r} contains a hyphen; this script cannot tell such "
            "a prefix apart from a slug. Rewrite by hand."
        )

    original_text = jsonl_path.read_text(encoding="utf-8")
    records = load_jsonl(jsonl_path)
    all_ids = [r.get("id") for r in records if isinstance(r.get("id"), str)]

    print(f"workspace: {beads_dir}")
    print(f"prefix:    {prefix}")
    print(f"issues:    {len(records)}   dependency edges: {count_edges(records)}")

    slugged = sorted({i for i in all_ids if is_slug_shaped(i, prefix)})
    if not slugged:
        print("\nNo slug-shaped root IDs found. Nothing to do.")
        return 0

    # Report before touching anything.
    print(f"\nFound {len(slugged)} slug-shaped root ID(s):")
    existing = set(all_ids)
    replacements = []
    collisions = []
    for old in slugged:
        new = deslug(old, prefix)
        descendants = sum(1 for i in all_ids if i.startswith(old + "."))
        print(f"  {old}  ->  {new}   ({descendants} descendant ID(s) follow it)")
        # A collision in a personal workspace is rare enough to be worth a
        # human decision. Do not invent a disambiguator.
        if new in existing:
            collisions.append((old, new))
        replacements.append((old, new))

    if collisions:
        print()
        for old, new in collisions:
            print(f"error: {old} would become {new}, which already exists", file=sys.stderr)
        fail("collision -- refusing to write. Resolve by hand.")

    # Longest first, so that if one old root were ever a prefix of another the
    # more specific replacement still wins.
    replacements.sort(key=lambda pair: len(pair[0]), reverse=True)

    rewritten = [rewrite_strings(record, replacements) for record in records]
    new_text = dump_jsonl(rewritten)

    # ---- Verification, on the in-memory result, before anything is written ----
    problems = []

    if len(rewritten) != len(records):
        problems.append(f"issue count changed: {len(records)} -> {len(rewritten)}")

    before_edges, after_edges = count_edges(records), count_edges(rewritten)
    if before_edges != after_edges:
        problems.append(f"dependency edge count changed: {before_edges} -> {after_edges}")

    dangling = dangling_edges(rewritten)
    if dangling:
        problems.append("dangling dependency edges after rewrite:\n    " + "\n    ".join(dangling))

    remaining = sorted(
        {
            r["id"]
            for r in rewritten
            if isinstance(r.get("id"), str) and is_slug_shaped(r["id"], prefix)
        }
    )
    if remaining:
        problems.append("slug-shaped IDs remain: " + ", ".join(remaining))

    # Not just the `id` field: any occurrence of an old ID string anywhere in
    # the file, which is what catches a half-migrated free-text reference.
    stale = [old for old, _ in replacements if old in new_text]
    if stale:
        problems.append("old ID strings still present in the file: " + ", ".join(stale))

    print("\nVerification (issue count / edge count / no dangling edges / no slug IDs / no old ID text):")
    print(f"  issues:            {len(records)} -> {len(rewritten)}")
    print(f"  dependency edges:  {before_edges} -> {after_edges}")
    print(f"  dangling edges:    {len(dangling)}")
    print(f"  slug-shaped IDs:   {len(slugged)} -> {len(remaining)}")
    print(f"  old ID strings:    {len(stale)} remaining")

    if problems:
        print()
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        fail("verification failed -- nothing was written")

    if not args.write:
        print("\nDry run: nothing was written. Re-run with --write to apply.")
        return 0

    # Refuse to write unless a backup exists first.
    backup = jsonl_path.with_suffix(f".jsonl.bak-{time.strftime('%Y%m%d-%H%M%S')}")
    if backup.exists():
        fail(f"backup path {backup} already exists; refusing to overwrite it")
    backup.write_text(original_text, encoding="utf-8")
    if backup.read_text(encoding="utf-8") != original_text:
        fail(f"backup at {backup} does not match the original; refusing to write")
    print(f"\nBackup: {backup}")

    jsonl_path.write_text(new_text, encoding="utf-8")
    print(f"Wrote:  {jsonl_path}")

    # The database is derived and gitignored. A stale one alongside a rewritten
    # JSONL is the one way this can corrupt a workspace, so it goes.
    removed = []
    for db in sorted(beads_dir.glob("*.db")):
        for path in (db, Path(str(db) + "-wal"), Path(str(db) + "-shm")):
            if path.exists():
                path.unlink()
                removed.append(path.name)
    if removed:
        print("Deleted derived database files (br rebuilds them): " + ", ".join(removed))
    else:
        print("No derived database files to delete.")

    print("\nOld IDs that were replaced -- grep your source tree for these, this")
    print("script does not touch source, and git history still names them:")
    for old, new in sorted(replacements):
        print(f"  {old}  ->  {new}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
