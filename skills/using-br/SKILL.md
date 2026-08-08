---
name: using-br
description: >-
  Use when working inside a directory that has a `.beads/` workspace (or when
  asked to initialize one), and the task involves tracking, creating,
  querying, updating, or closing issues with the `br` CLI. Covers the full
  27-command surface, the `--json` agent output mode, the
  auto-flush/auto-import sync model, and the claim-work-close loop. Load this
  before shelling out to `br` for the first time in a session.
---

# Using `br`

`br` is a single SQLite-backed binary with a JSONL export for portability and
version control. This skill is the agent contract for *using* an existing
`br` installation against a workspace — not for developing `br` itself. If
you are instead working on the `br` source tree, that is a different
audience; see this repository's own contributor documentation instead of
this file.

Full flag-level reference (every option, exit code, `--json` schema) lives in
[`docs/CLI_REFERENCE.md`](../../docs/CLI_REFERENCE.md). This file is the
quick-start contract; reach for `CLI_REFERENCE.md` for anything this file
doesn't cover. [`docs/AGENT_INTEGRATION.md`](../../docs/AGENT_INTEGRATION.md)
covers workspace-level integration concerns (detection, environment
variables, policy files) in more depth than this skill does.

## Detect a workspace before doing anything else

A workspace is a `.beads/` directory holding a SQLite database and a tracked
`issues.jsonl` export. Check for it before running any command that assumes
one exists:

```bash
test -d .beads && echo "workspace present"
```

If there is no `.beads/` directory and the task calls for one, create it:

```bash
br init
```

`br` also does cross-project routing: an explicit issue ID whose prefix
matches an entry in `.beads/routes.jsonl` is transparently routed to another
workspace. Don't assume every ID you see belongs to the current directory's
workspace; see the "Cross-Project Routing" section of `CLI_REFERENCE.md`.

## Always use `--json` when scripting

Every query and mutating command accepts `--json` for machine-readable
output. Parse `--json` output with `jq` rather than the human-formatted
default:

```bash
br ready --json --limit 10 | jq '.[].id'
br show bd-abc123 --json | jq '.status'
```

## The core loop: find work, claim it, do it, close it

```bash
# Find unblocked work
br ready --json --unassigned

# Claim atomically (sets assignee=actor and status=in_progress in one step)
br update bd-abc123 --claim --json

# ... do the work ...

# Close with a reason, and surface newly-unblocked work in one call
br close bd-abc123 --reason "done" --suggest-next --json
```

Mutating commands (`create`, `update`, `delete`, `close`, `reopen`, and a few
subcommands of `dep`/`label`/`comments`/`epic`) auto-flush their changes to
`.beads/issues.jsonl` when they succeed, so the JSONL file is normally ready
to `git add` right after the command completes — no separate sync step is
needed in the common case. `br sync --flush-only` is still useful as an
idempotent final check before committing.

## Command index

All 27 top-level commands. Flags, subcommands, and exit codes are in
`CLI_REFERENCE.md`, linked above.

| Command | Purpose |
|---|---|
| `init` | Initialize a beads workspace in the current directory |
| `create` | Create a new issue |
| `list` | List issues |
| `show` | Show issue details |
| `update` | Update an issue |
| `close` | Close an issue |
| `reopen` | Reopen an issue |
| `delete` | Delete an issue (creates tombstone) |
| `ready` | List ready issues (open, unblocked, not deferred) |
| `blocked` | List blocked issues |
| `search` | Search issues |
| `stale` | List stale issues |
| `dep` | Manage dependencies |
| `label` | Manage labels |
| `epic` | Epic management commands |
| `detach` | Move an issue out from under its parent |
| `rename` | Change an issue's ID, cascading to its descendants |
| `statuses` | Print the status vocabulary this project accepts |
| `types` | Print the issue-type vocabulary this project accepts |
| `comments` | Manage comments |
| `sync` | Sync database with JSONL file (export or import) |
| `config` | Configuration management |
| `stats` | Show project statistics |
| `info` | Show diagnostic metadata about the workspace |
| `version` | Show version information |
| `history` | Manage local history backups |
| `completions` | Generate shell completions |

## Notes for an agent driving `br`

- Set `BD_ACTOR` (or pass `--actor <NAME>`) so mutations carry a meaningful
  audit-trail identity instead of the OS user.
- `br ready` only shows `open` issues by default (not `in_progress`); a
  project's `.beads/policy.yaml` can widen that set — see "Configurable
  ready status group" in `CLI_REFERENCE.md`.
- `search` and `blocked` default to a capped result count (50); `list` and
  `ready` are unlimited by default. Pass `--limit N` either direction.
- Prefer `br update <id> --claim` over hand-setting `--assignee` and
  `--status in_progress` separately: it is one atomic transaction.
