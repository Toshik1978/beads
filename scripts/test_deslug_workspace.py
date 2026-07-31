#!/usr/bin/env python3
"""Self-test for `deslug_workspace.py`.

THIS IS AN OPERATOR TOOL, NOT PART OF `br`. Like the script it exercises, it
is not compiled into the binary and is not run by `task check` -- it is here
so the claims made for the migration script are reproducible by hand:

    python3 scripts/test_deslug_workspace.py

It builds throwaway `.beads` fixtures in a temp directory and checks:

* a slug-shaped epic with descendants, dependency edges into it, and a
  free-text reference is fully rewritten, all verification checks pass, and a
  second run is a no-op;
* a collision aborts without writing;
* a dry run writes nothing;
* a missing prefix is an error rather than a guess.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "deslug_workspace.py"

ROOT = "em-split-transaction-viewmodel-ih4"
OTHER = "em-9kq"


def dep(issue_id: str, depends_on_id: str, kind: str = "parent-child") -> dict:
    return {
        "issue_id": issue_id,
        "depends_on_id": depends_on_id,
        "type": kind,
        "created_at": "2026-07-30T10:00:00Z",
        "created_by": "anton",
        "metadata": "{}",
        "thread_id": "",
    }


def issue(issue_id: str, **extra) -> dict:
    record = {
        "id": issue_id,
        "title": f"Issue {issue_id}",
        "status": "open",
        "priority": 2,
        "issue_type": "task",
        "created_at": "2026-07-30T10:00:00Z",
        "updated_at": "2026-07-30T10:00:00Z",
        "created_by": "anton",
        "compaction_level": 0,
    }
    record.update(extra)
    return record


def build_workspace(directory: Path, records: "list[dict]", prefix_line: str = "") -> Path:
    beads = directory / ".beads"
    beads.mkdir(parents=True)
    (beads / "config.yaml").write_text(
        "# Beads Project Configuration\n"
        f"{prefix_line}"
        "# default_priority: 2\n",
        encoding="utf-8",
    )
    (beads / "issues.jsonl").write_text(
        "".join(json.dumps(r, separators=(",", ":"), ensure_ascii=False) + "\n" for r in records),
        encoding="utf-8",
    )
    # Derived artefacts the script is expected to delete on --write.
    (beads / "beads.db").write_bytes(b"not really sqlite")
    (beads / "beads.db-wal").write_bytes(b"")
    (beads / "beads.db-shm").write_bytes(b"")
    return beads


def standard_records() -> "list[dict]":
    return [
        issue(
            ROOT,
            issue_type="epic",
            description=f"Epic. Split out of {ROOT} planning.",
            dependencies=[dep(ROOT, OTHER, "blocks")],
        ),
        issue(f"{ROOT}.1", dependencies=[dep(f"{ROOT}.1", ROOT)]),
        issue(
            f"{ROOT}.1.2",
            dependencies=[dep(f"{ROOT}.1.2", f"{ROOT}.1")],
            notes=f"See {ROOT}.1 for context.",
            comments=[
                {
                    "id": 1,
                    "issue_id": f"{ROOT}.1.2",
                    "author": "anton",
                    "text": f"Blocked on {ROOT}.1 landing first.",
                    "created_at": "2026-07-30T11:00:00Z",
                }
            ],
        ),
        issue(
            OTHER,
            description=f"Unrelated, but mentions {ROOT} in prose.",
            dependencies=[dep(OTHER, ROOT, "related")],
        ),
    ]


def run(beads: Path, *extra: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(SCRIPT), str(beads), *extra],
        capture_output=True,
        text=True,
        check=False,
    )


failures: "list[str]" = []


def check(condition: bool, label: str) -> None:
    print(f"  {'PASS' if condition else 'FAIL'}  {label}")
    if not condition:
        failures.append(label)


def test_full_migration(tmp: Path) -> None:
    print("full migration: slugged epic with descendants, edges and free text")
    beads = build_workspace(tmp / "full", standard_records(), "issue_prefix: em\n")
    jsonl = beads / "issues.jsonl"
    before = json.loads(jsonl.read_text().splitlines()[0])
    del before

    result = run(beads, "--write")
    check(result.returncode == 0, f"exit 0 (got {result.returncode}): {result.stderr.strip()}")

    text = jsonl.read_text(encoding="utf-8")
    records = [json.loads(l) for l in text.splitlines() if l.strip()]
    check(len(records) == 4, "issue count preserved")
    check(
        sum(len(r.get("dependencies") or []) for r in records) == 4,
        "dependency edge count preserved",
    )
    known = {r["id"] for r in records}
    check(
        all(d["depends_on_id"] in known for r in records for d in r.get("dependencies") or []),
        "no dangling dependency edges",
    )
    check(ROOT not in text, "no occurrence of the old ID anywhere in the file")
    check("em-ih4" in known, "root rewritten to em-ih4")
    check({"em-ih4.1", "em-ih4.1.2"} <= known, "descendants rewritten")
    prose = next(r for r in records if r["id"] == OTHER)["description"]
    check("em-ih4" in prose and ROOT not in prose, "free-text reference rewritten")
    comment = next(r for r in records if r["id"] == "em-ih4.1.2")["comments"][0]["text"]
    check("em-ih4.1" in comment, "comment body rewritten")
    check(not list(beads.glob("*.db*")), "derived database and sidecars deleted")
    check(len(list(beads.glob("issues.jsonl.bak-*"))) == 1, "backup taken")

    second = run(beads, "--write")
    check(second.returncode == 0, "second run exits 0")
    check("Nothing to do" in second.stdout, "second run is a no-op")
    check(jsonl.read_text(encoding="utf-8") == text, "second run left the file byte-identical")


def test_collision_aborts(tmp: Path) -> None:
    print("collision: stripped form already exists")
    records = standard_records()
    records.append(issue("em-ih4", title="Pre-existing occupant of the stripped ID"))
    beads = build_workspace(tmp / "collision", records, "issue_prefix: em\n")
    jsonl = beads / "issues.jsonl"
    original = jsonl.read_text(encoding="utf-8")

    result = run(beads, "--write")
    check(result.returncode != 0, "non-zero exit")
    check("already exists" in result.stderr, "explains the collision")
    check(jsonl.read_text(encoding="utf-8") == original, "wrote nothing")
    check(not list(beads.glob("issues.jsonl.bak-*")), "took no backup")
    check((beads / "beads.db").exists(), "left the database alone")


def test_dry_run_writes_nothing(tmp: Path) -> None:
    print("dry run: the default")
    beads = build_workspace(tmp / "dry", standard_records(), "issue_prefix: em\n")
    jsonl = beads / "issues.jsonl"
    original = jsonl.read_text(encoding="utf-8")

    result = run(beads)
    check(result.returncode == 0, "exit 0")
    check("Dry run" in result.stdout, "says it was a dry run")
    check(jsonl.read_text(encoding="utf-8") == original, "wrote nothing")
    check((beads / "beads.db").exists(), "left the database alone")


def test_missing_prefix_is_an_error(tmp: Path) -> None:
    print("prefix: commented out in config.yaml, not supplied on the command line")
    beads = build_workspace(tmp / "noprefix", standard_records(), "# issue_prefix: em\n")

    result = run(beads)
    check(result.returncode != 0, "non-zero exit rather than a guess")
    check("--prefix" in result.stderr, "tells the operator to pass --prefix")

    supplied = run(beads, "--prefix", "em")
    check(supplied.returncode == 0, "--prefix makes it work")
    check(ROOT in supplied.stdout, "reports the slugged ID it found")


def main() -> int:
    with tempfile.TemporaryDirectory() as raw:
        tmp = Path(raw)
        test_full_migration(tmp)
        test_collision_aborts(tmp)
        test_dry_run_writes_nothing(tmp)
        test_missing_prefix_is_an_error(tmp)

    print()
    if failures:
        print(f"{len(failures)} check(s) FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
