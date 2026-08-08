# Workspace Failure Fixtures

This corpus stores small, sanitized workspace roots for reliability regression work.

Each fixture directory contains:

- `fixture.json`: human-readable metadata about the failure family and why it matters
- `fixture.json.expected_command_outcomes`: the replay contract for the key surfaces this fixture is expected to exercise
- `beads/`: the workspace payload that the loader materializes into `.beads/` inside an isolated test root

Conventions:

- Every fixture is a complete workspace root, not just a raw `.beads` dump.
- The checked-in payload lives under visible `beads/` because the remote `rch` transport used for cargo test/check/clippy does not preserve newly added hidden untracked directories reliably.
- Sidecars, recovery artifacts, and other debris are preserved when they are the point of the case.
- The payloads are intentionally small so they can live in git and be inspected by hand.
- New fixtures should model one primary anomaly each. If a future incident needs a new combination, add a new directory instead of mutating an unrelated case.
- A payload whose `*.jsonl` lacks the `format_version` marker (see
  `src/sync/jsonl_format.rs`) is a *previous-generation* file: `br` migrates it
  on import and then flags the workspace for a restamping flush. That is a real
  anomaly, but not the one any fixture here is about, so it would make a replay
  contract assert two things at once. `corrupt_db_text` and
  `interrupted_rebuild_leftovers` carry the marker for that reason — both
  rebuild from JSONL, so neither stores a content hash the edit could
  invalidate. The rest deliberately do not: their databases pin the hash of
  their own payload, and stamping the file would break the very relationship
  the fixture captures. A fixture that wants to exercise the migration should be
  added as its own directory.

Current families covered here:

- corrupt or non-SQLite `beads.db`
- JSONL conflict-marker corruption
- DB/JSONL disagreement
- duplicate config rows after legacy-schema drift
- metadata-based custom path discovery
- WAL sidecar without matching SHM
- interrupted rebuild leftovers and recovery debris
