//! Guards the licensing invariants.
//!
//! The upstream license is MIT *with an OpenAI/Anthropic rider* that must
//! travel unmodified into derivative works. These tests make it impossible to
//! silently relicense the project or drop the attribution.

use std::fs;

/// Repo-relative paths, by name, that are allowed to name the projects this
/// fork descends from. `NOTICE.md` is the licensed place for that attribution
/// (see `notice_documents_the_fork_lineage_and_the_rider` above); this file
/// asserts its content. Everywhere else in the tree, naming an external
/// project or carrying a path to one is prose rot: a stale detail that leaks
/// the original author's machine or sends a reader of *this* fork to someone
/// else's repo. This is a name allowlist, not a pattern — a pattern (e.g. "any
/// `licensing*.rs`") would silently re-admit any file the sweep that produced
/// this guard removed those references from.
const ALLOWED_FILES: &[&str] = &["NOTICE.md", "tests/licensing.rs"];

/// Substrings that identify an external project or a path onto someone else's
/// machine, rather than describing this fork's own history or invariants.
///
/// Both the snake_case spelling (as it appears in prose, paths, and crate
/// names) and the CamelCase spelling (as it would appear in a Rust
/// identifier, e.g. an enum variant) are listed for `beads_rust` and
/// `beads_viewer` — a prior round of this sweep renamed a snake_case string
/// but left a `KnownDataset::BeadsRust` enum variant behind, which this list
/// would not have caught without the CamelCase form.
const DISALLOWED_PATTERNS: &[&str] = &[
    "beads_rust",
    "BeadsRust",
    "beads_viewer",
    "BeadsViewer",
    "Dicklesworthstone",
    "/data/projects",
    "../beads",
];

#[test]
fn no_upstream_project_references_outside_the_attribution_files() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    // Enumerate git-tracked files rather than walking the filesystem. A raw
    // walk would also sweep up untracked local scratch directories (e.g. SDD
    // planning notes, review-package diffs that themselves quote the old
    // names verbatim) that are not part of the repository at all — `git
    // status --ignored` confirms those are gitignored, not just absent from
    // this list by convention. Tracked-file enumeration also gives us
    // `.beads` exclusion and binary-file skipping for free: `.beads/` holds
    // this project's own issue-tracker data (tracked, but not source, and
    // excluded from this sweep by design) and `git ls-files` still lists any
    // binary fixtures, which are filtered out below the same way `grep -I`
    // would skip them.
    let output = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .expect("git ls-files must run");
    assert!(output.status.success(), "git ls-files failed: {output:?}");

    let mut violations = Vec::new();

    for rel_bytes in output.stdout.split(|&b| b == 0) {
        if rel_bytes.is_empty() {
            continue;
        }
        let rel = String::from_utf8_lossy(rel_bytes).into_owned();

        // `.beads/...` (and, defensively, any nested `.beads/` — none exist
        // today, but this stays correct if one is ever added) is tracked
        // data, not source; the brief's own acceptance grep excludes it too.
        if rel == ".beads" || rel.starts_with(".beads/") || rel.contains("/.beads/") {
            continue;
        }
        if ALLOWED_FILES.contains(&rel.as_str()) {
            continue;
        }

        let Ok(content) = fs::read_to_string(root.join(&rel)) else {
            continue; // binary file (e.g. a fixture .db) — mirrors `grep -I`
        };
        for pattern in DISALLOWED_PATTERNS {
            if content.contains(pattern) {
                violations.push(format!("{rel}: contains {pattern:?}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "found a reference to an external project or a path onto someone \
         else's machine outside the two files licensed to carry it \
         (NOTICE.md, tests/licensing.rs):\n{}",
        violations.join("\n")
    );
}

#[test]
fn license_file_retains_the_openai_anthropic_rider() {
    let license = fs::read_to_string("LICENSE").expect("LICENSE must exist at the repo root");

    assert!(
        license.contains("MIT License (with OpenAI/Anthropic Rider)"),
        "LICENSE lost its title; it must remain byte-identical to upstream"
    );
    assert!(
        license.contains("ADDITIONAL RIDER / RESTRICTION (OpenAI / Anthropic)"),
        "LICENSE lost the rider section"
    );
    assert!(
        license.contains("Copyright (c) 2026 Jeffrey Emanuel"),
        "LICENSE lost the upstream copyright notice"
    );
    // Both notices are load-bearing and neither may displace the other. MIT
    // requires the upstream notice to survive in every copy, and this fork's
    // own modifications are separately copyrightable — so the file carries two
    // lines, not one replaced by the other. NOTICE.md records the same split.
    //
    // Note what this line does NOT do: the rider's operative clauses name
    // Jeffrey Emanuel as the party who can grant permission and seek relief,
    // and a modifications copyright neither transfers nor shares that.
    assert!(
        license.contains("Copyright (c) 2026 Anton Krivenko (modifications)"),
        "LICENSE lost the modifications copyright notice for this fork"
    );
    assert!(
        license.contains("must include this rider provision unmodified"),
        "LICENSE lost the clause requiring the rider to propagate"
    );
}

#[test]
fn cargo_manifest_does_not_claim_plain_mit() {
    let manifest = fs::read_to_string("Cargo.toml").expect("Cargo.toml must exist");

    assert!(
        !manifest.contains(r#"license = "MIT""#),
        "Cargo.toml claims plain MIT, but the license carries a binding rider"
    );
    assert!(
        manifest.contains(r#"license-file = "LICENSE""#),
        "Cargo.toml must point at LICENSE via license-file"
    );
}

#[test]
fn notice_documents_the_fork_lineage_and_the_rider() {
    let notice = fs::read_to_string("NOTICE.md").expect("NOTICE.md must exist at the repo root");

    for needle in [
        "Steve Yegge",
        "Dicklesworthstone",
        "Jeffrey Emanuel",
        "not plain MIT",
    ] {
        assert!(notice.contains(needle), "NOTICE.md must mention {needle:?}");
    }
}

#[test]
fn there_is_no_build_script_and_no_build_dependencies() {
    // THIS CRATE HAS NO BUILD SCRIPT, deliberately.
    //
    // There was one. It stamped a git SHA, branch, rustc version and target
    // triple into `br version` through four `option_env!` reads. It was deleted
    // because those values were redundant where they were visible and wrong
    // where they were not: a released binary's version and its matching tag
    // already identify the commit exactly, while the script emitted no
    // `cargo:rerun-if-changed` for git state, so a dev build's stamped SHA went
    // stale after any commit that did not also touch a source file. That
    // staleness was measured, not assumed.
    //
    // The guard survives its subject because the original hazard is still real.
    // vergen-gix pulls in 46 gix-* crates — a whole Rust git implementation —
    // to supply strings a build script can compute for free, and reintroducing
    // a build script is exactly how it would get back in.
    assert!(
        !std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build.rs")
            .exists(),
        "build.rs reappeared; `br version` deliberately reports only the crate \
         version and the build profile, both of which cargo supplies for free"
    );

    let manifest = std::fs::read_to_string("Cargo.toml").expect("Cargo.toml must exist");

    assert!(
        !manifest.contains("[build-dependencies]"),
        "a [build-dependencies] section reappeared; this crate has no build script"
    );
    // TOML also allows the dotted-table spelling `[build-dependencies.foo]`,
    // which registers a real build dependency (confirmed via `cargo metadata`)
    // without containing the bracketed-section literal above. Without this
    // check, someone could reintroduce a build dependency that way and the
    // first assertion would stay green. Do not delete this as redundant.
    assert!(
        !manifest.contains("[build-dependencies."),
        "a [build-dependencies.<crate>] entry reappeared; this crate has no build script"
    );
}

#[test]
fn manifest_has_no_toon_dependency() {
    // tru (declared as toon_rust) declares its own [build-dependencies]
    // vergen-gix, which was the only remaining path pulling 46 gix-* crates --
    // a whole Rust git implementation -- plus 3 vergen crates into the build
    // graph. The TOON output format was dropped to remove it; keep it dropped.
    let manifest = fs::read_to_string("Cargo.toml").expect("Cargo.toml must exist");

    assert!(
        !manifest.contains("toon_rust"),
        "the toon_rust dependency reappeared; it drags in the gix tree"
    );
    assert!(
        !manifest.contains(r#"package = "tru""#),
        "tru reappeared under a different alias; it drags in the gix tree"
    );
    // The two checks above only catch `toon_rust = { package = "tru", .. }`.
    // A plain `tru = "0.2.3"` trips neither, so match the bare crate name
    // directly. Two spellings have to be covered, exactly as
    // `there_is_no_build_script_and_no_build_dependencies` covers both of its own:
    //   tru = "0.2.3"          -- an inline dependency line
    //   [dependencies.tru]     -- the dotted-table form
    // Substring matching alone is not enough: `contains("tru")` would also
    // fire on unrelated crates that merely end in "tru".
    let declares_tru_directly = manifest.lines().map(str::trim).any(|line| {
        let inline = line
            .strip_prefix("tru")
            .is_some_and(|rest| rest.trim_start().starts_with(['=', '.']));
        let dotted_table = line.starts_with('[') && line.ends_with(".tru]");
        inline || dotted_table
    });
    assert!(
        !declares_tru_directly,
        "tru reappeared under its own name; it drags in the gix tree"
    );
}
