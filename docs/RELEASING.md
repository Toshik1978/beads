# Releasing

A release is cut by pushing a tag. Everything else is automated by
[`.github/workflows/release.yml`](../.github/workflows/release.yml), which
builds four binaries, archives them, writes a checksum file, and publishes one
GitHub release.

The workflow only ever *reads* the repository. It never commits, never bumps a
version, and never pushes anything back. If something needs to change, it
changes in a commit you make and push yourself.

## What a release consists of

Four `tar.gz` archives plus `br_<version>_checksums.txt`. Each archive
contains the `br` binary, `LICENSE`, `NOTICE.md`, and `README.md`.

| Target | Notes |
| --- | --- |
| `x86_64-unknown-linux-musl` | Fully static. No GLIBC requirement. |
| `aarch64-unknown-linux-musl` | Fully static. No GLIBC requirement. |
| `x86_64-apple-darwin` | Intel macOS. |
| `aarch64-apple-darwin` | Apple silicon macOS. |

SQLite is statically linked into all four — that is what `rusqlite`'s
`bundled` feature does — so none of them need a C compiler or a system
libsqlite3 to run, and a libsqlite3 upgrade on the host cannot affect them.

There is no Windows binary. `cargo-zigbuild` does not cover
`*-pc-windows-msvc`, and more decisively, `ci.yml` runs the test suite on
Linux and macOS only. Adding Windows means adding Windows CI first.

## Why Linux is musl-only

There is no `-gnu` archive. Both flavours were published in the first draft of
this pipeline and the glibc pair was dropped before any release was cut, so
nothing depends on them.

musl links fully static, which removes the GLIBC version floor from the
support matrix entirely and makes one binary work on any distribution
including Alpine — verified by running it there.

The standing objection to musl is that its allocator is slower under
allocation-heavy load. It does not apply to this binary: `src/main.rs`
installs mimalloc as the `#[global_allocator]` on every non-Windows target, so
musl's malloc never serves a Rust allocation. The other differences that
usually argue for glibc do not apply either — `br` does no networking (so
musl's resolver never runs), loads nothing dynamically (so static linking
costs nothing), and traverses the dependency graph iteratively rather than
recursively (so musl's smaller thread stacks are ample).

The one residual exposure, stated so this stays a decision rather than a
slogan: the bundled C SQLite allocates through libc directly, because the
mimalloc crate registers a Rust global allocator rather than interposing the C
`malloc` symbol. That is a much smaller surface than the whole program, and
neither side of it has been benchmarked here.

## Cutting a release

Suppose the new version is `1.1.0`.

**1. Bump `Cargo.toml`.** Set `version = "1.1.0"` and run a build so
`Cargo.lock` picks the new version up.

```sh
env -u RUSTUP_TOOLCHAIN cargo build
```

This is not optional bookkeeping. Cargo compiles the manifest version into the
binary as `CARGO_PKG_VERSION`, and `br version` prints it. A tag that
disagrees with the manifest publishes an archive named after one version
containing a binary that reports another.

**2. Generate the commit list and write the prose.**

```sh
TAG=v1.1.0 task changelog
```

`git-cliff` prepends a `## v1.1.0 — <date>` section with the commits grouped
by type. Write the framing paragraph and a Highlights list above that
generated list — the whole section becomes the GitHub release body verbatim,
so it is the only release-notes surface there is.

**3. Check the tracked-file guards.** A generated changelog is a new tracked
file built out of commit subjects, so it is subject to the licensing sweep
like any other:

```sh
env -u RUSTUP_TOOLCHAIN cargo test --test licensing
```

**4. Verify the tag before it exists.**

```sh
TAG=v1.1.0 task release:verify
```

This runs exactly the two checks the workflow runs first — the manifest/tag
match and the changelog section — while they are still free to fail. A tag is
public the moment it is pushed, and correcting one means deleting it.

**5. Prove the artifacts build.** Nominally optional, and skipped once at a
cost: this is the only step that catches a target that has stopped linking, and
the only one that runs `.goreleaser.yaml` at all.

```sh
task release:snapshot
```

Four release-profile builds of a crate that compiles the C SQLite
amalgamation, with no cache shared between targets. It takes a while, and the
first run also pulls the image. The archives land in `dist/` and are named
`-SNAPSHOT` so they cannot be mistaken for real ones.

**A config error costs you none of that time.** GoReleaser validates and
resolves the whole build before it compiles anything, so a bad
`.goreleaser.yaml` fails in about a second — which is how v1.0.0 shipped a tag
that could not build (the rust builder refuses a `[workspace]` manifest with no
`--package=`, and nothing but this step exercises that path). If you skip step 5
on the grounds that nothing touched the build, run it anyway when
`.goreleaser.yaml`, `Cargo.toml`'s `[workspace]`, or the crate layout changed;
you will know inside a second whether it was worth it.

Needs Docker. It runs in the same container image the release job uses, and
that is not incidental — see [Why the build runs in a
container](#why-the-build-runs-in-a-container).

**6. Commit, then tag and push.**

```sh
git commit -am 'chore(release): v1.1.0'
git tag v1.1.0
git push origin main v1.1.0
```

The tag push is what starts the workflow.

## What the workflow does, in order

A `guards` job on a bare runner:

1. Checks the tag against `Cargo.toml` (`check-release-version.sh`).
2. Checks the tag has a CHANGELOG section (`extract-changelog.sh`).

Then a `release` job in the cross-compilation container:

3. Trusts the workspace for git, updates the toolchain past the crate's
   `rust-version` floor, and refuses a pre-release compiler.
4. Re-derives `release-body.md` from CHANGELOG.md.
5. Runs `goreleaser release --clean --release-notes=release-body.md`.

The guards are a separate job on purpose. Both are cheap, both catch mistakes
that are otherwise only visible after publication, and neither needs the
container image the build job spends its first minutes pulling — so a bad tag
fails in seconds while the release is still a complete no-op.

## Why the build runs in a container

**This is about cross-compilation only. Building `br` for the machine you are
sitting at needs none of it** — no container, no zig, no cargo-zigbuild:

```sh
env -u RUSTUP_TOOLCHAIN task build:release   # -> target/release/br
env -u RUSTUP_TOOLCHAIN cargo install --path .
```

A native build uses your platform's own C compiler and linker. Everything
below concerns asking *zig* to link for a target that is not the host, which
is a different job with a different requirement.

The two `*-apple-darwin` targets link against Apple frameworks —
CoreFoundation, by way of chrono's local-timezone lookup. zig does not bundle
those, so `cargo zigbuild` needs a macOS SDK on disk to resolve them.
`ghcr.io/rust-cross/cargo-zigbuild` ships one and points `SDKROOT` at it
(`/opt/MacOSX11.3.sdk`, which carries `CoreFoundation.tbd`).

This is measured, not defensive. Running the release matrix against a plain
`cargo install cargo-zigbuild` on a macOS host fails `aarch64-apple-darwin` at
link time:

```
error: undefined symbol: _CFRelease
  note: referenced by ...chrono::offset::local::inner::current_zone
```

Setting `SDKROOT` to the Command Line Tools SDK does not fix it. The
significant part is the shape of the failure: every Linux target and one of
the two macOS targets succeed, so a release built outside the container would
ship most of its artifacts and quietly omit the rest.

The image also ships a rustc *below* this crate's `rust-version` floor, which
is why both the workflow and `task release:snapshot` run `rustup default
stable` before building. Without it every target fails with a message that
blames the dependencies for what is actually a toolchain problem.

`task build:cross` is the standing local reproduction of the release matrix
in that image, without GoReleaser in the picture.

## What ordinary CI checks

`ci.yml` has a `release-config` job that runs on every push and pull request.
It validates `.goreleaser.yaml` and checks that the version currently in
`Cargo.toml` already has a `CHANGELOG.md` section. Neither builds anything.

The second check is why step 1 and step 2 above belong in the same commit. Bump
the manifest without writing the notes and CI goes red immediately, rather than
letting the omission sit until a tag push discovers it. If you need the bump
landed before the prose is ready, write a placeholder section under the new
heading and fill it in before tagging.

## Things that are deliberately not here

**No crates.io publishing.** GitHub Releases is the only binary distribution
channel. `cargo install --path .` from a clone works and is documented in the
README, including its C-compiler requirement.

**No version bumping in CI.** The tag is the version and a human chooses it.
Nothing in the pipeline writes to the repository.

**No signing or attestation, yet.** GoReleaser supports cosign signing and
SBOM generation, and GitHub Actions can attest build provenance. None of it is
wired up. It would be a reasonable next step rather than a gap that makes the
current pipeline wrong.

**GoReleaser's own changelog generator is switched off**
(`changelog.disable` in `.goreleaser.yaml`) rather than left running and
overridden. Two sources of release notes would be one too many.

## A note on the builder

`.goreleaser.yaml` uses `builder: rust`, which GoReleaser itself still labels
experimental — `goreleaser check` prints "you are using the experimental Rust
builder" on every run. In practice it does one job: drive `cargo zigbuild` per
target and run `rustup target add` for each. If it ever regresses,
`task build:cross` reproduces the same release matrix directly, without
GoReleaser, and is the reference for what the builds should do.
