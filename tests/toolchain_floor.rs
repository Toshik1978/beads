//! The declared toolchain floor must stay consistent with the evidence beneath
//! it — without being confused *for* that evidence.
//!
//! Two separate numbers live in `Cargo.toml`:
//!
//! - the **declared floor**, the `rust-version` key, a support decision about
//!   which toolchains this project stands behind;
//! - the **measured minimum**, concluded by the bisection comment above the
//!   key, a property of the pinned `Cargo.lock` and nothing else.
//!
//! They are not required to be equal, and today they are not: the floor is
//! deliberately set above the minimum, excluding releases that do compile.
//! What they *are* required to do is stay legible — stated the same way in the
//! manifest key, the comment, and `CLAUDE.md`'s "Toolchain floor" section — and
//! the floor may never sit below the minimum, which would declare support for a
//! toolchain that cannot build the lock.
//!
//! Nothing in the build compared them before. Cargo enforces the key against
//! the running compiler and stops there; it has no opinion on whether the
//! number matches its own documentation. The key read `1.97.1` while the prose
//! in two other tracked files said `1.95`, and only a human reading the file
//! ever noticed.

use std::fs;
use std::path::PathBuf;

/// A Rust release as (major, minor). The manifest key carries a patch
/// component and the bisection table carries `.0`; comparing at minor
/// granularity is deliberate, since the evidence is gathered per minor release.
type Release = (u32, u32);

fn parse_release(text: &str) -> Option<Release> {
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

fn render(release: Release) -> String {
    format!("{}.{}", release.0, release.1)
}

fn repo_file(name: &str) -> String {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), name].iter().collect();
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The `rust-version = "X"` key, taken only from column zero so the prose above
/// it — which quotes other releases to explain the distinction — cannot be
/// mistaken for the declaration.
fn declared_floor(manifest: &str) -> Release {
    let line = manifest
        .lines()
        .find(|line| line.starts_with("rust-version"))
        .expect("Cargo.toml should declare rust-version at column zero");
    let quoted = line
        .split('"')
        .nth(1)
        .unwrap_or_else(|| panic!("malformed rust-version line: {line}"));
    parse_release(quoted).unwrap_or_else(|| panic!("unparsable rust-version: {quoted}"))
}

/// The concluding sentence of the bisection comment,
/// `# So X is the technical minimum: …`.
fn measured_minimum(manifest: &str) -> Release {
    let line = manifest
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("# So ") && line.contains("is the technical minimum"))
        .expect("the bisection comment should conclude '# So X is the technical minimum'");
    let version = line
        .strip_prefix("# So ")
        .and_then(|rest| rest.split_whitespace().next())
        .expect("malformed conclusion sentence");
    parse_release(version).unwrap_or_else(|| panic!("unparsable minimum in conclusion: {version}"))
}

/// The bisection table: `#   1.95.0  ok …` / `#   1.94.0  error…`, returned as
/// (release, did_it_build).
fn bisection_table(manifest: &str) -> Vec<(Release, bool)> {
    manifest
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("#   ")?;
            let mut fields = rest.split_whitespace();
            let release = parse_release(fields.next()?)?;
            let outcome = fields.next()?;
            Some((release, outcome == "ok"))
        })
        .collect()
}

/// Every release named in backticks in `CLAUDE.md`'s "Toolchain floor" section,
/// in document order. The section must name both numbers, because naming only
/// one is how a reader ends up believing they are the same.
fn documented_releases(claude_md: &str) -> Vec<Release> {
    let section = claude_md
        .split_once("## Toolchain floor")
        .expect("CLAUDE.md should have a '## Toolchain floor' section")
        .1;
    let section = section
        .split_once("\n## ")
        .map_or(section, |(head, _)| head);

    section
        .split('`')
        .skip(1)
        .step_by(2)
        .filter_map(|inner| {
            let digits = inner
                .trim()
                .trim_start_matches("rust-version = \"")
                .trim_matches('"');
            parse_release(digits)
        })
        .collect()
}

#[test]
fn manifest_and_claude_md_agree_on_both_releases() {
    let manifest = repo_file("Cargo.toml");
    let claude_md = repo_file("CLAUDE.md");

    let declared = declared_floor(&manifest);
    let measured = measured_minimum(&manifest);
    let documented = documented_releases(&claude_md);

    assert!(
        documented.contains(&declared),
        "Cargo.toml declares rust-version = \"{}\" but CLAUDE.md's 'Toolchain \
         floor' section never names it. Releases named there: {documented:?}",
        render(declared),
    );
    assert!(
        documented.contains(&measured),
        "the bisection comment concludes {} is the measured minimum but \
         CLAUDE.md's 'Toolchain floor' section never names it. Releases named \
         there: {documented:?}",
        render(measured),
    );
    // Naming only one of the two is the failure mode this whole file exists
    // for: a reader who sees a single number assumes it is both.
    assert!(
        declared == measured || documented.iter().any(|&r| r != declared),
        "CLAUDE.md names only {}; it must distinguish the declared floor from \
         the measured minimum.",
        render(declared),
    );
}

#[test]
fn the_measured_minimum_matches_its_own_bisection_table() {
    let manifest = repo_file("Cargo.toml");
    let measured = measured_minimum(&manifest);
    let table = bisection_table(&manifest);

    assert!(
        !table.is_empty(),
        "the bisection table in Cargo.toml is empty or no longer parses; it is \
         the only evidence for the measured minimum"
    );

    let ok_at_minimum = table
        .iter()
        .any(|&(release, built)| release == measured && built);
    assert!(
        ok_at_minimum,
        "the comment concludes {} is the measured minimum, but the table has no \
         matching 'ok' row — nothing has actually been built there. \
         Table: {table:?}",
        render(measured),
    );

    // A minimum is only a minimum if the release below it fails.
    let below = (measured.0, measured.1 - 1);
    let fails_below = table
        .iter()
        .any(|&(release, built)| release == below && !built);
    assert!(
        fails_below,
        "the table has no failing row for {}, one minor release below the \
         claimed minimum of {}. Either the minimum is higher than the evidence \
         supports, or the table needs a row for {}.",
        render(below),
        render(measured),
        render(below),
    );

    for &(release, built) in &table {
        assert!(
            !(built && release < measured),
            "the table records {} as building, below the claimed measured \
             minimum of {}.",
            render(release),
            render(measured),
        );
    }
}

#[test]
fn the_declared_floor_is_not_below_what_the_lock_can_build() {
    let manifest = repo_file("Cargo.toml");
    let declared = declared_floor(&manifest);
    let measured = measured_minimum(&manifest);

    // Deliberately `>=`, not `==`. The floor is a support decision and may sit
    // above the minimum — it does today. It may never sit below it: that would
    // promise support for a toolchain the pinned lock cannot build on.
    assert!(
        declared >= measured,
        "rust-version = \"{}\" is below the measured minimum of {}. That \
         declares support for a toolchain this lock cannot build with — cargo \
         would admit the build and it would then fail in a build script.",
        render(declared),
        render(measured),
    );
}
