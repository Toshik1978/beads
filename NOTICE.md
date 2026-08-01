# NOTICE

## What this project is

A personal task tracker, forked and heavily reduced from `beads_rust`.

## Lineage

1. **[beads](https://github.com/steveyegge/beads)** — the original issue
   tracker, by Steve Yegge.
2. **[beads_rust](https://github.com/Dicklesworthstone/beads_rust)** — a Rust
   port of beads' "classic" SQLite + JSONL architecture, by Jeffrey Emanuel
   (Dicklesworthstone). Copyright (c) 2026 Jeffrey Emanuel.
3. **This project** — a fork of `beads_rust`, reduced to a personal-scale
   tracker. Modifications copyright (c) 2026 Anton Krivenko.

This repository has no upstream git remote. It is maintained independently and
does not track changes from either predecessor.

## Licensing — read this before redistributing

This project is licensed under the terms in [`LICENSE`](./LICENSE), which is
**MIT with an additional OpenAI/Anthropic Rider**, and is reproduced from
upstream **unmodified**, as that license requires.

**This is not plain MIT.** The rider is a binding condition of the license, not
a footnote. In particular:

- No rights are granted to OpenAI, L.L.C., Anthropic, PBC, their affiliates, or
  anyone acting on their behalf or under their direction.
- The Software and any derivative works may not be made available to those
  parties, including for training, evaluation, benchmarking, or indexing.
- **Any redistribution of this project, or of works derived from it, must
  include the rider unmodified.**

Because this project is a derivative work of `beads_rust`, the rider applies to
it in full and cannot be removed or relaxed here. Do not describe this project
as MIT-licensed without that qualification.

## Modifications

Substantial changes from upstream `beads_rust`:

- Command surface reduced from 42 commands to 24 (counting top-level
  subcommands as `br --help` lists them, excluding clap's generated `help`).
- Removed: the MCP server, swarm-coordination and agent-orchestration
  machinery, the `doctor` diagnostic subsystem, self-update, and release
  packaging.
- Storage engine changed from `fsqlite` to `rusqlite`, moving the project from
  a required nightly toolchain to stable Rust.
- Test suite made hermetic — no dependence on the OS username, ambient
  timezone, locale, terminal width, or a symlink-free temp directory.

The `.beads/` on-disk layout and the `issues.jsonl` schema are deliberately
unchanged, so that existing readers of those files keep working. The layout is
kept stable for its own sake; this project carries no contract on any
particular consumer's behalf.
