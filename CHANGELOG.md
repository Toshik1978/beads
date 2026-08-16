# Changelog

What changed in each `br` release and why. Highlights are written by hand; the
commit lists under them are generated with [git-cliff](https://git-cliff.org)
via `TAG=vX.Y.Z task changelog`.

Versions follow [semver](https://semver.org). Commits follow
[Conventional Commits](https://www.conventionalcommits.org).

---

## v1.7.0 — 2026-08-16

`br` gains a second surface: **`br remote`**, which mirrors a workspace into a
YouTrack project. It is the only command in the CLI that opens a socket —
nothing else does — and the mirror moves only when one of its five verbs is
actually run, never as a side effect of `list`, `create`, or `sync`. Without a
`.beads/remote.yaml` there is no network and no behaviour change at all, so
every existing workspace is unaffected by this release.

The design is stateless full reconciliation. There is no watermark, no
sidecar, and no local sync state to corrupt or resynchronise: every run
fetches both sides and re-derives the whole plan from nothing. Pairing
identity is single-sided and lives in the bead's own `external_ref`, so the
mirror's memory rides in `issues.jsonl` and survives `git clean -fdx`, a fresh
clone, or a deleted database. Beads stays authoritative for every field except
the three a human actually edits in a web UI — **`State`, `Priority` and
comments** — and those two fields resolve by timestamp, with a tie going to
beads.

The subsystem was built and then validated end to end against a live YouTrack
instance, which is where six of its assumptions turned out to be wrong and got
corrected; the fixes below are mostly that.

### Highlights

- **`br remote init` provisions the project, then does nothing.** It
  reconciles the project's schema against `remote.yaml` — five custom fields,
  plus every `Type`/`State`/`Priority` value the maps name — and a project
  that already matches issues **zero write requests**, so re-running it is
  free. Two of its effects are visible to people who never run `br`, and it
  names each one before it acts: custom field prototypes are instance-wide
  (YouTrack has no per-project field namespace to use instead), and the
  project's adoption defaults are rewritten. Stock YouTrack adopts a new issue
  as `Bug` / `Submitted` / `Normal`, which reads back as *a deferred bug at
  P3* — wrong on all three axes, for every issue a human types into the web
  UI. `--keep-project-defaults` leaves them alone.
- **Bundles are never written blind.** Before adding a value to a `Type` or
  `State` bundle, `init` scans which projects use it. A provably private
  bundle is filled in place; a shared bundle on an empty project is copied,
  the project's field re-pointed at the copy, and the copy filled; a shared
  bundle on a project that already holds issues is refused, with both ways
  forward named. A scan the token cannot complete refuses as well, rather than
  assuming privacy — the branch that assumes privacy is the branch that writes
  into another team's vocabulary. `--allow-shared-bundle` opts into that write
  and says out loud, before making it, which projects will see the new values.
- **`status` writes nothing, and a consumer can assert that.** It fetches both
  sides and prints the plan — creates, field changes with the winning
  direction marked, link changes, comments pending each way, adoption
  candidates, refusals, dangling refs, unmapped values, tombstoned pairs and
  unmirrored relations — printing each section only when it has content.
  `--json` emits `{"project", "plan", "reads", "writes"}`, where `writes` is
  always `0`.
- **A first `push` refuses without `--confirm-initial`.** A run where no bead
  is paired yet would create an issue for every unpaired bead, and since
  nothing here deletes a remote issue, aiming that at the wrong project means
  cleaning up by hand. The asymmetry is documented rather than papered over:
  `pull` carries no such gate, and adopting even one issue writes a pairing
  that satisfies the gate for the *next* push — so `url` and `project` deserve
  the same check before a first pull as before a first push.
- **A first run is resumable, and cannot double-create.** Creates land in
  batches with progress reported; the plan is then recomputed once the new
  pairings exist, so the same run goes on to mirror their links, comments and
  labels. An issue whose create succeeded but whose response was lost is
  recognised on the next run by the `Beads ID` it already carries and simply
  paired — never created a second time, never adopted as a stranger.
- **Nothing is ever deleted on the remote.** No code path in this codebase
  issues an HTTP `DELETE` against an issue — the client has no general delete,
  only one narrowly typed to link removal — and an end-to-end test drives a
  local delete and asserts the run made no `DELETE` request at all. A local
  `br delete` moves the mirrored issue to `deleted_state` and comments saying
  so; a tombstone left behind by `br rename` is forwarded to the new id
  instead, so a rename never reads as a deletion.
- **Adoption is whole, once, and refuses rather than guesses.** An issue
  created in the web UI that no bead claims is imported entire on the next
  `pull` or `sync` — the one direction where a remote record outranks the
  local one — and is an ordinary mirrored bead from then on. Adoptees are
  minted in parentage order, so a child adopted alongside its parent is
  created as a proper child rather than flat and reparented afterwards. A
  `Type`, `State` or `Priority` no map covers is refused and named
  individually, together with the config key that would cover it; refusing one
  adoptee does not stop the run.
- **Comments cross both ways with stateless echo suppression.** Outbound
  comments carry a `[br]` marker and inbound ones are filtered by author, so
  neither side re-imports its own writing — with nothing recorded between runs
  to keep in step.
- **The token has exactly one source.** `BR_YOUTRACK_TOKEN`, and nowhere else:
  no file `br` writes — not config, not logs, not `--dry-run` output — can
  carry it. Its `Debug` is redacted, and a test asserts that no error variant
  can leak it.

### What it deliberately does not do

- **A link drawn by hand between two already-paired issues is deleted on the
  next push.** Links are local-wins like every other field except the three
  named above. That is the contract working, not a bug — but it is the one
  local-wins consequence most likely to surprise, so it is stated here.
- **Editing an already-mirrored comment appends a duplicate instead of
  updating it.** Comments are matched by exact body text, there being no
  shared identity for either side to key on; an edited body no longer matches
  the original it was being suppressed against. For the same reason, a second
  local comment identical to one already pushed is never pushed, being
  indistinguishable from an echo. Both are tracked as follow-up work rather
  than fixed here.
- **Only three dependency types mirror.** Hierarchy becomes a `Subtask` link,
  `blocks` a `Depend` link, and `related`/`relates-to` a `Relates` link.
  Everything else — `waits-for`, `duplicates`, `conditional-blocks`,
  `discovered-from`, `replies-to`, `supersedes`, `caused-by` and any custom
  type — has no YouTrack equivalent, and collapsing one onto `Depend` would be
  lossy in a way nothing could undo. They are listed under "unmirrored
  relations" so a `br dep list` row with no matching link change is explained
  rather than silently dropped.
- **Creation timestamps do not survive either crossing.** A mirrored issue
  reads as created today by the token's owner, because `created` and
  `reporter` are read-only on YouTrack's issue entity; an adopted issue reads
  locally as created at adoption time, for the same reason on the beads side.
- **YouTrack is the only backend.** The CLI surface and everything above the
  wire are backend-neutral, but nothing else is implemented.

### Features

- [dbe9df9](https://github.com/Toshik1978/beads/commit/dbe9df9687140d3aa3f40f8bb912458d0d7aacb4) feat(remote): add the remote client, config, and credentials
- [f9b22a8](https://github.com/Toshik1978/beads/commit/f9b22a893faf538b508f7504e982b36b0bf8a818) feat(remote): map beads issues, links, and labels to YouTrack
- [0a58376](https://github.com/Toshik1978/beads/commit/0a58376652e20c2f4be0677d4268cae50a392e4a) feat(remote): add br remote init and its YouTrack provisioning
- [65155f2](https://github.com/Toshik1978/beads/commit/65155f267eca2c09b791d86e1e1f848470917805) feat(remote): reconcile a workspace against the mirror, statelessly
- [218f2f6](https://github.com/Toshik1978/beads/commit/218f2f64515447abea2d52d11864eb0906c2411f) feat(remote): sync comments with symmetric echo suppression
- [2d50ce7](https://github.com/Toshik1978/beads/commit/2d50ce745683ba3f8908b5fc3ee075b390303e73) feat(remote): forward renamed tombstones and mark genuine deletions
- [f34a181](https://github.com/Toshik1978/beads/commit/f34a1811c491a5dd799b079feb1426799b8f2aac) feat(remote): adopt web-UI issues into the workspace, once
- [65a7745](https://github.com/Toshik1978/beads/commit/65a7745cf287fbeb01b0ae0ee028491b027e402b) feat(remote): add push, pull and sync with a resumable first run

### Bug Fixes

- [06f6dda](https://github.com/Toshik1978/beads/commit/06f6dda1af36fc3c4bd4db5a81ef62938b034901) fix(remote): validate priority_map totality and correct sync --help
- [bcf5ff1](https://github.com/Toshik1978/beads/commit/bcf5ff1db3b8249ce329a5a98d528183e3abe86f) fix(remote): correct init's dry-run and adopted-clone disclosures
- [7604d19](https://github.com/Toshik1978/beads/commit/7604d1936d37a913af26c4275159c866591ba573) fix(remote): account for comment pulls a bare push leaves unexplained
- [29ba56c](https://github.com/Toshik1978/beads/commit/29ba56c91970c8535d75cf5f7e5160c25e4aeda1) fix(remote): make init's out-of-range priority pre-flight reachable
- [d36178b](https://github.com/Toshik1978/beads/commit/d36178b896ed3b54561de7b77b74332b69f0777c) fix(test-support): time out a stalled mock-server read/write
- [ff8cf58](https://github.com/Toshik1978/beads/commit/ff8cf58b15f87e5fab5915f18b4ce72e89dbf61e) fix(remote): paginate every YouTrack read that could be truncated
- [edea7eb](https://github.com/Toshik1978/beads/commit/edea7ebf29f8daf4a5295265176694307a8970dc) fix(remote): page the link-type read, pin the field-summary paging, and stop double-printing the scan
- [9842d5e](https://github.com/Toshik1978/beads/commit/9842d5e7d9191e1321907bb47545a256bff08fb9) fix(remote): name the bundle type inside a re-point body
- [4fb7e9d](https://github.com/Toshik1978/beads/commit/4fb7e9d1b8f38c903e833421edb60eb369b0efeb) fix(remote): render each verb's own half of the plan

### Documentation

- [51933c7](https://github.com/Toshik1978/beads/commit/51933c75fac2ba434e70cc8cc008e510118ec064) docs: amend the br remote spec and plan it against a live YouTrack instance
- [0741a68](https://github.com/Toshik1978/beads/commit/0741a68d2d5ee8e2018d61d438bcedee8dd94908) docs: document br remote, its verbs, and what init changes

---

## v1.6.0 — 2026-08-16

An audit of the heaviest consumer of this tool — a workspace with 885 issues,
1177 comments, 1166 dependency edges and 6457 recorded `br` invocations over
five weeks — found a large part of the data model and two whole subsystems
that had never carried a non-default value or been invoked once, in that
workspace or in any of the four others on the machine that produced it. This
release removes them: the sync witness subsystem, the close-policy gates, the
JSONL `format_version` marker, and fifteen never-populated `Issue` fields,
followed by a schema v18 → v19 migration that drops the corresponding SQLite
columns and rebuilds every stored `content_hash`. Nothing here was dead code —
`task lint:dead` reported zero unreferenced items for all of it — this is a
product decision to stop carrying surface nobody used, not a dead-code sweep.

### Highlights

- **The sync witness subsystem is gone.** `--witness`, `--witness-chunk-lines`
  and `--witness-parallelism`, the `Witness` sync operation, and its artefact
  paths are removed along with `src/sync/witness.rs`. This is unrelated to the
  JSONL mtime/size staleness witness, the dependency-cycle witness, or the
  startup-cache witness, which share the name and are untouched.
- **The close-policy gates are gone.** `ClosePolicy` and its evaluator, the
  `--bypass-policy` and `--bypass-reason` flags, and the `close_policy:`
  section of `.beads/policy.yaml` are removed. `Workflow` and `CapacityPolicy`
  share the same module and are untouched — they are enforced on every status
  transition, not just on close, and remain fully live.
- **The JSONL `format_version` marker is gone.** `issues.jsonl` goes back to
  carrying no generation marker at all: a record is `Issue`'s own derived
  `Serialize` output, nothing wrapped around it. A file written by a build
  that still stamped the marker imports cleanly — the key is simply an
  unmodelled field, dropped on read with no error and no warning. `br sync
  --json` no longer reports `format_upgraded` or `format_upgraded_from` —
  both were only meaningful when a file stamped with the marker was read —
  and the `JsonlFormatTooNew` error variant (`JSONL_FORMAT_TOO_NEW` in
  structured error output) is gone with it: there is no longer a version to
  be too new.
- **Fifteen never-populated fields leave `Issue`:** `compaction_level`,
  `compacted_at`, `compacted_at_commit`, `original_size`, `sender`,
  `ephemeral`, `pinned`, `is_template`, `estimated_minutes`, `due_at`,
  `closed_by_session`, `source_system`, `source_repo_path`, `agent_context`
  and `assignee`, along with the flags that fed them (`--ephemeral`,
  `-e`/`--estimate`, `--due`/`--due-after`/`--due-before`/`--overdue`,
  `--session`, `--agent-context`, `--source-repo-path`,
  `-a`/`--assignee`/`--unassigned`/`--claim`/`--if-assignee`/`--by-assignee`).
  `owner`, `defer_until`, `former_ids`, `external_ref` and `source_repo` are
  kept — each is populated in real workspaces and sits adjacent to something
  removed.
- **Schema v19 drops the columns and rebuilds every hash.** The SQLite columns
  behind the fifteen removed fields stayed in place, unread, through the field
  removal so no `content_hash` moved before this step; v19 drops them and
  their indexes and recomputes every stored digest in the same transaction.
- **A routed command that fails partway now says what it wrote.** `update`,
  `close`, `reopen`, `delete`, `label add`/`remove` and `detach` commit each
  routed workspace before opening the next, so a later route's failure cannot
  undo an earlier route's write — and used to surface only the cause, which
  names the target that failed and reads as though nothing had happened. That
  case is now exit **3** with a new error code `PARTIALLY_APPLIED`, whose
  `context` splits every target into `applied`, `uncertain` and `not_applied`
  and carries the underlying `cause_code`. It is marked non-retryable: re-run
  against `not_applied`, not against the whole command. Unrouted commands and
  single-route fan-outs are unaffected and report exactly what they did before.

### Four behavior changes that fall out of the column drop, not a redesign

- **`br stats`' "Pinned" count now counts only `Status::Pinned`.** It used to
  also count the boolean `pinned` column, which no workspace on this machine
  ever set to true; with the column gone, the status value is the only thing
  left to count, which is what the field was presumably meant to mean anyway.
- **A former template epic now counts toward `epics_eligible_for_closure`.**
  The `is_template` flag used to exclude an epic from that count regardless of
  its children's status; with the column gone, an epic that used to be flagged
  as a template is evaluated the same as any other epic once its children are
  closed.
- **A legacy `is_template = 1` row is now visible in `list`, `search`,
  `ready`, `stale` and `blocked`.** The flag gated nineteen SQL predicates
  across those five paths; with the column gone, so is the filter. No CLI
  path ever set `is_template`, so real exposure is close to zero — but it is
  not exactly zero for a workspace where the flag was set by hand against the
  SQLite file directly.
- **`br list --format csv` and `br search --format csv` emit two fewer
  columns.** `assignee` left `DEFAULT_FIELDS` (8 columns → 7) and `assignee` /
  `due_at` left `ALL_FIELDS` along with the fields themselves; all three were
  valid `--fields` names before this branch. An unrecognised `--fields` name
  used to be dropped silently — `--fields assignee` alone produced an empty
  header and an empty row at exit 0 — and is now a validation error instead.

### The real hazard is a v18/v19 ping-pong, not a one-way upgrade

Measured directly against the released `br` 1.5.0 binary, on a workspace
schema-v19 migrated by this release: reopening a v19 workspace with an older
`br` does not fail to open it. Its `ensure_columns` step re-adds all fifteen
dropped columns with `ALTER TABLE ADD COLUMN` — which appends at the end of
the table, so the physical column order no longer matches
`EXPECTED_ISSUE_COLUMN_ORDER` ([`src/storage/schema.rs`](src/storage/schema.rs),
whose own doc comment names mis-ordered appends as the cause of "no such
column" errors on older databases) — and stamps `user_version` back down to
18 before the old binary does anything else. The old binary's own read then
fails with `Database error: no such column: <one of the dropped columns>` —
which column is named depends on which query the command runs first;
`assignee`, on `br list --json` and `br show --json`, in the measurement
here — but a *write* (`br create`, `br update`, and so on) goes through
cleanly against the downgraded schema, JSONL flush included; there is no
half-failure and no silently-stopped export. Reopen that same workspace with
the new binary and it logs `Migrating database to schema version 19 (drop
dead columns, rehash)`, converges the schema back to v19, and reads back
everything the old binary wrote correctly.

The real hazard is what that round trip costs, not that it can't be undone.
Each direction rewrites the whole of `issues.jsonl` on its next flush, but not
for the same reason. Going v18 → v19, schema v19's own migration exists to
rebuild every stored `content_hash`, which is what marks every issue dirty.
Going v19 → v18, the old binary never runs that (or any) rehash arm — it
stamps `user_version` back to 18 directly — so the rewrite there comes from
the serialized record shape differing instead: the old `Issue` still emits
the fifteen removed fields, so every line it writes differs from the v19
binary's, and the next flush is a full-file diff regardless of the cause.
`beads.db`
is gitignored and derived, so a teammate who only ever pulls the new JSONL and
rebuilds their own database never sees any of this; the hazard is confined to
a single machine that carries two `br` binaries at once — an older release
still on `PATH` alongside a newer build — pointed at the same
already-migrated workspace.

### ⚠ Breaking Changes

- [93a2795](https://github.com/Toshik1978/beads/commit/93a279557c88ccf5ef1cced86cdb920955842327) remove the witness subsystem
- [098b379](https://github.com/Toshik1978/beads/commit/098b37916cd071551031eb2371cf42a21daa5747) remove the close-policy gates and bypass flags
- [5a9b0c4](https://github.com/Toshik1978/beads/commit/5a9b0c46c0d82f614eb6d03b8dbc0ebfa5ebf822) remove the JSONL format marker
- [e183ade](https://github.com/Toshik1978/beads/commit/e183ade2df9160db2c501ca40f10f24cd754fbb0) remove fifteen never-populated fields from Issue
- [cb7e5b7](https://github.com/Toshik1978/beads/commit/cb7e5b7cd06e062c7f891c0f49aa9194162b125a) schema v19 drops the dead columns and rebuilds hashes

### Features

- [cb7e5b7](https://github.com/Toshik1978/beads/commit/cb7e5b7cd06e062c7f891c0f49aa9194162b125a) feat(storage)!: schema v19 drops the dead columns and rebuilds hashes

### Bug Fixes

- [2850922](https://github.com/Toshik1978/beads/commit/2850922c7c8e5c287331991020f69d67169450ee) fix: correct the findings from the whole-branch review
- [c5b07bf](https://github.com/Toshik1978/beads/commit/c5b07bfe33f2989200e96f275973f71a92da4463) fix(storage): remove the unwritable workflow-policy bypass field
- [863ea63](https://github.com/Toshik1978/beads/commit/863ea63685757be155fed0c45225df157cb943d3) fix(cli): report which targets a routed update already wrote
- [d357d80](https://github.com/Toshik1978/beads/commit/d357d80d469d86b8a00dd9927b0f1920e68297fa) fix(cli): report partial application from every routed fan-out

### Documentation

- [4f72584](https://github.com/Toshik1978/beads/commit/4f72584379f1e985b8538659eddb390d20492285) docs: record the cleanup spec, the br remote spec, and the cleanup plan
- [4ee592e](https://github.com/Toshik1978/beads/commit/4ee592e85128079c8dc6f0a1d68b6d78af1694dc) docs: remove the deleted fields, flags and subsystems from the docs
- [0dd8085](https://github.com/Toshik1978/beads/commit/0dd808519d4a2b91e40ef1cb22218ac3ad02c68f) docs: record inbound adoption and bundle provisioning in the br remote spec

### Others

- [93a2795](https://github.com/Toshik1978/beads/commit/93a279557c88ccf5ef1cced86cdb920955842327) refactor(sync)!: remove the witness subsystem
- [098b379](https://github.com/Toshik1978/beads/commit/098b37916cd071551031eb2371cf42a21daa5747) refactor(close)!: remove the close-policy gates and bypass flags
- [5a9b0c4](https://github.com/Toshik1978/beads/commit/5a9b0c46c0d82f614eb6d03b8dbc0ebfa5ebf822) refactor(sync)!: remove the JSONL format marker
- [e183ade](https://github.com/Toshik1978/beads/commit/e183ade2df9160db2c501ca40f10f24cd754fbb0) refactor(model)!: remove fifteen never-populated fields from Issue

---

## v1.5.0 — 2026-08-08

`issues.jsonl` is the artefact this project is actually about. It is the file
that gets committed, the file that gets merged, and the file other tools read —
and it carried no way to say which generation of the format wrote it. Two
separate defects could also silently revert an edit to it. This release fixes
all three: every record now leads with a `format_version`, and neither an
out-of-band edit nor a clock-skewed peer can have its change written back over.

The rest is surface. `br update` takes compare-and-set guards, `br list` and
`br search` take ten date bounds and four exclusions, `br rename` becomes a
command rather than an internal primitive, and `br statuses` / `br types`
answer a question that previously had none: which status and type names does
*this* workspace accept.

No schema migration — an existing database opens unchanged. An existing JSONL
is migrated forward on import, counted, reported, and restamped by the next
flush, so the upgrade is announced once. A file written by a newer build is
refused rather than half-read.

### Highlights

- **Every JSONL record leads with `format_version`.** The marker is per record
  rather than per file because the two things these records actually undergo —
  concatenation and three-way merge — destroy a header line, and
  `metadata.json` is per workspace, so it says nothing about a file that
  arrived from somewhere else. It lives outside the `Issue` struct, in a
  serialize wrapper, because it describes the format rather than an issue; the
  pinned `Issue` field set is unchanged as a result. A missing marker is
  generation 0. A newer generation is refused with `JsonlFormatTooNew`,
  mirroring the `SchemaMismatch` posture for a database newer than the binary,
  and for the same reason: a newer format may reinterpret a key this build
  already knows, and a best-effort read would flush the misreading back over a
  committed file. An unrecognised key is still dropped, now with a stated
  reason and a round-trip test rather than by accident.
- **`br update` takes compare-and-set guards.** `--if-status` and
  `--if-assignee` generalise the predicate `--claim` already built, and compose
  with it and with each other. The guard is checked against the row the write
  transaction loaded *and* restated on the `UPDATE`'s `WHERE` clause — both
  halves, so that the error can name the value actually found, and so that no
  later refactor can hoist the check out of the transaction and reintroduce the
  race it exists to close. A failed guard writes nothing and reports
  `PRECONDITION_FAILED` with exit code 4 — deliberately not the 3 that carries
  `ISSUE_NOT_FOUND`, because a caller retrying a guarded update has to tell
  those two apart. A guard with no field update to guard is refused rather than
  silently unenforced.
- **Ten date bounds and four exclusions on the query surface.**
  `--created-after/-before`, `--updated-*`, `--closed-*`, `--due-*` and
  `--defer-*` land on `br list` and `br search` together, because both commands
  share one flattened argument struct. Both ends of a range are inclusive,
  matching what `--updated-before` already meant and what `br stale` rests on.
  A bare date widens to the whole day it names, because a range whose ends both
  resolve to 09:00 matches nothing while looking like an honest empty result. A
  NULL column never satisfies a bound on it, so "closed in the last week"
  cannot return everything still open, and setting a `closed_*` bound implies
  `--all` — without that the flag could only match rows the default view hides.
  Relative values need the attached form (`--created-after=-7d`), the same
  convention `--sort=-updated` already follows. The exclusions —
  `--exclude-label`, `--exclude-type`, `--no-labels`, `--no-parent` — reach
  `br ready` as well, as one shared struct and one SQL builder, so the same
  flag cannot come to mean three things on three commands. Repeating an
  exclusion is a union, "neither a nor b", deliberately not symmetric with
  `--label`'s AND, and `--no-parent` asks the `parent-child` dependency row
  rather than the shape of the ID. Five query fast paths that used to name
  individual filter fields now gate on a single predicate, so a bound added
  later cannot leave one of them answering a filtered query unfiltered.
- **`br rename` is a command.** v1.4.0 shipped the transactional subtree
  cascade, the tombstone at the vacated ID and the `former_ids` provenance, and
  `br detach` already drove all of it — what was missing was the front door, so
  there was no supported way for a person to change an issue's ID. The storage
  layer is untouched by this: it is a command, its arguments, its dispatch and
  its refusals. Beyond an occupied target and a tombstoned source, it refuses
  to move an issue within the hierarchy or to change its prefix, and names what
  to run instead — `br update --parent` and `br detach` for reparenting,
  `br sync --rename-prefix` for prefixes. Both would break invariants that hold
  everywhere else: a dotted ID always names its real parent, and a workspace's
  rows share its prefix. `--dry-run` reports the whole subtree the rename would
  move without writing.
- **`br statuses` and `br types` print the vocabulary this workspace accepts.**
  That set is project-specific rather than constant — a custom status is
  accepted unless `policy.yaml` enumerates one under `workflow.statuses` with
  `strict` on — and there was no way to ask which of those two worlds you were
  in. `br statuses` merges the built-in set with the policy's, marks which is
  which and which are currently allowed, and prints the ready group. It
  distinguishes three states, the middle one deliberately: no policy, strict
  with an empty status list — which enforces nothing, and this is the only
  place that is visible — and strict with a set. A test asserts that what
  `br statuses` calls allowed is what `br update` actually accepts, and the
  completion tables behind both commands are now shared rather than copied,
  with exhaustive-match reminders beside them so a new variant breaks the build
  instead of silently going unreported.
- **Smaller gaps closed.** `br close --reason-file` mirrors the
  `--description-file` that already existed, for reasons too long or too
  quote-heavy to pass through a shell. `br create --notes` and `--acceptance`
  let an issue be created fully populated instead of costing a second command
  and a second `updated_at` bump. `br update --append-notes` adds a paragraph
  instead of replacing the field, doing its read-modify-write inside the write
  transaction so two agents appending at once cannot lose each other's text.
  `br close --continue` closes what it can and reports the rest by ID: it
  replaces the exit-code rule with "did every issue end up closed", counting
  already-closed as closed so a retry over a half-finished batch exits 0, and
  reports the new `PARTIALLY_COMPLETED` rather than `NOTHING_TO_DO`, which
  would be a lie in front of "closed 2 issue(s)". The default rule is
  untouched. Dropping an issue from the batch for a policy violation recomputes
  the batch's closable set rather than narrowing it, because the dropped issue
  may be another's blocker. `br stale --limit` caps the report.

### Two ways a write could revert itself

Both are import-path defects, both have been there since 1.0.0, and both end
the same way: a change that appeared to succeed and was then written back over
from the other side.

- **A hand edit to `issues.jsonl` no longer undoes itself.** Removing a field
  from a record does not touch `updated_at`, so the import read both sides as
  the same revision and skipped the record as up-to-date — and because the skip
  left the export hash unrecorded, the next flush wrote the unedited database
  row back over the file. Equal timestamps are no longer a verdict. They are
  resolved against the stored row, reusing the comparison the import already
  ran for every skip, and at a tie the JSONL wins. That is not a preference for
  one side but a statement of what the two artefacts are: the file is
  committed, and the database beside it is gitignored and rebuildable. It is
  also only safe because of the fix below — every local write now advances
  `updated_at` strictly, so a row that differs at an identical timestamp cannot
  be unflushed local work.
- **A clock-skewed peer can no longer make a label or dependency write
  invisible.** v1.4.0 clamped `updated_at` on the writes that already held the
  row in hand and left four paths unclamped as a recorded follow-up; this is
  that follow-up. Labels, dependencies and comments all travel in the JSONL and
  all take part in the sync comparison, but the import decides what to do on
  `updated_at` alone. A label or dependency write against a row seeded from a
  machine whose clock runs ahead therefore *lowered* the timestamp while
  changing synced content: the next import skipped the row as older, and the
  following flush exported the stale copy back over the file. Every such write
  now reads the row's own stored value and advances strictly past it. The bulk
  paths read the affected rows first and partition, so the ordinary case still
  costs one statement per chunk.

### Features

- [ddbe8b3](https://github.com/Toshik1978/beads/commit/ddbe8b3b520597bfbb97c4110fad68429db81603) feat(sync): version the JSONL interchange format and give it a migration path
- [aca1148](https://github.com/Toshik1978/beads/commit/aca11485188c84bc88de901a4f64ed44b944cb3b) feat(update): add compare-and-set guards --if-status and --if-assignee
- [c3fc89f](https://github.com/Toshik1978/beads/commit/c3fc89f5f819ddbab1808630ed9ebc349cf41f91) feat(list): add date-range filters to br list and br search
- [ddef216](https://github.com/Toshik1978/beads/commit/ddef21613c58162f89ca87ad6a39dec7d059f0a9) feat(list): add exclusion filters to br list, search and ready
- [d8e9549](https://github.com/Toshik1978/beads/commit/d8e95499a736fd2ccf9b4298193aecb1660861d7) feat(cli): add br rename as a first-class command
- [499713b](https://github.com/Toshik1978/beads/commit/499713b5fb6a50eab1889798f8d9b387940f4f22) feat(cli): add br statuses and br types, and br stale --limit
- [5d2396f](https://github.com/Toshik1978/beads/commit/5d2396fb5a74c6edd9c5c021ce76542681295c66) feat(cli): add close --reason-file/--continue, update --append-notes, create --notes

### Bug Fixes

- [1dc875c](https://github.com/Toshik1978/beads/commit/1dc875cabe3d3d30b65543030c5391b925c398e1) fix(storage): clamp label, dependency and comment writes against future timestamps
- [23c3a73](https://github.com/Toshik1978/beads/commit/23c3a73cd83960cb900385159ab35203200a6577) fix(sync): let the JSONL win when an import ties on updated_at

### Others

- [4610507](https://github.com/Toshik1978/beads/commit/4610507492f1217d3693e019c5f3ec15bd877f50) refactor(error): remove the unconstructed InvalidStatus and InvalidType errors
- [e748ff5](https://github.com/Toshik1978/beads/commit/e748ff5ca29049cd7d49972c02335d5241cb9806) build(lint): add an advisory dead-public-code report
- [db0a227](https://github.com/Toshik1978/beads/commit/db0a2272ee618e198807647fa230dda432291dd7) refactor: clear the actionable dead-code buckets in src/ and test-support

---

## v1.4.0 — 2026-08-05

The dotted ID becomes the truth about parentage. Until now an issue's parent
was recorded twice — once in the `parent-child` dependency row, once in the ID
itself — and nothing kept the two in agreement. An epic could hold a child that
had been reparented away from it, refuse to close because of that child, and
offer no way to move the child out. This release makes the ID authoritative:
a dotted prefix always names the real parent, having a parent always implies a
dotted ID, and every path that can set or clear a parent either renumbers the
issue or refuses and names what to run instead.

Renumbering means renaming, so the rename had to become safe: one transactional
subtree cascade, a tombstone at the vacated ID, and a `former_ids` array so
references written before the move keep resolving. `br detach` is the new
command that makes a child independent, and `br info --projections` reports the
divergence that JSONL import can still let in.

An existing workspace migrates in place (schema v17 → v18) and imports exactly
as before. `former_ids` is omitted from the JSONL entirely for issues that were
never renamed, so a repository that does no renaming exports byte-identical
records.

### Highlights

- **`br detach` moves an issue out from under its parent.** What happens
  depends on the ID's own shape, not just on whether a dependency exists. A
  **dotted** ID makes a hierarchy claim by its shape, so detaching mints a
  fresh flat ID from the same generator `br create` uses, renames the issue to
  it, and drops the edge. A **flat** ID makes no such claim, so detaching drops
  the edge and leaves the ID alone. An issue with **no parent** by either
  measure is a successful no-op, so detaching twice in a row is safe to script.
  It takes batch IDs, emits JSON, and routes across workspaces like the other
  mutating commands. The point of it is closing epics: an epic cannot close
  without `--force` while it has open children, and this is how a child that
  should no longer count stops counting, without touching the epic.
- **An old ID keeps working after a rename.** The moved issue accumulates its
  previous IDs in `former_ids`, oldest first, and a tombstone is left at the
  vacated address naming the destination. The two do different jobs: the
  tombstone is what propagates the move through `issues.jsonl` — without it a
  clone still holding the old ID merges its copy back as a live issue and
  quietly undoes the rename — while `former_ids` is permanent provenance that
  outlives the tombstone, which `br delete --hard` may collect. `br show
  <old-id>` resolves to whatever now holds it; `br show <new-id> --json` lists
  the old one. The resolver change was a reordering rather than a new step: the
  two exact-match lookups now prefer a live issue, the `former_ids` redirect
  runs next, and a tombstone-tolerant retry follows so a genuinely deleted ID
  still reports its tombstone. All of it sits ahead of the abbreviation scan,
  so a fully spelled current ID costs nothing new.
- **Seven chokepoints, not six.** The invariant is only worth as much as its
  least-guarded entrance, so every path that can set or clear a parent-child
  edge was closed. Attaching renumbers: `br update x --parent E` and `br dep add
  x E --type parent-child` both move `x` to `E.n` and report the new ID.
  Clearing refuses and names the alternative: `br update x --parent ""` on a
  dotted ID and `br dep remove` on a parent-child edge both point at `br
  detach`, which does the rename too, while `br create --dep parent-child:E`
  points at `--parent`, which mints the child correctly in the first place.
  Refusing rather than silently detaching is deliberate — a rename is
  consequential and visible, and should not be a side effect of an update flag.
  The spec named those six. Implementation found a seventh — the `Parent:`
  field of a markdown import,
  when the reference is forward or duplicated, which folded its resolved edge
  into the ordinary bulk dependency insert and so set the edge without
  renumbering the child. Parent-child rows in an import are now applied one at
  a time in file order, each endpoint re-resolved immediately before it is
  used, so a rename earlier in the same batch is picked up through the
  `former_ids` fallback instead of hitting a tombstone.
- **`br info --projections` reports the divergence it cannot prevent.** JSONL
  import accepts arbitrary data, including repositories that already violate
  the invariant, so it is the one door that cannot be locked — and the reason
  `--force` on close stays available. The detector reports every non-tombstone
  issue whose dotted prefix disagrees with its `parent-child` edge, in human
  and JSON output, with a capped ID list that states the cap; a single
  comparison covers all three shapes (prefix naming a different parent, dotted
  with no edge at all, flat with an edge). It is deliberately *not* a rebuild
  reason: a cache rebuild cannot fix divergence, which lives in the JSONL and
  survives a rebuild intact. A failed divergence query now says `unavailable`
  rather than defaulting to zero and reading as a clean workspace.
- **The rename primitive is guarded by the schema, not by a maintained list.**
  `rename_issue` moves an issue and its whole subtree in one transaction,
  deepest-first, under transaction-scoped `PRAGMA defer_foreign_keys` — no
  foreign key declares `ON UPDATE`, so the issues row has to move before the
  rows referencing it. It rewrites nine `(table, column)` pairs, one of which,
  `dependencies.depends_on_id`, carries no foreign key at all (kept that way so
  `external:` references can dangle) and would therefore fail *silently* if
  missed. A test walks the live schema via `PRAGMA foreign_key_list` and fails
  when an ID-bearing column exists that the list does not cover — the same
  approach `tests/licensing.rs` takes by enumerating tracked files rather than
  trusting a list someone has to remember to update. Child counters only ever
  increase, so a vacated child number is never reissued to a different issue.


### Two sync fixes that predate this release

The rename work needed an audit of the merge path, and it turned up two defects
that have been there since 1.0.0 and have nothing to do with hierarchy.

- **`br sync` no longer drops an incoming `agent_context`.** `sync_equals` —
  the comparison behind the three-way merge — never looked at the field, so an
  incoming record whose only delta was `agent_context` read as identical to the
  stored issue and was short-circuited to the local copy. The value was
  exported faithfully and discarded on the way back in.
- **A clock-skewed peer can no longer make a local write invisible.** An issue
  imported from a machine whose clock runs ahead carries an `updated_at` in the
  future. Updating that row wrote a bare `Utc::now()`, which sits *before* the
  stored value and moves the column backwards — and since the import decides
  what to do purely on `updated_at`, the local edit was then skipped as stale
  by every subsequent import. Writes that already have the row in hand now
  clamp to strictly greater than the stored value. The remaining unclamped
  writes (dependencies, labels, comments, bulk label updates) would each need a
  new per-row read, and two of them update many rows in one statement, so they
  are a recorded follow-up rather than a bundled guess.

### Features

- [28c48b6](https://github.com/Toshik1978/beads/commit/28c48b6988f5d39a8cea9e55a4ce5f183a5679aa) feat(storage): add descendant_ids subtree query
- [96b0dea](https://github.com/Toshik1978/beads/commit/96b0dea5b3a064d0fdd0fc1328827d548b9274fc) feat(storage): add rename_issue with subtree cascade
- [899225f](https://github.com/Toshik1978/beads/commit/899225f70c65d797e7b1e9766c55290e94f9acec) feat(model): add former_ids to issues
- [0940c66](https://github.com/Toshik1978/beads/commit/0940c660bf688aa9d6fcc46c0731f4b09cb94a11) feat(storage): record former_ids and tombstone the vacated id on rename
- [fe46e61](https://github.com/Toshik1978/beads/commit/fe46e61a3c065f44d5ba201ba93833de6413775d) feat(resolver): resolve former ids to the issue that now holds them
- [d064d83](https://github.com/Toshik1978/beads/commit/d064d83a4b82570e38a6409f8d8a216052769e89) feat(cli): add br detach
- [75ac85f](https://github.com/Toshik1978/beads/commit/75ac85f90d03928e9f2de9985278fab1438a3d57) feat(info): report hierarchy divergence in --projections

### Bug Fixes

- [1f5c3b0](https://github.com/Toshik1978/beads/commit/1f5c3b0c7426d8b47c01cdd0f3611b2b4e409e96) fix(storage): invalidate blocked cache on rename, add rollback coverage
- [bf1b4cf](https://github.com/Toshik1978/beads/commit/bf1b4cf554da73fc6842d62da09cec03f54bbae7) fix(storage): narrow schema-introspection accessor and verify the FK-exempt column
- [f831d8e](https://github.com/Toshik1978/beads/commit/f831d8e5cdfe2fc5663f3f2ebe4f2f4f57b1b4f0) fix(storage): null the tombstone's external_ref and share the issue-insert binder
- [a768c05](https://github.com/Toshik1978/beads/commit/a768c05776c21f6922de6a26f89f7c18b32126c0) fix(cli): preserve blocked-cache and no-db flush on mid-batch detach failure
- [d0000b1](https://github.com/Toshik1978/beads/commit/d0000b16b6b1a58f66db237f6c80e1d1336bf12c) fix(update): renumber on reparent and refuse to clear a dotted parent
- [07de210](https://github.com/Toshik1978/beads/commit/07de2101bd42328ae692a31609d38c6713705443) fix(storage): make attach_to_parent atomic and bump the target's child counter
- [c672aad](https://github.com/Toshik1978/beads/commit/c672aadd9ab9222b696c92a931d179df77902896) fix(dep): apply the hierarchy invariant to dep add and dep remove
- [829666e](https://github.com/Toshik1978/beads/commit/829666e913fed950414a4bbaca7a8805a6975999) fix(dep): reject metadata on parent-child adds, retest detach on flat children
- [734dd4a](https://github.com/Toshik1978/beads/commit/734dd4a53e09d7370cf17c0fc98d726e7ca110c8) fix(create): close the dep-import and create --dep hierarchy chokepoints
- [008a62d](https://github.com/Toshik1978/beads/commit/008a62d4a0b821ad851b8f5f673e43c2f02d3fbf) fix(dep): resolve depends_on_id per row in a parent-child import batch
- [c353950](https://github.com/Toshik1978/beads/commit/c353950c41daa312cf1845525ad1e3746cfd20eb) fix(storage): give every renamed node its own tombstone and former id
- [29f7032](https://github.com/Toshik1978/beads/commit/29f7032a6dd75113f94440fb646861624fa3c643) fix(storage): stop re-tombstoning descendants already tombstoned by an earlier rename
- [f389150](https://github.com/Toshik1978/beads/commit/f389150223cce2a17d60648cfbab4620916b027a) fix(storage): make attach_to_parent's no-op guard depth-aware and restore clear collision errors
- [fcc0889](https://github.com/Toshik1978/beads/commit/fcc0889f559fdba2278e21de82fd80f464c21e1b) fix(import): renumber Parent:-field children through attach_to_parent
- [943cad2](https://github.com/Toshik1978/beads/commit/943cad2907c9724ac8206bd97502060701d27ad8) fix(detach): reject a resolved-to-tombstone target instead of renaming it
- [46f7bf4](https://github.com/Toshik1978/beads/commit/46f7bf475eb0fc7948c4c192a4c3ba0e0c74e913) fix(detach): route across workspaces like other mutating commands
- [49a43fc](https://github.com/Toshik1978/beads/commit/49a43fce8d60556457655487ac5f6531f7f6b252) fix(sync): compare former_ids in sync_equals
- [7aab4dc](https://github.com/Toshik1978/beads/commit/7aab4dc56326ae7ca39e197519c29a0b30342d4c) fix(rename): bump updated_at when former_ids changes
- [de7d185](https://github.com/Toshik1978/beads/commit/de7d185ac2bd1b0f9898697ecea088df0823bb87) fix(sync): compare agent_context in sync_equals
- [497d6f1](https://github.com/Toshik1978/beads/commit/497d6f14749d8a49284cb619f22d3aa324ba6e67) fix(storage): clamp updated_at against clock-skewed future timestamps
- [57d69e8](https://github.com/Toshik1978/beads/commit/57d69e8a94f8de79552f622551fef76a1b483216) fix(hierarchy): apply final review findings from the dot-notation epic
- [5427f8f](https://github.com/Toshik1978/beads/commit/5427f8f00d6c87471e5baca7ecf3e73de8d71f71) fix(resolver): make a former-id collision resolve the same way every time

### Documentation

- [8bfa3db](https://github.com/Toshik1978/beads/commit/8bfa3db360a394cc8fb50fd96fc4b6508b564349) docs(spec): make the hierarchical id authoritative, add br detach
- [3c019fb](https://github.com/Toshik1978/beads/commit/3c019fb2aa7e61c8e9d65d7475b9dcb5f122aa13) docs(cli): document br detach across the reference and agent surfaces
- [0336873](https://github.com/Toshik1978/beads/commit/03368738bd25385add8d8b79222bbe03c1562e59) docs(health): state that this module does not back a br health command

---

## v1.3.0 — 2026-08-03

`--sort` grows from a single field into a chained specification, and three
`--json` commands stop misreporting how much they returned. The sort work is
the bulk of it: seven sortable fields instead of four, a direction marker per
key, and one parsed spec driving both the SQL `ORDER BY` and the in-memory
comparator so the two orderings cannot disagree. The truncation fix is small
and breaking — see below.

### Why this is 1.3.0 and not 2.0.0

The same reasoning as v1.1.0: the major version tracks the shape of the tool,
and the **Breaking Changes** list is the contract. This release leans on that
harder than the last one did, because what changes here is a machine-readable
shape rather than a redundant spelling, so the blast radius is worth stating
plainly.

Three commands' `--json` output goes from a bare array to an object. Anything
piping them through `jq` breaks, and breaks loudly rather than silently:

```sh
br search widget --json | jq -r '.[].id'        # before
br search widget --json | jq -r '.issues[].id'  # after
```

`br list --json` has emitted that object since 1.0.0, so a script already
handling `list` needs no new shape for the other three. `br stale` and
`br show` still return bare arrays and are not changing: neither takes a
`--limit`, so neither can drop a row without saying so.

### Highlights

- **`--sort` takes more than one key.** `--sort priority,status,-updated`
  orders by each key in turn, with `-` forcing descending and `+` forcing
  ascending per key. `status`, `type` and `assignee` are newly sortable,
  joining `priority`, `created_at`, `updated_at` and `title`. `status` and
  `type` order by workflow rank rather than alphabetically — open before
  blocked before closed, not blocked before closed before open — and
  `assignee` puts unassigned last in both directions. Every sort ends with an
  implicit `id` tiebreaker, so a page is deterministic even when every named
  key ties. Bare `--sort priority` still means `priority,created`, exactly as
  it did before.
- **The two orderings are rendered from one spec.** `br list` sorts in SQL, or
  in memory when a client-side filter has already run, and those used to be
  independent implementations of the same intent. Both now read one
  `SortSpec::resolved`, and a property test asserts they produce the same
  sequence for the same spec across generated issues — which is the only
  reason a seven-field grammar with per-key directions is safe to offer at
  all.
- **`search`, `blocked` and `ready` `--json` now report truncation.** All three
  could return fewer issues than matched and say nothing about it: `search` and
  `blocked` cap at 50 by default, and `ready --limit N` truncates on request.
  A consumer could not tell 50 matches from 50 of 500. They now emit the
  envelope `list` already used — `{"issues": […], "total", "limit",
  "offset", "has_more"}` — with a truthful `total` counted before the cap
  applied. The caps themselves are unchanged; only the reporting is. Read
  `has_more`, not the array length: a full page is not evidence that the result
  set ended there.
- **A guessable bad `--priority` now names the value it inferred.**
  `br create x --priority high` answers `Did you mean --priority 1?` instead of
  reciting the valid range and leaving you to map "high" onto it. The
  detection already existed and its answer was being computed and discarded,
  because the static per-error suggestion was consulted first; specific
  answers now win, with the static text as the fallback when nothing can be
  inferred.
- **Sorting a large result set is roughly 2.5× faster.** `sort_by` calls its
  comparator O(n log n) times, and the old one re-derived the resolved key
  list and allocated a `Vec` per field — plus a `String` per side for
  `title` and `assignee` — on every call. Sorting 5000 issues turned about
  5000 allocations into about 250000. The resolved spec is now walked once per
  sort: 5000 issues by `priority,status,assignee,title` went from 10.2ms to
  4.0ms.

### One documentation fix worth calling out

`--sort -updated` never worked as the reference wrote it. A leading `-` is
read by clap as the start of another flag rather than as `--sort`'s value, so
the documented example failed for everyone who copied it; the attached form
`--sort=-updated` is required whenever a key beginning with `-` leads the
argument. Later keys (`priority,-updated`) are unaffected.


### ⚠ Breaking Changes

- [6954b4d](https://github.com/Toshik1978/beads/commit/6954b4d6304f555d5cf06f961a54ea88b63c8984) search --json and blocked --json emit an object with an issues field instead of a bare array.
- [1c83e65](https://github.com/Toshik1978/beads/commit/1c83e65d760004cd195688895b315efc715b085a) ready --json emits an object with an issues field instead of a bare array.

### Features

- [4deee93](https://github.com/Toshik1978/beads/commit/4deee931d466a8ed20a570c8c8c6ecee36f92bef) feat(model): rank Status and IssueType for sorting
- [23d7c67](https://github.com/Toshik1978/beads/commit/23d7c6741f223820f165dd069ef2d7a1e18da327) feat(model): add SortSpec parsing and resolution
- [1e57974](https://github.com/Toshik1978/beads/commit/1e57974aac7c219fe9c93d11bfce40373085902e) feat(model): render SortSpec as a SQL ORDER BY clause
- [8b0bd19](https://github.com/Toshik1978/beads/commit/8b0bd195139f20a678c259ad0294f904055b40be) feat(model): add the in-memory SortSpec comparator
- [7b4425a](https://github.com/Toshik1978/beads/commit/7b4425a3ea9712c94dea97eaccd3bbeab6c49fa4) feat(storage): order queries by a parsed multi-key sort spec
- [6954b4d](https://github.com/Toshik1978/beads/commit/6954b4d6304f555d5cf06f961a54ea88b63c8984) feat(cli)!: report truncation in search and blocked --json
- [1c83e65](https://github.com/Toshik1978/beads/commit/1c83e65d760004cd195688895b315efc715b085a) feat(ready)!: report truncation in the --json envelope

### Bug Fixes

- [62cce85](https://github.com/Toshik1978/beads/commit/62cce852ad8c5c80e1f7fb7e86524ea9daf383e5) fix(cli): fix a broken --sort example and close a parse/ALL drift hole
- [7de347d](https://github.com/Toshik1978/beads/commit/7de347df076917ba065e843d69f6ecf30d158c88) fix(errors): prefer the detected value over the static suggestion

### Performance

- [bd9ef45](https://github.com/Toshik1978/beads/commit/bd9ef45b7f03915dbaf60760b0965143ef37fcd9) perf(sort): resolve the sort spec once per sort, not per comparison

### Documentation

- [be3f246](https://github.com/Toshik1978/beads/commit/be3f246383a128ce859e396168d0481ba33479a8) docs(cli): document the multi-key sort grammar
- [76783f9](https://github.com/Toshik1978/beads/commit/76783f9677f51090f1198facba944a8e98d2aeae) docs(cli): fix --sort=-updated example claiming oldest-first
- [9d5d593](https://github.com/Toshik1978/beads/commit/9d5d593ec62acb31f689ba03576a368f42ac7eec) docs: correct the stale full-suite test count in CLAUDE.md
- [926257d](https://github.com/Toshik1978/beads/commit/926257da764b139ace1fc5e2ed8b1983d1a2800b) docs(sort): stop claiming --reverse flips every key

### Others

- [38fe37d](https://github.com/Toshik1978/beads/commit/38fe37d51b1da7e99054812f4b8739d1e25bef4e) refactor(cli): sort in memory through the shared sort spec
- [6a40779](https://github.com/Toshik1978/beads/commit/6a40779101f93418c8a18183bf2b782643d3d387) refactor(storage): share the search WHERE clause between query and count

---

## v1.2.0 — 2026-08-01

Mostly a subtraction release: about 8,400 lines net leave the tree, and almost
none of it was code you could reach. A per-mutation audit log that no clone
could ever read, a flag that parsed a value and ignored it, arguments accepted
and discarded, and several algorithms the tree carried two copies of — in more
than one case with the tested copy and the shipped copy having quietly
diverged. Two real fixes and one completion speedup ride along.

### Highlights

- **A mistyped issue ID now suggests the right one.** `br show bd-abc21` on a
  workspace holding `bd-abc12` answers "Did you mean `bd-abc12`?" instead of a
  bare "Issue not found". The bound is a single edit and the distance is
  Damerau, so a transposed pair — a common way to mistype an opaque ID — costs
  one rather than two. Deliberately tight: beads IDs are a shared prefix plus a
  short random suffix, so a looser bound suggests strangers.
- **`br completions <shell> -o <file>` now says what to do with the file.** The
  per-shell install guidance existed and nothing called it, so writing a
  completion script to disk gave you a file and no next step. It goes to
  stderr, so a redirected script stays clean, and `--quiet` suppresses it.
- **`br config get <TAB>` no longer copies your database.** It used to copy the
  whole SQLite family — `.db`, `-wal` and `-shm` — into a temp directory and
  scan the copy, synchronously, between your keystroke and the candidates.
  Config keys come from the YAML layers and the environment; the completion
  never needed a database at all.
- **`br sync --status --json` now reports damage rather than ordinary state.**
  A workspace with an import or flush merely pending used to report
  `"workspace_health": "degraded"` — which is the normal mid-session condition
  of every workspace, described as breakage. Health now moves only for on-disk
  conditions worth acting on, the most important being conflict markers in
  `issues.jsonl`, since that is the file you commit.
- **The events subsystem is gone.** It cost a write on every mutation to
  maintain a log that was never durable: events never reached
  `issues.jsonl`, and `.beads/*.db` is a derived, gitignored cache. So the log
  neither travelled between clones nor survived a rebuild. Its one consumer was
  a close-policy gate that defaulted to off and degraded silently when the
  history it needed was missing.
- **`br sync --orphans` never did anything.** It accepted `strict`,
  `resurrect`, `skip` or `allow`, wrote the choice into a config field, and the
  import engine never read it. Removed rather than implemented — see
  **Breaking Changes**.

### What the sweep found

The bulk of the diff is unreachable code, and the two patterns worth naming
generalise beyond this project.

The first is the **dead duplicate**: a second copy of live logic, living in a
different module, invisible to a search for the name. There were several —
two `Theme`s, two `TreeNode`s, two `should_show_progress`es, two path guards,
two cycle detectors, two ID resolvers, two export loops. In two cases the copy
that had tests was the copy that did *not* ship: the export-policy tests drove
a traversal missing tombstone expiry and parallel preparation, and a dependency
validator re-ran a cycle check *outside* the transaction that the live path
deliberately runs *inside* to close a race.

The second is the **argument accepted and discarded**. `--orphans` is the
user-visible instance. Internally, every storage mutation passed an `actor`
into a transaction that dropped it, which made the parameter inert for eleven
methods that took it only to forward. A consequence worth stating plainly:
label and dependency edits record no actor today. Removing the argument does
not change that — it stops the signature claiming otherwise.

### Upgrading

The database migrates itself on first open (schema 17, which drops the `events`
table and three `close_metadata` columns). Nothing else is automatic:

- **`policy.yaml` must not set `forbid_self_close_after_in_progress` or
  `attribution`.** The close-policy structs reject unknown fields, so a config
  carrying either now fails to parse with an error naming the key. Delete the
  keys. The gate defaulted to off, so removing it changes no behaviour you had.
- **Drop `--agent-name`, `--harness` and `--model`** from any script calling
  `br create`, `update`, `close` or `reopen`, and unset `BR_AGENT_NAME`,
  `BR_HARNESS` and `BR_MODEL`. Their only sink was the events table.
- **`--json` consumers reading `reliability_audit`** should read the flat
  `anomalies` array instead. The codes inside it are unchanged. `health`,
  `anomaly_count` and `source` are gone as redundant; `workspace_health` and
  `anomalies | length` give the first two, and the third was a constant.
- **`br delete --json` no longer reports `events_removed`.**

There is no JSONL change. `issues.jsonl` never carried events or attribution,
so a tracker's committed history is untouched and older `br` binaries still
read files written by this one.

### Why this is 1.2.0 and not 2.0.0

For the reason [v1.1.0](#v110--2026-08-01) gives: `br` is a personal tool, the
command surface is not an interface under semver, and the **Breaking Changes**
list is the contract rather than the version number. Everything removed here
was either unreachable, undurable, or actively lying about what it did.

### ⚠ Breaking Changes

- [3fd4030](https://github.com/Toshik1978/beads/commit/3fd40305400cfa73905d6160f97aad309d1d66ca) `--agent-name`, `--harness` and `--model` are gone from `br create`, `br update`, `br close` and `br reopen`, as are the `BR_AGENT_NAME`, `BR_HARNESS` and `BR_MODEL` environment variables. A `policy.yaml` that sets `forbid_self_close_after_in_progress` or `attribution` now fails to parse with an unknown-field error naming the key, since the close-policy structs deny unknown fields. `br delete --json` no longer reports `events_removed`, and `close_metadata` loses `closed_by_agent_name`, `closed_by_harness` and `closed_by_model`. The schema moves to 17, dropping the `events` table and those three columns.
- [1a8c9d4](https://github.com/Toshik1978/beads/commit/1a8c9d4a6267acde11b2a5f8bab5e4d2b8aaa7e7) `br sync --status --json` replaces the `reliability_audit` object with a flat `anomalies` array, dropping its `source`, `health` and `anomaly_count` fields -- the first was always the same literal, the other two duplicated the sibling `workspace_health` and the array length. `workspace_health` also no longer degrades when an import or flush is merely pending; `jsonl_newer` and `db_newer` stay in the same payload as the booleans they always were.
- [dece3f7](https://github.com/Toshik1978/beads/commit/dece3f74c5abb9c5b27c4def9afd6c1cac5020b1) `br sync --orphans` is gone. It parsed into an `ImportConfig` field that the import engine never read, so every value it accepted was silently ignored; there is no behaviour to migrate to, only a flag that no longer pretends.

### Features

- [3fd4030](https://github.com/Toshik1978/beads/commit/3fd40305400cfa73905d6160f97aad309d1d66ca) feat(storage)!: delete the events subsystem and Tier 1 attribution

### Bug Fixes

- [a839a33](https://github.com/Toshik1978/beads/commit/a839a3300d520a3d9acb953ea4d82f4f773901a7) fix(cli): print completion install instructions when writing to a file
- [ef9f2b6](https://github.com/Toshik1978/beads/commit/ef9f2b68b9b71e2cc8c8072c9e9f9573e0d0913c) fix(cli): suggest a near-miss ID when a lookup fails

### Performance

- [4be4733](https://github.com/Toshik1978/beads/commit/4be4733a34f813968069872bc480a4f4215395b5) perf(cli): stop copying the database to complete a config key

### Documentation

- [cdf466e](https://github.com/Toshik1978/beads/commit/cdf466eeb763fc3f90f85bf6edfbb743c9b5952a) docs(sync): correct the Case 1 label in the 3-way merge table
- [df024ac](https://github.com/Toshik1978/beads/commit/df024ac8859970f23ef0ed78bac815a8433f1e94) docs(build): separate the declared toolchain floor from the measured minimum

### Others

- [1a8c9d4](https://github.com/Toshik1978/beads/commit/1a8c9d4a6267acde11b2a5f8bab5e4d2b8aaa7e7) refactor(health)!: keep only the anomalies something actually detects
- [96c3bdb](https://github.com/Toshik1978/beads/commit/96c3bdb4d704b8bfe16e963040982a19aa090a4b) refactor: sweep the unreachable code out of format, output, storage and validation
- [dece3f7](https://github.com/Toshik1978/beads/commit/dece3f74c5abb9c5b27c4def9afd6c1cac5020b1) refactor(sync)!: drop the export preflight and the inert --orphans flag
- [85d2c47](https://github.com/Toshik1978/beads/commit/85d2c47953eb1d1d539dc205d3da6616fdc29285) refactor(util): collapse the duplicated ID resolver (bds-ves)
- [961f138](https://github.com/Toshik1978/beads/commit/961f138bb1be92c0bc598af1e4662fdd2ab30e6c) refactor: sweep the second round of unreachable code from storage and the CLI

---

## v1.1.1 — 2026-08-01

One fix, worth the detail: `br` no longer writes the absolute path of your
workspace into every issue it creates.

### Highlights

- **Issues no longer name the machine they were created on.** `br create` and
  the markdown import stamped `source_repo_path` — the canonicalized path of
  the directory containing `.beads/` — onto each new issue, and that value
  reached `.beads/issues.jsonl`, which projects commit. Every issue therefore
  published the author's directory layout. Nothing read the field back, and
  because `sync_equals` compared it, two clones of one repository at different
  paths also disagreed on every record and produced diffs saying nothing.
- **`source_repo` is unchanged.** The repository *basename* is still stamped on
  new issues. It identifies the repository without naming the machine, and it
  is the value cross-repo tooling was reading anyway.
- **The field still exists and can still be set.** `br update <id>
  --source-repo-path <path>` writes it explicitly and it round-trips through
  the JSONL, so the cross-clone disambiguation it was added for stays available
  to anyone who wants it. There is no schema migration — the column remains,
  nullable, and every existing database keeps working.

### Upgrading a repository that already has paths in it

Nothing is cleaned up for you. Issues created before this release keep their
stored path, and `br` keeps re-exporting it.

Clearing it per issue is supported and does what you would expect:

```sh
br update <id> --source-repo-path ''
```

For a whole tracker, strip the key from every record and rebuild:

```sh
python3 - <<'EOF'
import json
path = '.beads/issues.jsonl'
records = [json.loads(line) for line in open(path) if line.strip()]
for record in records:
    record.pop('source_repo_path', None)
with open(path, 'w') as out:
    for record in records:
        out.write(json.dumps(record, separators=(',', ':'), ensure_ascii=False) + '\n')
EOF
br sync --import-only --rebuild
```

**The `--rebuild` is the part that matters.** A plain `br sync --import-only`
treats a field missing from a record as "leave the stored value alone", reports
the record as up-to-date, and the next write to that issue exports the old path
straight back into the file. `--rebuild` reconstructs the database from the
JSONL instead, so an omitted field really is cleared; dependencies, comments,
tombstones and every other field are preserved, and it writes a verified backup
under `.beads/.br_recovery/` first. Flushing the other way (database to JSONL)
is not a fix at all — the stored paths are exactly what it would write back.

That `--import-only` behaviour is a separate bug and is not changed by this
release.

Note that this cleans the working tree only. Paths already committed remain in
the repository's history.

### Why this is a patch release

`source_repo_path` disappears from `br --json` and from newly written JSONL
records, which is a visible change to a machine-readable surface. It is still a
patch: the field is optional and omitted when unset, and `Issue`'s own
documentation has always recorded that databases and hand-edited records
lacking it are valid. A consumer that required the key was already broken
against every issue predating the field.

### Bug Fixes

- [3b6fcba](https://github.com/Toshik1978/beads/commit/3b6fcbaf817f7fc12fee3a77b8e2194f10d4b029) fix(cli): stop stamping an absolute workspace path onto every issue

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

