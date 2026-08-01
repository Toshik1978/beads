# Agent Integration

How an AI agent integrates `br` into work on some *other* project's
repository — detecting a workspace, scripting against it, and staying out of
its own way around git. This is a different audience from
[`../CLAUDE.md`](../CLAUDE.md), which covers working *on* the `br` source
tree itself; check there instead if that's what you're doing.

For the full command/flag/exit-code/`--json`-schema reference, see
[`CLI_REFERENCE.md`](CLI_REFERENCE.md) — this document does not repeat it.
For a compact, load-once contract meant to be installed as an agent skill,
see [`../skills/using-br/SKILL.md`](../skills/using-br/SKILL.md); this
document goes into more depth on workspace-level integration than that file
does.

## Detecting a workspace

A `br` workspace is a `.beads/` directory containing a SQLite database
(`.beads/*.db`, gitignored — it's a derived cache) and a tracked
`issues.jsonl` export (the durable, version-controlled source of truth). Test
for the directory before assuming a workspace exists:

```bash
test -d .beads && echo "workspace present"
```

`br --db <PATH>` auto-discovers `.beads/*.db` when `--db` is omitted, walking
upward from the current directory the same way git finds `.git/`. If no
workspace exists yet and the task calls for one, `br init` creates it,
optionally with `--prefix <PREFIX>` to control the issue-ID prefix.

## JSON is the integration surface

Every query and mutating command accepts `--json`. Treat
`--json` output as the stable interface and the default text/table rendering
as for humans only — parse with `jq`, not by scraping text columns:

```bash
br ready --json --unassigned | jq -r '.[].id'
br show bd-abc123 --json | jq '{status, assignee}'
```

The `Issue`, `Dependency`, sync-status, and error object shapes returned by
`--json` are documented under "JSON Output Schemas" in `CLI_REFERENCE.md`.
The same field set is what gets written to `issues.jsonl`, and that
serialized field set is a tested, stable interface (see
[`ARCHITECTURE.md`](ARCHITECTURE.md#the-issue-field-set-is-a-published-interface))
— an external tool parsing `issues.jsonl` directly is a supported use case,
not an implementation detail to route around.

## The sync model: JSONL is durable, the database is not

Only `issues.jsonl` (and `attachments/`) are tracked in git; the SQLite
database is gitignored and disposable. Mutating commands (`create`,
`update`, `delete`, `close`, `reopen`, and the mutating subcommands of
`dep`/`label`/`comments`/`epic`) auto-flush to `issues.jsonl` on success, and
every command auto-imports a newer `issues.jsonl` before it runs — so an
agent that just runs commands normally, without any explicit sync step, stays
converged with git across a `pull`/`push` cycle. See
[`ARCHITECTURE.md`](ARCHITECTURE.md) for why the design is shaped this way.

Two situations where an agent should sync explicitly:

- **Before committing**, run `br sync --flush-only` as an idempotent final
  export check — useful after `--no-auto-flush`, after disabling auto-flush
  in config, or during recovery.
- **After a `git pull`/`git merge`** that could have changed `issues.jsonl`
  concurrently with local database mutations, `br sync --merge` performs a
  three-way merge against `.beads/beads.base.jsonl`; see "Merge semantics" in
  `CLI_REFERENCE.md` for conflict-resolution flags.

`br sync` never runs git commands and never modifies files outside the
workspace's `.beads/` (unless `--allow-external-jsonl` is explicit) — an
agent is expected to own the actual `git add`/`git commit`.

## Environment and identity

| Variable | Effect |
|---|---|
| `BD_ACTOR` | Default actor name recorded in the audit trail; prefer this (or `--actor <NAME>`) over letting `br` fall back to the OS user, so mutations are attributable to the agent/session that made them. |
| `BEADS_DIR` | Override `.beads` directory discovery. |
| `BEADS_JSONL` | Override the JSONL file path (requires `--allow-external-jsonl`). |
| `RUST_LOG` | Logging verbosity; `error` is a reasonable default for scripted use — it suppresses dependency-level noise without hiding real failures. |

## The `ready` group and capacity policy are project-configurable

Two behaviors that vary per project, controlled by an optional
`.beads/policy.yaml` and worth checking for before assuming defaults:

- **What counts as "ready".** By default `br ready` only surfaces `open`
  issues; a project can widen this (e.g. to also surface `rework` after a
  review bounce) via `workflow.status_groups.ready`. `br ready`'s output
  changes accordingly — parse the returned `status` field rather than
  assuming it's always `"open"`.
- **Work-in-progress limits.** `workflow.capacity` can cap how many issues
  may be in a given status or status group at once, and gate specific status
  transitions behind those caps. A mutating command can be rejected outright
  by capacity policy, distinct from being rejected for a dependency reason —
  check the error's `kind`/`error_code` rather than assuming every rejection
  is a blocked-dependency error.

Both are documented in full, with examples, under the `ready` and the
capacity sections of `CLI_REFERENCE.md`.

## Cross-project routing

An explicit issue ID whose prefix matches an entry in
`.beads/routes.jsonl` is routed to another workspace automatically — useful
when one project needs to read or update an issue that belongs to a sibling
project. An agent operating across multiple repositories should not assume
every ID it sees resolves against the current directory's workspace; see
"Cross-Project Routing" in `CLI_REFERENCE.md` for the resolution rules.

## Command index

All 24 top-level commands, for orientation. Full flags, subcommands, and
`--json` shapes are in `CLI_REFERENCE.md`.

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
| `comments` | Manage comments |
| `sync` | Sync database with JSONL file (export or import) |
| `config` | Configuration management |
| `stats` | Show project statistics |
| `info` | Show diagnostic metadata about the workspace |
| `version` | Show version information |
| `history` | Manage local history backups |
| `completions` | Generate shell completions |
