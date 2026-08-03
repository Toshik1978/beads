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
  - [comments](#comments)
- [Workflow Commands](#workflow-commands)
  - [defer / undefer](#defer--undefer)
- [Sync & Config](#sync--config)
  - [sync](#sync)
  - [config](#config)
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
| `-a, --assignee <NAME>` | Assign to person |
| `--owner <EMAIL>` | Set owner email |
| `-l, --labels <LABELS>` | Labels (comma-separated) |
| `--parent <ID>` | Parent issue ID (creates parent-child dependency) |
| `--deps <DEPS>` | Dependencies (format: `type:id,type:id`) |
| `-e, --estimate <MINUTES>` | Time estimate in minutes |
| `--due <DATE>` | Due date (RFC3339 or relative like `+2d`, `tomorrow`) |
| `--defer <DATE>` | Defer until date |
| `--external-ref <REF>` | External reference (e.g., `gh-123`) |
| `--ephemeral` | Mark as ephemeral (not exported to JSONL) |
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

# Feature with assignee and labels
br create "Add dark mode" -t feature -a alice -l "ui,enhancement"

# Task with due date
br create "Deploy to production" --due "+3d"

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
| `--assignee <NAME>` | Filter by assignee |
| `--unassigned` | Show only unassigned issues |
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
| `--overdue` | Filter for overdue issues |

**Output Options:**
| Option | Description |
|--------|-------------|
| `--limit <N>` | Maximum results (0=unlimited; default: unlimited — the full work surface). Pass `--limit N` to cap. |
| `--sort <KEYS>` | Comma-separated sort keys (see Sorting below) |
| `-r, --reverse` | Reverse sort order |
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
| `assignee` | ascending — A-Z, unassigned always last |
| `title` | ascending — A-Z, case-insensitive |
| `created_at` (`created`) | descending — newest first |
| `updated_at` (`updated`) | descending — newest first |

`-` forces descending and `+` forces ascending. `--reverse` flips every key.
Every sort ends with an implicit `id` tiebreaker, so output is deterministic.

Because `created_at` and `updated_at` are already descending by default,
`-created`/`-updated` change nothing — they force the same direction the
field already sorts in. Use `+created`/`+updated` for oldest-first, or
`--reverse` to invert every resolved key at once.

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

# My assigned work
br list --assignee $(whoami)

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
| `--notes <TEXT>` | Update additional notes |
| `--transition-comment <TEXT>` | Add a fresh comment atomically with a status transition |
| `-s, --status <STATUS>` | Change status |
| `-p, --priority <N>` | Change priority |
| `-t, --type <TYPE>` | Change issue type |
| `--assignee <NAME>` | Assign (empty string clears) |
| `--owner <EMAIL>` | Set owner (empty string clears) |
| `--claim` | Atomic claim (assignee=actor + status=in_progress) |
| `--force` | Force update even if issue is blocked |
| `--due <DATE>` | Set due date (empty string clears) |
| `--defer <DATE>` | Set defer date (empty string clears) |
| `--estimate <MINUTES>` | Set time estimate |
| `--add-label <LABEL>` | Add label(s) |
| `--remove-label <LABEL>` | Remove label(s) |
| `--set-labels <LABELS>` | Replace all labels |
| `--parent <ID>` | Reparent (empty string removes) |
| `--external-ref <REF>` | Set external reference |
| `--session <ID>` | Set `closed_by_session` when closing |

**Examples:**
```bash
# Claim a task
br update bd-abc123 --claim

# Change status
br update bd-abc123 -s in_progress

# Update multiple issues
br update bd-abc123 bd-def456 -p 1

# Add labels
br update bd-abc123 --add-label "urgent,reviewed"
```

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
| `--transition-comment <TEXT>` | Add a fresh comment atomically with the close transition |
| `-f, --force` | Close even if blocked by open dependencies |
| `--suggest-next` | Return newly unblocked issues |
| `--session <ID>` | Session ID for tracking |

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
| `--assignee <NAME>` | Filter by assignee |
| `--unassigned` | Show only unassigned |
| `-l, --label <LABEL>` | Filter by label (AND logic) |
| `--label-any <LABEL>` | Filter by label (OR logic) |
| `-t, --type <TYPE>` | Filter by type |
| `-p, --priority <N>` | Filter by priority |
| `--sort <POLICY>` | Sort: hybrid (default), priority, oldest |
| `--include-deferred` | Include deferred issues |
| `--parent <ID>` | Filter to children of a parent issue |
| `-r, --recursive` | Include all descendants with `--parent` |
| `--wrap` | Wrap long lines instead of truncating in text output |
| `--format <FMT>` | Output format: text, json |

**Examples:**
```bash
# My ready work
br ready --assignee $(whoami)

# Unassigned high-priority
br ready --unassigned -p 0 -p 1

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
- Multi-target `update`/`--claim`, `close`, `reopen`, `defer`, and `undefer`
  commands evaluate the repository's final prospective state and commit all
  status changes in one transaction. Hard-limit and late validation failures
  roll back the entire repository-local batch; capacity-neutral swaps do not
  depend on request order.
- Routed commands transact each repository independently. There is no
  distributed transaction across repositories, so an earlier route may already
  be committed if a later route fails and cross-repository atomicity is
  intentionally not claimed.
- Omitting `workflow.capacity` preserves existing behavior exactly.
- The current enforcement layer has fixed `repository` scope and `all`
  counting. Hierarchy-aware counting, audited exemptions, actor/assignee/
  harness/session/subtree scopes, and capacity observability are later phases
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
br search "bug" -t bug --assignee alice
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

**Abandoned in-progress claims:**

`br ready` does not show `in_progress` issues. To audit hidden work, combine
`stale` with an explicit in-progress listing and inspect the claim evidence:

```bash
br stale --days 1 --json
br list --status in_progress --json
br show <id> --json
br comments list <id> --json
```

An `in_progress` issue is a reclaim candidate when `updated_at` is old, the
assignee or session metadata no longer points to an active worker, and recent
comments or Agent Mail reservations do not show live work. Default thresholds
are two hours for automated swarm claims and one business day for human or
unclear claims.

Before reclaiming, add an audit comment with the evidence, then claim:

```bash
br comments add <id> --author "$BD_ACTOR" \
  --message "reclaim: previous in_progress claim appears abandoned; evidence: updated_at=<timestamp>, assignee=<name>, no active reservation or pane" \
  --json
br update <id> --claim --json
```

There is not a separate reclaim command; the audit comment plus `update --claim`
is the documented recovery workflow.

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
- `--rebuild` is rejected with every non-import mode, including `--flush-only`, `--merge`, `--status`, and `--witness`.
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
| `--by-assignee` | Show breakdown by assignee |
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
  "assignee": "alice",
  "owner": "owner@example.com",
  "created_at": "2025-01-15T10:30:00Z",
  "created_by": "bob",
  "updated_at": "2025-01-16T14:20:00Z",
  "close_reason": "",
  "closed_by_session": "",
  "source_system": "",
  "deleted_by": "",
  "delete_reason": "",
  "sender": "",
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
