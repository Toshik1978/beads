# br CLI Reference

Comprehensive reference for all `br` commands.

---

## Table of Contents

- [Global Options](#global-options)
- [Cross-Project Routing](#cross-project-routing)
- [Core Commands](#core-commands)
  - [init](#init)
  - [create](#create)
  - [list](#list)
  - [show](#show)
  - [update](#update)
  - [close](#close)
  - [reopen](#reopen)
  - [delete](#delete)
- [Query Commands](#query-commands)
  - [ready](#ready)
  - [blocked](#blocked)
  - [search](#search)
  - [stale](#stale)
- [Organization Commands](#organization-commands)
  - [dep](#dep)
  - [label](#label)
  - [epic](#epic)
  - [detach](#detach)
  - [rename](#rename)
  - [statuses](#statuses)
  - [types](#types)
  - [comments](#comments)
- [Workflow Commands](#workflow-commands)
  - [defer / undefer](#defer--undefer)
- [Sync & Config](#sync--config)
  - [sync](#sync)
  - [config](#config)
  - [remote](#remote)
- [Diagnostics & Info](#diagnostics--info)
  - [stats](#stats)
  - [info](#info)
  - [version](#version)
  - [history](#history)
- [Utilities](#utilities)
  - [completions](#completions)
- [Exit Codes](#exit-codes)
- [Environment Variables](#environment-variables)
- [JSON Output Schemas](#json-output-schemas)

---

## Global Options

These options apply to all commands:

| Option | Description |
|--------|-------------|
| `--db <PATH>` | Database path (auto-discover `.beads/*.db` if not set) |
| `--actor <NAME>` | Actor name for audit trail |
| `--json` | Output as JSON (machine-readable) |
| `--no-auto-flush` | Skip automatic JSONL export after mutations |
| `--no-auto-import` | Skip automatic import check |
| `--allow-stale` | Allow stale DB (bypass freshness check warning) |
| `--lock-timeout <LOCK_TIMEOUT>` | SQLite busy/write-lock timeout in milliseconds |
| `--no-db` | JSONL-only mode (no DB connection) |
| `-v, --verbose` | Increase logging verbosity (-v, -vv) |
| `-q, --quiet` | Quiet mode (errors only) |
| `--no-color` | Disable colored output |
| `-h, --help` | Print help |
| `-V, --version` | Print version |

By default, successful mutating commands auto-flush SQLite changes to
`.beads/issues.jsonl`, so the JSONL file is normally ready to stage after the
command completes. Use `--no-auto-flush` to skip that export for a single
command. `br sync --flush-only` remains useful as an idempotent final export
check before committing, after `--no-auto-flush`, after disabling auto-flush in
config, or during recovery.

---

## Cross-Project Routing

`br` can route explicit issue IDs to another workspace when their prefix matches
`.beads/routes.jsonl`. This is useful for town or multi-repository setups where
one project needs to inspect or update an issue owned by another project.

Each route is one JSON object per line:

```jsonl
{"prefix":"api-","path":"../api"}
{"prefix":"ops-","path":"/srv/projects/ops/.beads"}
```

Route resolution:

1. Extract the issue prefix before the final hyphen, including the hyphen, so
   hyphenated prefixes such as `document-intelligence-` route correctly.
2. Search the local `.beads/routes.jsonl`.
3. If a parent town root with `mayor/town.json` exists, search its
   `.beads/routes.jsonl`.
4. Resolve `path` as a project root or a direct `.beads`/`_beads` directory.
5. Follow a target `.beads/redirect` file when present.

Current route-aware commands include common issue-ID operations such as `show`,
`update`, `close`, `reopen`, `delete`, `comments`, `label`, and `dep`.
Routed write operations acquire the target
workspace's `.write.lock` and mutate the target workspace, not the caller's
local database.

Safety boundaries:

- Routing never runs git, copies repositories, or performs network sync.
- Routing is not real-time collaboration; each affected repository still needs
  its own normal `br sync --flush-only`/VCS commit flow.
- Routes are prefix dispatch rules. They do not import external issues into the
  local database.
- Cross-project dependency status checks use explicit IDs such as
  `external:api:api-123` plus config keys like `external_projects.api=../api`.

**A routed fan-out is not atomic across routes, and says so.** Each route is a
separate database and a separate transaction, and every route-aware command
opens, mutates and finalizes one workspace before it opens the next — so once a
route has committed, nothing can undo it. A later route can still fail: because
its IDs do not resolve there, because a workflow or capacity rule rejects it, or
because a commit-time guard such as `update --if-status` does not hold.

When that happens the command exits **3** with error code `PARTIALLY_APPLIED`
instead of surfacing the bare cause, whose message would name only the target
that failed and read as though the whole command had been refused. This applies
to `update`, `close`, `reopen`, `delete`, `label add`/`remove`, and `detach`;
`show` is read-only and `comments`/`dep`/`rename` act on a single route.

Its `context` splits every target three ways:

| Field | Meaning |
| --- | --- |
| `applied` | Written. That route committed and cannot be rolled back. A route that succeeded without writing anything — every issue already closed, every detach a no-op — is *not* listed here. |
| `uncertain` | The failing route's targets, when it had already written something before failing. A route is atomic in its primary write but not across the follow-up steps (labels, re-parenting, per-ID deletes), so these may be partly updated. |
| `not_applied` | Untouched — the route was never reached, it failed before writing anything, or it ran and changed nothing. |

Targets are listed **as you supplied them**, not canonicalised: a route that was
never reached was never opened, so its IDs were never resolved, and one field
cannot mean two things.

`context.cause_code` carries the code the failure would have had on its own
(`PRECONDITION_FAILED` for a rejected `--if-status`, `ISSUE_NOT_FOUND` for an ID
that does not exist in the target workspace), so a caller can still branch on
*why* the fan-out stopped after branching on the fact that it stopped halfway.
`retryable` is `false`: re-running the same command can trip on the targets that
already moved. Re-run against `not_applied` instead.

Nothing partial is reported when no route wrote anything — the cause is
surfaced unchanged, exactly as an unrouted command would surface it.

---

## Core Commands

### init

Initialize a beads workspace in the current directory.

```bash
br init [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--prefix <PREFIX>` | Issue ID prefix (e.g., "bd", "proj") |
| `--force` | Overwrite existing database |
| `--backend <BACKEND>` | Backend type placeholder; currently ignored and always uses SQLite |

**Examples:**
```bash
# Initialize with default prefix
br init

# Initialize with custom prefix
br init --prefix myproj

# Force reinitialize
br init --force
```

---

### create

Create a new issue.

```bash
br create [OPTIONS] [TITLE]
```

**Arguments:**
- `TITLE` - Issue title (can also use `--title-flag`)

**Options:**
| Option | Description |
|--------|-------------|
| `-t, --type <TYPE>` | Issue type (task, bug, feature, epic, chore, docs, question) |
| `-p, --priority <PRIORITY>` | Priority (0-4 or P0-P4, where 0=critical) |
| `-d, --description <TEXT>` | Issue description |
| `--notes <TEXT>` | Additional notes |
| `--acceptance-criteria <TEXT>` | Acceptance criteria (alias: `--acceptance`) |
| `--owner <EMAIL>` | Set owner email |
| `-l, --labels <LABELS>` | Labels (comma-separated) |
| `--parent <ID>` | Parent issue ID (creates parent-child dependency) |
| `--deps <DEPS>` | Dependencies (format: `type:id,type:id`) |
| `--defer <DATE>` | Defer until date |
| `--external-ref <REF>` | External reference (e.g., `gh-123`) |
| `-s, --status <STATUS>` | Initial status (`open`, `deferred`, `in_progress`, `closed`) |
| `--dry-run` | Preview without creating |
| `--silent` | Output only issue ID |
| `-f, --file <PATH>` | Create issues from markdown file (bulk import) |

**Examples:**
```bash
# Simple task
br create "Fix login bug"

# High-priority bug with details
br create "Critical security issue" -t bug -p 0 -d "XSS vulnerability in form input"

# Feature with an owner and labels
br create "Add dark mode" -t feature --owner alice -l "ui,enhancement"

# Bulk import from markdown
br create -f issues.md
```

---

### list

List issues with filtering and sorting.

```bash
br list [OPTIONS]
```

**Filter Options:**
| Option | Description |
|--------|-------------|
| `-s, --status <STATUS>` | Filter by status (can repeat) |
| `-t, --type <TYPE>` | Filter by issue type (can repeat) |
| `--id <ID>` | Filter by specific IDs (can repeat) |
| `-l, --label <LABEL>` | Filter by label (AND logic, can repeat) |
| `--label-any <LABEL>` | Filter by label (OR logic, can repeat) |
| `-p, --priority <PRIORITY>` | Filter by priority (can repeat) |
| `--priority-min <N>` | Filter by minimum priority |
| `--priority-max <N>` | Filter by maximum priority |
| `--title-contains <TEXT>` | Title contains substring |
| `--desc-contains <TEXT>` | Description contains substring |
| `--notes-contains <TEXT>` | Notes contains substring |
| `-a, --all` | Include closed issues |
| `--deferred` | Include deferred issues |

**Exclusion Options** (also on `br search` and `br ready`):
| Option | Description |
|--------|-------------|
| `--exclude-label <LABEL>` | Exclude issues carrying this label (can repeat) |
| `--exclude-type <TYPE>` | Exclude issues of this type (can repeat) |
| `--no-labels` | Exclude issues that carry any label |
| `--no-parent` | Exclude issues that have a parent |

Repeating an exclusion means **none of these**, not "not all of these" —
`--exclude-label a --exclude-label b` drops anything carrying either. Note that
this is the opposite of `--label`, which ANDs. Exclusions compose with the
positive form of the same field, so `--label urgent --exclude-label wontfix`
narrows twice.

There is no `--no-assignee` or `--unassigned`: `assignee` and its whole query
surface were removed from `Issue`.

`--no-parent` asks about the `parent-child` dependency row, which is where
parenthood is recorded — not about the shape of the ID, which is a consequence of
it.

**Date-Range Options** (also on `br search`):
| Option | Description |
|--------|-------------|
| `--created-after <WHEN>` / `--created-before <WHEN>` | Bound on `created_at` |
| `--updated-after <WHEN>` / `--updated-before <WHEN>` | Bound on `updated_at` |
| `--closed-after <WHEN>` / `--closed-before <WHEN>` | Bound on `closed_at`; implies `--all` |
| `--defer-after <WHEN>` / `--defer-before <WHEN>` | Bound on `defer_until` |

`<WHEN>` accepts a timestamp (`2026-03-01T09:00:00Z`), a bare date
(`2026-03-01`), a relative offset (`-7d`, `+2w`), or `today` / `yesterday` /
`tomorrow` / `next-week`.

**Both ends are inclusive.** A bare date widens to the whole day it names — the
start of it for `--*-after`, the last instant for `--*-before` — so
`--created-after 2026-03-01 --created-before 2026-03-01` means "created on the
1st". A timestamp given explicitly is used exactly as given.

`closed_at` and `defer_until` are nullable, and **a row with no value
in the column never matches a bound on it**: "closed in the last week" does not
include issues that are still open. Because a `closed_at` bound can only ever be
satisfied by a closed issue, `--closed-after`/`--closed-before` turn on `--all`
for you rather than returning an empty list.

Write a backwards offset attached with `=` — `--updated-after=-7d`. With a space
the shell hands `-7d` to clap, which reads it as flags. This is the same
convention `--sort=-updated` and `--defer=-7d` already follow.

```bash
# Everything touched since last Monday, newest first
br list --updated-after=-7d --sort=-updated

# What got closed yesterday
br list --closed-after yesterday --closed-before yesterday

# Deferred work that was filed this quarter
br list --defer-before today --created-after 2026-01-01
```

**Output Options:**
| Option | Description |
|--------|-------------|
| `--limit <N>` | Maximum results (0=unlimited; default: unlimited — the full work surface). Pass `--limit N` to cap. |
| `--sort <KEYS>` | Comma-separated sort keys (see Sorting below) |
| `-r, --reverse` | Reverse sort order (the implicit `id` tiebreaker stays ascending) |
| `--long` | Long output format |
| `--pretty` | Tree/pretty output format |
| `--wrap` | Wrap long lines instead of truncating in text output |
| `--format <FMT>` | Output format: text, json, csv |
| `--fields <FIELDS>` | CSV fields (comma-separated) |

#### Sorting

`--sort` takes one or more keys, applied left to right:

```
--sort <key>[,<key>...]     key := ['-'|'+'] field
```

| Field | Bare direction |
| --- | --- |
| `priority` | ascending — critical first |
| `status` | ascending — open, in_progress, blocked, deferred, draft, closed, tombstone, pinned, then custom |
| `type` | ascending — task, bug, feature, epic, chore, docs, question, then custom |
| `title` | ascending — A-Z, case-insensitive |
| `created_at` (`created`) | descending — newest first |
| `updated_at` (`updated`) | descending — newest first |

`-` forces descending and `+` forces ascending. Every sort ends with an
implicit `id` tiebreaker, so output is deterministic.

`--reverse` flips every key in the effective ordering **except** that `id`
tiebreaker, which is always ascending. That exception is what keeps a reversed
listing deterministic rather than merely mirrored. Note that the effective
ordering includes keys you did not type: bare `--sort priority` expands to
`priority,created` (see below), and `--reverse` flips both of them.

Because `created_at` and `updated_at` are already descending by default,
`-created`/`-updated` change nothing — they force the same direction the
field already sorts in. Use `+created`/`+updated` for oldest-first, or
`--reverse` to invert the whole ordering at once.

A key that *begins* with `-` (a leading key with no other key before it, e.g.
`-updated`) must be attached to `--sort` with `=`: `--sort=-updated`, not
`--sort -updated`. Written as two arguments, the shell hands `-updated` to
clap as the next token, and clap reads its leading `-` as another flag rather
than as `--sort`'s value — the same class of problem `--acceptance-criteria`
and the other free-text flags have with `-`-leading values. A `-` prefix on a
second or later key (`priority,-updated`) does not need `=`, because it no
longer begins the argument.

```bash
br list --sort priority,updated     # critical first, most recent within each band
br list --sort status,priority      # group by workflow state, then priority
br list --sort=+updated             # oldest first
```

`--sort priority` on its own keeps its historical tiebreaker and is
equivalent to `--sort priority,created`.

**Examples:**
```bash
# All open issues
br list

# High-priority bugs
br list -t bug -p 0 -p 1

# Export to CSV
br list --format csv --fields id,title,status,priority > issues.csv

# JSON for scripting
br list --json | jq '.issues[].id'
```

---

### show

Show detailed issue information.

```bash
br show [IDS]...
```

**Options:**
| Option | Description |
|--------|-------------|
| `--format <FMT>` | Output format: text, json |
| `--wrap` | Wrap long lines instead of truncating in text output |

**Examples:**
```bash
# Show single issue
br show bd-abc123

# Show multiple issues
br show bd-abc123 bd-def456

# JSON output
br show bd-abc123 --json
```

---

### update

Update one or more issues.

```bash
br update [OPTIONS] [IDS]...
```

**Options:**
| Option | Description |
|--------|-------------|
| `--title <TEXT>` | Update title |
| `--description <TEXT>` | Update description |
| `--design <TEXT>` | Update design notes |
| `--acceptance-criteria <TEXT>` | Update acceptance criteria |
| `--notes <TEXT>` | Replace additional notes |
| `--append-notes <TEXT>` | Append to additional notes, separated by a blank line. The read-modify-write happens inside the write transaction, so concurrent appends cannot lose each other |
| `--transition-comment <TEXT>` | Add a fresh comment atomically with a status transition |
| `-s, --status <STATUS>` | Change status |
| `-p, --priority <N>` | Change priority |
| `-t, --type <TYPE>` | Change issue type |
| `--owner <EMAIL>` | Set owner (empty string clears) |
| `--if-status <STATUS>` | Compare-and-set guard: apply only while the status is still this |
| `--force` | Force update even if issue is blocked |
| `--defer <DATE>` | Set defer date (empty string clears) |
| `--add-label <LABEL>` | Add label(s) |
| `--remove-label <LABEL>` | Remove label(s) |
| `--set-labels <LABELS>` | Replace all labels |
| `--parent <ID>` | Reparent (empty string removes) |
| `--external-ref <REF>` | Set external reference |

**Examples:**
```bash
# Claim a task (claiming is assigning: move it to in_progress)
br update bd-abc123 -s in_progress

# Update multiple issues
br update bd-abc123 bd-def456 -p 1

# Add labels
br update bd-abc123 --add-label "urgent,reviewed"
```

**Compare-and-set guard.** `--if-status` makes an update conditional on the
status the issue still holds. The guard is evaluated inside the same write
transaction as the update, so two agents racing the same transition produce
exactly one winner — no read-then-write race:

```bash
# Take this only if nobody else has moved it yet
br update bd-abc123 -s in_progress --if-status open
```

When the guard does not hold, nothing is written — not the fields, not
`updated_at`, not a `--transition-comment` — and the command exits **4** with
error code `PRECONDITION_FAILED`, whose `context` names the field, the value
expected and the value found. That exit code is deliberately not `3`: `3` is
`ISSUE_NOT_FOUND`, and a caller retrying a guarded update has to be able to tell
"someone got there first" (re-read and decide) from "there is nothing to update"
(stop).

A guard needs a field update to guard. Label and parent changes are written in
their own transactions and cannot be guarded atomically, so `br update <id>
--add-label x --if-status open` is refused rather than silently unguarded.

"Nothing is written" holds within one workspace. A multi-target update whose IDs
[route](#cross-project-routing) to different workspaces commits each route in
turn, so a guard rejecting a later route cannot undo an earlier one; that case
exits 3 with `PARTIALLY_APPLIED` and names which targets landed. The same
applies to every other route-aware fan-out.

---

### close

Close one or more issues.

```bash
br close [OPTIONS] [IDS]...
```

**Options:**
| Option | Description |
|--------|-------------|
| `-r, --reason <TEXT>` | Close reason |
| `--reason-file <PATH>` | Read the close reason verbatim from a file (or `-` for stdin). Mutually exclusive with `-r/--reason` |
| `--transition-comment <TEXT>` | Add a fresh comment atomically with the close transition |
| `-f, --force` | Close even if blocked by open dependencies |
| `--continue` | Keep going past a per-issue failure, and make the exit code report the outcome (see below) |
| `--suggest-next` | Return newly unblocked issues |

**`--continue` and the exit code.** By default one unresolvable ID fails the whole
command before any issue is touched. `--continue` turns that into a recorded skip
and closes the rest. A workflow policy violation (e.g. a missing required
transition field) still aborts the whole batch either way — `--continue` does not
rescue it.

It also *replaces* the exit-code rule, because a caller who passes it intends to
inspect the outcome:

| | Default | `--continue` |
|---|---|---|
| Nothing closed, some skipped | exit 3, `NOTHING_TO_DO` | exit 3, `PARTIALLY_COMPLETED` |
| Some closed, some not | **exit 0** | exit 3, `PARTIALLY_COMPLETED` |
| Everything already closed | exit 3, `NOTHING_TO_DO` | **exit 0** |

The last row is what makes `--continue` usable in a retry loop: "already closed"
is not a failure, so re-running a batch that half-succeeded exits 0. The default
rule is deliberately left as it was — changing an exit code under existing callers
would be worse than the gap it closes.

The batch write itself is still one transaction. `--continue` decides which issues
enter that transaction; it does not make the write partial.

**Examples:**
```bash
# Close with reason
br close bd-abc123 -r "Completed in PR #42"

# Close multiple
br close bd-abc123 bd-def456 -r "Sprint complete"

# Force close blocked issue
br close bd-abc123 --force

# Close and get next work
br close bd-abc123 --suggest-next --json
```

---

### reopen

Reopen a closed issue.

```bash
br reopen [OPTIONS] [IDS]...
```

**Options:**
| Option | Description |
|--------|-------------|
| `-r, --reason <TEXT>` | Reason for reopening, stored as a comment |

---

### delete

Delete an issue (creates tombstone).

```bash
br delete [OPTIONS] <IDS>...
```

**Options:**
| Option | Description |
|--------|-------------|
| `--reason <TEXT>` | Delete reason (default: `delete`) |
| `--from-file <PATH>` | Read IDs from file (one per line, `#` comments ignored) |
| `--cascade` | Delete dependents recursively |
| `--force` | Bypass dependent checks, orphaning dependents |
| `--hard` | Prune tombstones from JSONL immediately |
| `--dry-run` | Preview only, no changes |

---

## Query Commands

### ready

List issues ready to work on (unblocked, not deferred).

```bash
br ready [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--limit <N>` | Maximum results (0=unlimited; default: unlimited — the full ready set). Pass `--limit N` to cap. |
| `-l, --label <LABEL>` | Filter by label (AND logic) |
| `--label-any <LABEL>` | Filter by label (OR logic) |
| `-t, --type <TYPE>` | Filter by type |
| `-p, --priority <N>` | Filter by priority |
| `--exclude-label <LABEL>` / `--exclude-type <TYPE>` / `--no-labels` / `--no-parent` | The exclusion filters; see `br list` |
| `--sort <POLICY>` | Sort: hybrid (default), priority, oldest |
| `--include-deferred` | Include deferred issues |
| `--parent <ID>` | Filter to children of a parent issue |
| `-r, --recursive` | Include all descendants with `--parent` |
| `--wrap` | Wrap long lines instead of truncating in text output |
| `--format <FMT>` | Output format: text, json |

**Examples:**
```bash
# High-priority ready work
br ready -p 0 -p 1

# JSON for agent integration
br ready --json --limit 10
```

Because `--limit` can truncate, `--json` emits the [paginated
envelope](#paginated-envelope-list-ready-search-blocked): with the default
`--limit 0` it always reports `has_more: false`, and with an explicit cap
`has_more` tells you whether more ready work was left behind.

**Configurable ready status group (`.beads/policy.yaml`):**

By default, `br ready` treats only `open` issues as actionable. Projects with a
review workflow can widen what "ready" means — so review-returned work (e.g.
`rework`) resurfaces through the same `br ready --json` entrypoint instead of
forcing workflow knowledge into every agent prompt:

```yaml
workflow:
  status_groups:
    ready:
      - open
      - rework
```

Semantics:
- **Default:** when `workflow.status_groups.ready` is absent (or empty), the
  group is `[open]` — exactly the pre-#354 behavior (zero change for existing
  repos).
- **Status preserved:** returned issues keep their real status, so a `rework`
  item still emits `{"status":"rework"}` in `--json`.
- **Validation:** when `workflow.strict: true` (and `workflow.statuses` is set),
  every member of the ready group must be in `workflow.statuses`; an
  out-of-vocabulary member is rejected with a clear error. Without `strict`, the
  group is accepted as-is.
- **Deferred interaction:** the `defer_until` time-gate still applies to every
  non-`deferred` member of the group, so a configured member with a future
  `defer_until` stays out of `br ready` until it elapses. `--include-deferred`
  additionally surfaces `deferred` work and drops the time-gate, without
  double-counting `deferred` if it is also listed in the group.
- **Scope:** `br ready` and `br ready --json` both use the same ready group.

**Atomic workflow capacity (`.beads/policy.yaml`):**

Repository-level hard limits and transition-scoped admission guards are
configured under `workflow.capacity`. Every referenced status must be declared
in `workflow.statuses`; unknown fields, zero thresholds, undeclared references,
and a soft threshold greater than its hard threshold fail closed while loading
the policy.

```yaml
workflow:
  statuses: [open, in_progress, in_review, rework, closed]
  capacity:
    statuses:
      in_progress:
        hard: 3
    groups:
      active_work:
        statuses: [in_progress, in_review, rework]
        hard: 5
    admission:
      - name: drain_review_before_starting
        transitions:
          from: [open]
          to: [in_progress]
        require_below:
          statuses:
            in_review: 2
          groups:
            active_work: 5
```

- A hard limit of `N` admits the transition that reaches `N` and rejects a
  transition that would reach `N + 1`.
- Named groups count the union of their configured statuses without duplicate
  members.
- Admission requirements are exclusive: `in_review: 2` requires the
  prospective observed count to remain below 2 for matching transitions.
- Enforcement and mutation share one `BEGIN IMMEDIATE` transaction. Rejections
  therefore cannot race another writer and roll back every field in the update.
- Draining an overfull status/group is always allowed. JSONL import remains a
  state-replication path rather than a new-work admission path.
- Reaching a soft threshold still commits. Human output emits an actionable
  warning; JSON adds a structured `warnings` array only when warnings
  exist, preserving the legacy success shape below the threshold.
- Each warning contains `issue_id`, `from_status`, `to_status`,
  `capacity_kind`, `capacity_name`, `scope`, `counting_mode`, `current`,
  `prospective`, `soft_limit`, optional `hard_limit`, and `policy_path`.
  `update` wraps its normal array as `{updated, warnings}` and `create` as
  `{created, warnings}`; commands that already return an object add `warnings`
  to that object. The wrapper is never introduced below the soft threshold.
- Multi-target `update`, `close`, `reopen`, `defer`, and `undefer`
  commands evaluate the repository's final prospective state and commit all
  status changes in one transaction. Hard-limit and late validation failures
  roll back the entire repository-local batch; capacity-neutral swaps do not
  depend on request order.
- Routed commands transact each repository independently. There is no
  distributed transaction across repositories, so an earlier route may already
  be committed if a later route fails and cross-repository atomicity is
  intentionally not claimed. Every route-aware fan-out reports that case rather
  than leaving it to be inferred; see `PARTIALLY_APPLIED` under
  [Cross-Project Routing](#cross-project-routing).
- Omitting `workflow.capacity` preserves existing behavior exactly.
- The current enforcement layer has fixed `repository` scope and `all`
  counting. Hierarchy-aware counting, audited exemptions, actor/harness/
  session/subtree scopes, and capacity observability are later phases
  tracked in GitHub issue #384.

---

### blocked

List blocked issues.

```bash
br blocked [OPTIONS]
```

Shows issues that are blocked by other open issues.

**Options:**
| Option | Description |
|--------|-------------|
| `--limit <N>` | Maximum results (default: 50, 0=unlimited) |
| `--detailed` | Include full blocker details in text output |
| `--wrap` | Wrap long lines instead of truncating in text output |
| `-t, --type <TYPE>` | Filter by type |
| `-p, --priority <N>` | Filter by priority |
| `-l, --label <LABEL>` | Filter by label |
| `--format <FMT>` | Output format: text, json |

Because it can truncate, `--json` emits the [paginated
envelope](#paginated-envelope-list-ready-search-blocked): check `has_more` to learn
whether blocked issues were dropped by the cap.

---

### search

Full-text search across issues.

```bash
br search <QUERY> [OPTIONS]
```

Supports all filter options from `list`. Unlike `list`/`ready` (which are
complete by default), `search` results are **capped at 50 by default**
(`--limit <N>`, `0`=unlimited) — a broad text query can match a large fraction
of the corpus, so a bounded, relevance-ordered result set is the default.

Because it can truncate, `--json` emits the [paginated
envelope](#paginated-envelope-list-ready-search-blocked): check `has_more` to learn
whether matches were dropped.

**Examples:**
```bash
# Search in all fields
br search "authentication"

# Search with filters
br search "bug" -t bug -p 0
```

---

### stale

List stale issues (not updated recently).

```bash
br stale [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--days <N>` | Issues not updated in N days (default: 30) |
| `--status <STATUS>` | Filter by status (repeatable or comma-separated) |
| `--limit <N>` | Maximum results (0 = unlimited, the default). Output is stalest-first, so a limit keeps the worst offenders |

**Abandoned in-progress work:**

`br ready` does not show `in_progress` issues. To audit hidden work, combine
`stale` with an explicit in-progress listing and inspect the evidence:

```bash
br stale --days 1 --json
br list --status in_progress --json
br show <id> --json
br comments list <id> --json
```

An `in_progress` issue is a reclaim candidate when `updated_at` is old and
recent comments or Agent Mail reservations do not show live work. Default
thresholds are two hours for automated swarm claims and one business day for
human or unclear claims.

Before reclaiming, add an audit comment with the evidence, then move it back:

```bash
br comments add <id> --author "$BD_ACTOR" \
  --message "reclaim: previous in_progress work appears abandoned; evidence: updated_at=<timestamp>, no active reservation or pane" \
  --json
br update <id> --status open --json
```

There is not a separate reclaim command; the audit comment plus `update
--status open` is the documented recovery workflow.

---

## Organization Commands

### dep

Manage dependencies between issues.

```bash
br dep <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `add <ISSUE> <DEPENDS_ON>` | Add dependency (ISSUE depends on DEPENDS_ON) |
| `remove <ISSUE> <DEPENDS_ON>` | Remove dependency |
| `list <ISSUE>` | List dependencies of an issue |
| `tree <ISSUE>` | Show dependency tree |
| `cycles` | Detect dependency cycles |

**Dependency Types:**
- `blocks` (default) - Target blocks source
- `parent-child` - Hierarchical relationship
- `discovered-from` - Discovered during work on another issue
- `related` - Loosely related issues

**Examples:**
```bash
# Add blocking dependency
br dep add bd-123 bd-456  # bd-123 is blocked by bd-456

# Add with type
br dep add bd-123 bd-456 --type discovered-from

# Show tree
br dep tree bd-123

# Check for cycles
br dep cycles
```

---

### label

Manage labels on issues.

```bash
br label <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `add [ISSUES]... --label <LABEL>` | Add a label to one or more issues |
| `remove [ISSUES]... --label <LABEL>` | Remove a label from one or more issues |
| `list [ID]` | List labels (optionally for specific issue) |
| `list-all` | List all unique labels with counts |
| `rename <OLD_NAME> <NEW_NAME>` | Rename a label across all issues |

---

### epic

Epic management commands.

```bash
br epic <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `status [--eligible-only]` | Show epic status with child progress and eligibility |
| `close-eligible [--dry-run] [--transition-comment <TEXT>]` | Atomically close eligible epics; attach one fresh transition comment to each |

---

### detach

Move an issue out from under its parent.

```bash
br detach <IDS>...
```

What happens depends on the ID's own shape, not just on whether a
`parent-child` dependency exists:

- A **dotted ID** (`ab-xxx.1`) makes a hierarchy claim just by its shape, so
  detaching one mints a fresh, independent flat ID (via the same generator
  `br create` uses), renames the issue to it, and drops the `parent-child`
  dependency. The old dotted ID is folded into `former_ids` and keeps
  resolving afterward, so references written before the detach — other
  issues' text, external trackers, scripts — still work.
- A **flat ID** makes no hierarchy claim by its shape, so detaching one only
  drops the `parent-child` dependency; the ID itself never changes.
- An issue with **no parent** by either measure is a successful no-op:
  detaching the same ID twice in a row is safe to script.

The point of the command is closing epics: an epic can't close without
`--force` while it still has open children, and `br detach` is how a child
that should no longer count stops counting, without touching the epic
itself. `br info --projections` reports it if a dotted ID and its
`parent-child` dependency ever disagree; `br detach` is the manual fix for
that divergence.

**Examples:**
```bash
# Detach a dotted child - mints a new flat ID, old ID keeps resolving
br detach ab-abc123.1

# Detach a flat issue that only carries a parent-child dependency
br detach ab-abc123

# Safe to repeat: the second call is a no-op
br detach ab-abc123.1
br detach ab-abc123.1
```

---

### rename

Change an issue's ID.

```bash
br rename <OLD_ID> <NEW_ID> [--dry-run]
```

The whole subtree moves with it, deepest-first: renaming `ab-abc` to `ab-auth`
turns `ab-abc.1.2` into `ab-auth.1.2`. The vacated ID becomes a tombstone and is
folded into the new issue's `former_ids`, so references written before the rename
keep resolving — `br show ab-abc` hands back `ab-auth`.

`OLD_ID` is resolved like any other ID argument: a partial ID, a hash fragment or
a former ID all work. `NEW_ID` is taken literally, since it is a name being chosen
rather than a row being found.

**Options:**
| Option | Description |
|--------|-------------|
| `--dry-run` | Print the rename and its full cascade without writing anything |

Four things are refused:

| Refused | Why, and what to use instead |
|---------|------------------------------|
| A target that already exists | Includes tombstones. A tombstone is what keeps a previously-vacated ID resolving, so it is not free to reuse. |
| A tombstoned source | Same gate every other mutating command goes through. |
| A change of position in the hierarchy | A dotted ID always names its real parent, so `ab-abc → ab-xyz.1` would leave the ID and the `parent-child` link disagreeing. Use `br update <id> --parent <id>` or `br detach <id>`. |
| A change of prefix | Use `br sync --rename-prefix`, which rewrites a whole workspace. One issue at a time would leave a row whose prefix disagrees with its workspace. |

**Examples:**
```bash
# See the blast radius first - a rename cascades
br rename ab-abc123 ab-auth --dry-run

# Then do it
br rename ab-abc123 ab-auth

# The old ID keeps working
br show ab-abc123
```

---

### statuses

Print the status vocabulary this project accepts.

```bash
br statuses [--json]
```

`br statuses` and `br types` exist because the answer is project-specific rather than a constant.
`Status::Custom` means **any** status string is accepted unless
`.beads/policy.yaml` enumerates a set under `workflow.statuses` *and* sets
`workflow.strict: true` — and there was previously no way to ask which of those
two worlds you were in, let alone what the set was.

`br statuses` reports, per status, whether it is built into `br` or comes only
from `policy.yaml`, and — when the policy is enforcing — whether it is currently
allowed. It also prints the `br ready` status group. Three states are
distinguished, the middle one deliberately:

| State | Meaning |
|-------|---------|
| No policy | Any status value is accepted. |
| `strict: true`, `statuses:` empty | **Nothing is enforced.** A project that set `strict` and expected enforcement has a real problem, and this is where it shows. |
| `strict: true`, `statuses:` non-empty | Enforcing; a status outside the set is rejected by `create`/`update`. |

`closed` and `tombstone` are never settable through `br update --status`
regardless of policy — use `br close` and `br delete`, which apply their own
transition and capacity checks and rewire dependencies.

**Examples:**
```bash
# What may I set on this project?
br statuses --json | jq -r '.statuses[] | select(.allowed) | .name'

# Is anything actually enforced here?
br statuses --json | jq '.enforced'
```

---

### types

Print the issue-type vocabulary this project accepts.

```bash
br types [--json]
```

The flatter of the pair: it prints the built-in types and states plainly that any
other value is also accepted and stored as given (`--json` reports this as
`any_value_accepted`). There is no type vocabulary in `policy.yaml` to narrow it
with, unlike statuses — that is the honest answer rather than a placeholder for a
future policy key. See [statuses](#statuses) for why both commands exist.

---

### comments

Manage comments on issues.

```bash
br comments <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `add <ID> [TEXT]...` | Add a comment |
| `list <ID>` | List comments |

**Options:**
| Option | Description |
|--------|-------------|
| `--wrap` | Wrap long comment lines when listing |
| `add -f, --file <PATH>` | Read comment text from file |
| `add --author <NAME>` | Override the default author |
| `add --message <TEXT>` | Comment text as an alternative flag |
| `list --wrap` | Wrap long comment lines |

---

## Workflow Commands

### defer / undefer

Defer or undefer issues.

```bash
br defer <IDS>... [OPTIONS]
br undefer <IDS>... [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--until <DATE>` | Defer until date |
| `--transition-comment <TEXT>` | Add a fresh comment atomically with each status transition |

---

## Sync & Config

### sync

Sync database with JSONL file.

```bash
br sync [OPTIONS]
```

**SAFETY GUARANTEES:**
- NEVER executes git commands or auto-commits
- NEVER modifies files outside the selected workspace's `.beads/` (unless `--allow-external-jsonl`)
- Uses atomic temp-file-then-rename pattern
- Safety guards prevent accidental data loss

**Modes (one required unless --status):**
| Option | Description |
|--------|-------------|
| `--flush-only` | Export database to JSONL |
| `--import-only` | Import JSONL into database |
| `--merge` | Three-way merge `.beads/beads.base.jsonl`, SQLite, and JSONL |
| `--status` | Show sync status (read-only) |

**Options:**
| Option | Description |
|--------|-------------|
| `-f, --force` | Override safety guards (use with caution) |
| `--force-db` | With `--merge`, resolve conflicts by keeping the local SQLite version |
| `--force-jsonl` | With `--merge`, resolve conflicts by keeping the JSONL version |
| `--allow-external-jsonl` | Allow JSONL path outside `.beads/` |
| `--manifest` | Write manifest file with export summary |
| `--error-policy <POLICY>` | Export error handling: strict, best-effort, partial, required-core |
| `--rename-prefix` | During import, rewrite mismatched issue IDs into the configured default prefix |
| `--rebuild` | During import, rebuild SQLite from JSONL and remove DB entries absent from JSONL |

**Merge semantics:**
- `--merge` uses `.beads/beads.base.jsonl` as the common ancestor and compares it with the local SQLite database and current JSONL file.
- Without an explicit conflict policy, semantic conflicts stop the command. This covers both-modified, delete-vs-modify, and convergent same-ID creation conflicts.
- `--force-db` keeps local SQLite changes for conflicts, `--force-jsonl` keeps JSONL changes for conflicts, and `--force` chooses the side with the newer timestamp.
- `--force-db`, `--force-jsonl`, and `--force` are mutually exclusive for `--merge`.

**Rebuild semantics:**
- `--rebuild` is valid only with explicit import mode: `br sync --import-only --rebuild`.
- JSONL is authoritative. After import, entries present only in SQLite are removed; deletion tombstones are preserved when applicable.
- `--rebuild` is rejected with every non-import mode, including `--flush-only`, `--merge`, and `--status`.
- Recovery artifacts are preserved under `.beads/.br_recovery/` when br has to move aside a damaged SQLite family before rebuilding.
- If open-time recovery rebuilt the database before a semantic import flag such as `--rename-prefix` could apply, br prints a rerun command that includes the needed flags.

**Examples:**
```bash
# Export to JSONL explicitly; useful as a final check before committing .beads/
br sync --flush-only

# Import from JSONL
br sync --import-only

# Merge DB and JSONL after both changed
br sync --merge

# Resolve semantic merge conflicts explicitly
br sync --merge --force-db
br sync --merge --force-jsonl
br sync --merge --force

# Rebuild SQLite from authoritative JSONL
br sync --import-only --rebuild

# Rebuild while rewriting imported IDs to the configured prefix
br sync --import-only --rebuild --rename-prefix

# Check sync status
br sync --status

# Export with verbose logging
br sync --flush-only -v
```

---

### config

Configuration management.

```bash
br config <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `list [--project | --user]` | List available config options |
| `get <KEY>` | Get a specific config value |
| `set <KEY=VALUE>` or `set <KEY> <VALUE>` | Set a config value |
| `delete <KEY>` | Delete a config value; `unset` is an alias |
| `edit` | Open the user config file in `$EDITOR` |
| `path` | Show config file paths |

**Examples:**
```bash
# List all config
br config list

# Get specific value
br config get id.prefix

# Set value
br config set id.prefix=myproj
br config set id.prefix myproj

# Edit in editor
br config edit
```

---

### remote

Mirror this workspace into an external tracker. The CLI surface is
backend-neutral; the only backend today is YouTrack.

```bash
br remote <COMMAND>
```

Configuration lives in `.beads/remote.yaml` — the backend, the instance URL,
the project short name, and total, injective maps from every beads
`issue_type`, `status` and priority onto the remote's own vocabulary. The
credential is read from the `BR_YOUTRACK_TOKEN` environment variable and from
nowhere else, so no file `br` writes can carry it.

| Subcommand | Description |
|------------|-------------|
| `init` | Provision the remote project so this workspace can mirror into it |
| `status` | Report what a push or pull would do, writing nothing |
| `push` | Push local changes to the remote tracker |
| `pull` | Pull remote changes into this workspace |
| `sync` | Push, then pull |

Only `init` is implemented today; the other four are declared so the whole
surface lands in one place.

**`br remote init`**

| Option | Description |
|--------|-------------|
| `--allow-shared-bundle` | Add values to a bundle other projects also use, instead of refusing |
| `--keep-project-defaults` | Leave the project's Type/State/Priority defaults alone |
| `--dry-run` | Report what would change without writing anything |

`init` reconciles the remote project's schema against `remote.yaml`. Before it
opens a socket it checks that every `issue_type` and `status` this workspace
actually holds is named by a map, and fails naming both the value and the
config key that would cover it — so a config gap costs one local read rather
than dying several hundred writes into a first run.

Two of its effects are visible to people who never run `br`, and both are
deliberate:

- **Custom field prototypes are instance-wide.** Creating `Design`,
  `Acceptance Criteria`, `Notes`, `Close Reason` and `Beads ID` makes them
  appear in every project's administration UI. There is no per-project field
  namespace to use instead. `init` names each one before it creates it.
- **The project's `Type`/`State`/`Priority` defaults are rewritten** to the
  beads defaults (`task` / `open` / P2, through the configured maps). Stock
  YouTrack adopts a new issue as `Bug` / `Submitted` / `Normal`, which reads
  back as a deferred bug at P3 — wrong on all three axes. `init` prints each
  change as `old → new` and skips any that already matches;
  `--keep-project-defaults` turns the whole step off.

Values are added to a bundle only when that bundle is *provably* private —
one request returns every `(field, project)` pair using every bundle, and a
bundle whose only user is this project's field is safe. A shared bundle on an
empty project is copied, the project's field re-pointed at the copy, and the
copy filled; a shared bundle on a project that already holds issues is refused
with both ways forward named. A sharedness scan the token cannot complete
refuses too, rather than assuming privacy.

A project that already matches the maps issues **zero write requests**, so
`init` is safe to re-run.

**Examples:**
```bash
export BR_YOUTRACK_TOKEN=perm-…

# See what would change, writing nothing
br remote init --dry-run

# Provision for real
br remote init

# Leave the web UI's adoption defaults as they are
br remote init --keep-project-defaults
```

---

## Diagnostics & Info

### stats

Show project statistics.

```bash
br stats [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--by-type` | Show breakdown by issue type |
| `--by-priority` | Show breakdown by priority |
| `--by-label` | Show breakdown by label |
| `--activity` | Include recent activity stats explicitly |
| `--no-activity` | Skip recent activity stats |
| `--activity-hours <HOURS>` | Activity window in hours (default: 24) |
| `--format <FMT>` | Output format: text, json |

---

### info

Show workspace diagnostics and metadata.

```bash
br info [--schema] [--whats-new] [--thanks]
```

---

### version

Show version information.

```bash
br version
```

---

### history

Manage local history backups.

```bash
br history <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `list` | List backups |
| `restore <BACKUP>` | Restore from backup |

**Notes:**
- Backups are created during `br sync --flush-only` when overwriting a JSONL file inside `.beads/`, including custom `BEADS_JSONL` paths that still target `.beads/`.

---

## Utilities

### completions

Generate shell completions.

```bash
br completions <SHELL>
```

**Shells:** bash, zsh, fish, powershell

**Example:**
```bash
# Add to ~/.bashrc
br completions bash >> ~/.bashrc
source ~/.bashrc
```

---

## Exit Codes

| Code | Category | Description |
|------|----------|-------------|
| 0 | Success | Command completed successfully |
| 1 | Internal | Internal error |
| 2 | Database | Database error (not initialized, schema mismatch) |
| 3 | Issue | Issue error (not found, ambiguous ID) |
| 4 | Validation | Validation error (invalid input) |
| 5 | Dependency | Dependency error (cycle detected, self-dependency) |
| 6 | Sync/JSONL | Sync error (parse error, conflict markers) |
| 7 | Config | Configuration error |
| 8 | I/O | I/O error (file not found, permission denied) |

---

## Environment Variables

| Variable | Description |
|----------|-------------|
| `BEADS_DIR` | Override `.beads` directory location |
| `BEADS_JSONL` | Override JSONL file path (requires `--allow-external-jsonl`) |
| `BD_ACTOR` | Default actor name for audit trail |
| `EDITOR` | Editor for `br config edit` |
| `NO_COLOR` | Disable colored output (any value) |
| `RUST_LOG` | Logging level (debug, info, warn, error) |

Recommended routine default:

```bash
export RUST_LOG=error
```

This keeps successful commands readable by suppressing low-level dependency logs. Override it with `debug`/`trace` when troubleshooting.

---

## JSON Output Schemas

### Paginated envelope (list, ready, search, blocked)

**A command that can return fewer issues than matched wraps its array in an
envelope; a command that always returns everything emits a bare array.** That
is the rule, and it is the whole reason the two shapes coexist.

```json
{
  "issues": [ /* … issue objects … */ ],
  "total": 500,
  "limit": 50,
  "offset": 0,
  "has_more": true
}
```

| Field | Meaning |
| --- | --- |
| `issues` | The page. Never null; `[]` when nothing matched. |
| `total` | Matches **before** `--limit`/`--offset` applied. |
| `limit` | The cap in effect. `0` means unlimited, and then `has_more` is always `false`. |
| `offset` | Rows skipped before this page. |
| `has_more` | `true` when `offset + limit < total` — i.e. rows were dropped. |

Every command that takes a `--limit` uses it: `list` and `ready` (both default
`--limit 0`, unlimited), `search` and `blocked` (both default 50). `stale` and
`show` return bare arrays and always return everything — neither takes a
`--limit`, so neither can drop a row without saying so.

Read `has_more`, not `issues.length`: a full page is not evidence that the
result set ended there. Note that `limit` reflects what you asked for, so an
unlimited command reports `"limit": 0` and `"has_more": false`.

### Issue Object (list, show, ready)

```json
{
  "id": "bd-abc123",
  "title": "Issue title",
  "description": "Full description text",
  "design": "",
  "acceptance_criteria": "",
  "notes": "",
  "status": "open",
  "priority": 2,
  "issue_type": "task",
  "owner": "owner@example.com",
  "created_at": "2025-01-15T10:30:00Z",
  "created_by": "bob",
  "updated_at": "2025-01-16T14:20:00Z",
  "close_reason": "",
  "deleted_by": "",
  "delete_reason": "",
  "dependency_count": 0,
  "dependent_count": 3
}
```

### Dependency Object

```json
{
  "issue_id": "bd-abc123",
  "depends_on_id": "bd-def456",
  "dep_type": "blocks",
  "created_at": "2025-01-15T10:30:00Z",
  "created_by": "alice"
}
```

### Sync Status Object

```json
{
  "db_path": ".beads/beads.db",
  "jsonl_path": ".beads/issues.jsonl",
  "db_modified": "2025-01-16T14:20:00Z",
  "jsonl_modified": "2025-01-16T14:15:00Z",
  "db_issue_count": 150,
  "jsonl_issue_count": 148,
  "dirty_count": 2,
  "status": "db_newer"
}
```

### Error Object

Errors in `--json` mode go to **stdout**, so a scripted caller reads one
parseable stream; log lines stay on stderr.

```json
{
  "error": {
    "code": "ISSUE_NOT_FOUND",
    "message": "Issue not found: bd-xyz999",
    "hint": "Did you mean 'bd-xyz99'?",
    "retryable": false,
    "context": {
      "searched_id": "bd-xyz999",
      "similar_ids": ["bd-xyz99"]
    }
  }
}
```

| Field | Meaning |
| --- | --- |
| `code` | Stable `SCREAMING_SNAKE_CASE` identifier — match on this, not on `message`. |
| `message` | Human-readable description. Wording is not a stable interface. |
| `hint` | Optional suggested fix — the most specific one available. Absent when there is nothing useful to say. |
| `retryable` | Whether retrying could succeed — after fixing the input, or after waiting when the database is locked. |
| `context` | Optional, code-specific structured detail. |

The process exit code is derived from `code`: 2 for database errors, 3 for
issue errors, 4 for validation, 5 for dependencies, 6 for sync/JSONL, 7 for
config, 8 for I/O, 1 for internal, and 130 for an interrupted run.

`hint` prefers a value derived from what you actually typed over a generic
one. `br create x --priority high` answers `Did you mean --priority 1?`, and
falls back to the range (`Use a priority between 0 (critical) and 4
(backlog)`) only when nothing can be inferred. Do not match on the wording of
either — it is prose, and which of the two you get depends on the input.

For `ISSUE_NOT_FOUND`, `context.similar_ids` lists IDs within one edit of what
was searched for, closest first, and is `[]` when nothing is close — in which
case `hint` falls back to `Run 'br list' to see available issues.` Suggestions
are drawn from `issues.jsonl`, so an issue created since the last export is not
a candidate.

---

## See Also

- [README.md](../README.md) - Project overview
- [AGENTS.md](../AGENTS.md) - Agent integration guide
- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety model
