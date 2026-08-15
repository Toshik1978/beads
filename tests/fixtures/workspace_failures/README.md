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
- A fixture whose database pins the content hash of its own `*.jsonl`
  payload must not have that payload edited — the edit breaks the very
  relationship the fixture captures. `corrupt_db_text` and
  `interrupted_rebuild_leftovers` are the exceptions: both rebuild their
  database from JSONL rather than pinning a hash against it, so neither
  stores anything an edit to the payload could invalidate. That is also why
  those two, and only those two, still carry a leading `"format_version":1`
  key on every record — a relic of the interchange marker
  `src/sync/jsonl_format.rs` used to write, since removed. Leave it in
  place: it is now useful evidence that a file written by the old
  marker-carrying format still imports cleanly, with no migration step and
  no error. The other fixtures never had the key and stay as they are.

Current families covered here:

- corrupt or non-SQLite `beads.db`
- JSONL conflict-marker corruption
- DB/JSONL disagreement
- duplicate config rows after legacy-schema drift
- metadata-based custom path discovery
- WAL sidecar without matching SHM
- interrupted rebuild leftovers and recovery debris
