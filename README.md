[![CI](https://github.com/Toshik1978/beads/actions/workflows/ci.yml/badge.svg)](https://github.com/Toshik1978/beads/actions/workflows/ci.yml)
![Tests](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/Toshik1978/dd0300a64ea7f6b7acb4a6d70ef423b1/raw/tests.json&maxAge=180)
![Coverage](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/Toshik1978/dd0300a64ea7f6b7acb4a6d70ef423b1/raw/coverage.json&maxAge=180)

# beads

`br` is a personal, agent-friendly issue tracker: a single SQLite-backed
binary with a JSONL export for portability and version control.

## Install

This project is not published to crates.io.
[GitHub Releases](https://github.com/Toshik1978/beads/releases) is the only
binary distribution channel. A downloaded release binary needs no C compiler
and no system libsqlite3 to run, since SQLite is statically linked into every
release artifact.

Each release carries four archives. Pick the one matching your platform,
verify it against the published `br_<version>_checksums.txt`, and put `br`
somewhere on your `PATH`:

```sh
tar xzf br_1.0.0_aarch64-apple-darwin.tar.gz
install -m755 br ~/.local/bin/br
br --version
```

| Platform | Archive |
| --- | --- |
| Linux, x86_64 | `br_<version>_x86_64-unknown-linux-musl.tar.gz` |
| Linux, arm64 | `br_<version>_aarch64-unknown-linux-musl.tar.gz` |
| macOS, Apple silicon | `br_<version>_aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `br_<version>_x86_64-apple-darwin.tar.gz` |

The Linux downloads are `musl` builds and there is no glibc variant, by
design: they link fully static, so they carry no GLIBC version requirement
and the same binary runs on any distribution including Alpine.

There is no Windows binary — see [`docs/RELEASING.md`](docs/RELEASING.md) for
why, and for why Linux is musl-only.

To build from source instead:

```sh
git clone https://github.com/Toshik1978/beads
cd beads
cargo install --path .
```

`cargo install --path .` needs a working C compiler on your machine: the
`rusqlite` dependency's `bundled` feature compiles the SQLite amalgamation
from source. This is the one requirement a release download does not have.

Rust `1.95` or newer is required (see `rust-version` in `Cargo.toml`). There
is no `rust-toolchain.toml`; the project builds on any current stable
toolchain.

## Usage

```sh
br init                 # create a .beads/ workspace in the current directory
br create "Fix the bug" # create an issue
br ready                # list issues that are open and unblocked
br list
```

`br --help` lists all 27 top-level commands. Full documentation for every
command, its flags, exit codes, and `--json` output schemas lives in
[`docs/CLI_REFERENCE.md`](docs/CLI_REFERENCE.md).

## Storage

Each workspace is a `.beads/` directory holding a SQLite database plus an
`issues.jsonl` export. The JSONL file's serialized field set is a stable,
tested interface (`tests/storage/schema_shape.rs`) so that external tools
built against it keep working across releases.

## License

This project is licensed under the terms in [`LICENSE`](LICENSE) — MIT with
an additional rider restricting use by OpenAI and Anthropic. It is not plain
MIT; see [`NOTICE.md`](NOTICE.md) for the rider's terms and this project's
lineage.

## Contributing

beads is a personal project: pull requests may not be reviewed, issues may not
get a response, and no support is offered. Forking is a first-class outcome
rather than a fallback. [`CONTRIBUTING.md`](CONTRIBUTING.md) has the details
and the setup steps.

See also [`CLAUDE.md`](CLAUDE.md) for the development and verification
workflow, [`CHANGELOG.md`](CHANGELOG.md) for what changed in each release, and
[`docs/RELEASING.md`](docs/RELEASING.md) for how a release is cut.
