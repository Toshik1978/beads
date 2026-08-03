# Changelog

What changed in each `br` release and why. Highlights are written by hand; the
commit lists under them are generated with [git-cliff](https://git-cliff.org)
via `TAG=vX.Y.Z task changelog`.

Versions follow [semver](https://semver.org). Commits follow
[Conventional Commits](https://www.conventionalcommits.org).

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

