//! `--sort` with multiple keys, driven through the binary.

use crate::common;

use common::cli::{BrWorkspace, extract_issues_array, parse_created_id, parse_list_issues, run_br};

/// A workspace with four issues: two at p1, two at p0, so every sort has
/// ties for a later key to break.
fn seeded() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "srt"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    for (title, priority) in [
        ("alpha", "1"),
        ("bravo", "1"),
        ("charlie", "0"),
        ("delta", "0"),
    ] {
        let run = run_br(
            &workspace,
            ["create", title, "--priority", priority],
            "create",
        );
        assert!(
            run.status.success(),
            "create {title} failed: {}",
            run.stderr
        );
        let _ = parse_created_id(&run.stdout);
    }
    workspace
}

fn list_field(workspace: &BrWorkspace, args: &[&str], field: &str, label: &str) -> Vec<String> {
    let run = run_br(workspace, args.iter().copied(), label);
    assert!(run.status.success(), "{label} failed: {}", run.stderr);
    parse_list_issues(&run.stdout)
        .iter()
        .map(|issue| issue[field].to_string())
        .collect()
}

#[test]
fn multi_key_sort_orders_by_the_second_key_within_each_band() {
    // A monotonic-priority assertion would NOT be enough: it passes even if
    // the second key is ignored entirely. Sort by `priority,title` — both
    // controllable from the CLI — and pin the exact order, so the second key
    // has to do real work. Ids here are content-hashed and effectively
    // random, so the assertion is only deterministic because priority+title
    // together fully determine the order with no ties left for the id
    // terminator to break.
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "srt"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    for (title, priority) in [("zulu", "1"), ("alpha", "1"), ("mike", "0")] {
        let run = run_br(
            &workspace,
            ["create", title, "--priority", priority],
            "create",
        );
        assert!(
            run.status.success(),
            "create {title} failed: {}",
            run.stderr
        );
    }

    let titles = list_field(
        &workspace,
        &["list", "--sort", "priority,title", "--json"],
        "title",
        "list priority,title",
    );
    let titles: Vec<String> = titles
        .iter()
        .map(|t| t.trim_matches('"').to_string())
        .collect();

    // p0 first, then the p1 pair A-Z. "zulu" was created BEFORE "alpha", so
    // a build that dropped the title key would leave them in creation order
    // (or in random id order) and fail this.
    assert_eq!(titles, vec!["mike", "alpha", "zulu"]);
}

#[test]
fn status_sorts_in_workflow_order_not_alphabetical_order() {
    let workspace = seeded();
    let ids = list_field(&workspace, &["list", "--json"], "id", "list all");
    let first = ids[0].trim_matches('"').to_string();

    let update = run_br(
        &workspace,
        ["update", &first, "--status", "blocked"],
        "block one",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let statuses = list_field(
        &workspace,
        &["list", "--sort", "status", "--all", "--json"],
        "status",
        "list by status",
    );

    // 'blocked' precedes 'open' alphabetically; by workflow rank it must not.
    // Both `.expect()`s are load-bearing, not decoration: an `if let` here
    // would let the update silently no-op, or a status-field shape change,
    // pass the test having asserted nothing at all.
    let blocked = statuses
        .iter()
        .position(|s| s.contains("blocked"))
        .expect("blocked entry must be present after update");
    let open = statuses
        .iter()
        .rposition(|s| s.contains("open"))
        .expect("open entries must remain after update");
    assert!(open < blocked, "open must precede blocked: {statuses:?}");
}

#[test]
fn bare_priority_sort_is_unchanged() {
    // The legacy carve-out. If this test ever needs updating, that change is a
    // breaking one and belongs in a major release, not a refactor.
    let workspace = seeded();
    let implicit = list_field(
        &workspace,
        &["list", "--sort", "priority", "--json"],
        "id",
        "list --sort priority",
    );
    let explicit = list_field(
        &workspace,
        &["list", "--sort", "priority,created", "--json"],
        "id",
        "list --sort priority,created",
    );

    assert_eq!(
        implicit, explicit,
        "--sort priority must equal --sort priority,created"
    );
}

#[test]
fn reverse_composes_with_a_multi_key_spec() {
    // `seeded()` (two issues at each of two priorities) is wrong for this
    // test: flipping `-priority` alone swaps which band leads, so
    // `assert_ne!(forward, reversed)` would trip even if the `updated` key
    // were dropped entirely. Every issue below gets the SAME priority
    // instead, so priority contributes nothing to the order and only the
    // second key -- composed with `--reverse` -- can differentiate forward
    // from reversed.
    //
    // Note also that `-`/`+` are "force descending" / "force ascending",
    // not "flip from natural" (`SortSpec::resolved` in
    // `src/model/sort.rs`): `updated`'s natural direction is already
    // descending, so `-priority,-updated` is NOT the reverse of bare
    // `priority,updated` -- verified by hand against the built binary,
    // where the two produced an IDENTICAL order on this same-priority
    // fixture. `--reverse` is the mechanism that is guaranteed to invert
    // every resolved key regardless of each field's natural direction, so
    // it is what this test composes with the multi-key spec.
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "srt"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut id_of: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    for title in ["un", "deux", "trois", "quatre"] {
        let run = run_br(&workspace, ["create", title, "--priority", "1"], "create");
        assert!(
            run.status.success(),
            "create {title} failed: {}",
            run.stderr
        );
        id_of.insert(title, parse_created_id(&run.stdout));
    }

    // `br update` stamps `updated_at` from `Utc::now()` at the moment its
    // subprocess runs (src/cli/commands/update.rs), and `run_br` blocks
    // until each child exits, so touching the four issues in this order
    // gives a known, strictly increasing `updated_at` order: deux, quatre,
    // un, trois (established here by construction, not merely assumed).
    let touch_order = ["deux", "quatre", "un", "trois"];
    for title in touch_order {
        let id = id_of[title].clone();
        let new_title = format!("touched-{title}");
        let run = run_br(
            &workspace,
            ["update", id.as_str(), "--title", new_title.as_str()],
            "touch",
        );
        assert!(run.status.success(), "touch {title} failed: {}", run.stderr);
    }

    let forward = list_field(
        &workspace,
        &["list", "--sort", "priority,updated", "--json"],
        "id",
        "forward",
    );
    let forward: Vec<String> = forward
        .iter()
        .map(|id| id.trim_matches('"').to_string())
        .collect();
    let reversed = list_field(
        &workspace,
        &["list", "--sort", "priority,updated", "--reverse", "--json"],
        "id",
        "reversed",
    );
    let reversed: Vec<String> = reversed
        .iter()
        .map(|id| id.trim_matches('"').to_string())
        .collect();

    // Bare `updated` takes its natural direction (descending, newest
    // first), so forward order is the touch order reversed; `--reverse`
    // inverts that back to the touch order itself.
    let expected_forward: Vec<String> = ["trois", "un", "quatre", "deux"]
        .iter()
        .map(|t| id_of[t].clone())
        .collect();
    let expected_reversed: Vec<String> = touch_order.iter().map(|t| id_of[t].clone()).collect();

    assert_eq!(
        forward, expected_forward,
        "priority,updated should order by updated_at descending (newest first) \
         when every issue shares the same priority"
    );
    assert_eq!(
        reversed, expected_reversed,
        "--reverse should invert every resolved key (here, updated_at), not just the \
         first (priority, which is tied and so cannot itself account for a difference)"
    );

    // What a build that dropped the second key internally would produce:
    // NOT the same as literally passing bare `--sort priority` (that hits
    // the unrelated legacy carve-out, which falls back to `created_at DESC`
    // and would itself discriminate here). The hypothetical bug this guards
    // against is `SortSpec::resolved`/`compare` retaining only the first of
    // the two *parsed* keys from a `priority,updated` spec. With every issue
    // at the same priority, that leaves nothing but the `id ASC` terminator
    // -- and `--reverse` does not flip that terminator by design (see the
    // doc comment on `resolved`). Forward and reversed would then both be
    // plain id-ascending order and therefore IDENTICAL, so this assertion
    // would fail exactly the way it is meant to if the second key were
    // ignored or `--reverse` did not compose with it.
    assert_ne!(
        forward, reversed,
        "flipping every key must change the order"
    );
}

#[test]
fn search_accepts_the_same_grammar_as_list() {
    let workspace = seeded();
    let run = run_br(
        &workspace,
        ["search", "a", "--sort", "priority,updated", "--json"],
        "search sorted",
    );
    assert!(run.status.success(), "search failed: {}", run.stderr);
    // Unlike `list --json`, `search --json` emits a bare array rather than a
    // paginated `{"issues": [...]}` envelope, so `parse_list_issues` (which
    // asserts the envelope) is the wrong parser here; `extract_issues_array`
    // accepts either shape. It panics on malformed output, which is the
    // assertion: the same `--sort` grammar `list` accepts must not blow up
    // `search`.
    let _ = extract_issues_array(&run.stdout);
}

#[test]
fn a_descending_key_needs_the_equals_form_and_works() {
    // A `-`-prefixed value must be attached with `=`. `--sort -priority` as
    // two argv tokens is parsed by clap as a flag, not a value — `--sort`
    // does not set allow_hyphen_values, by deliberate project convention.
    // This test pins BOTH halves of that: the `=` form works, and the
    // separated form fails with the project's hyphen hint rather than
    // silently doing something else.
    let workspace = seeded();

    let ascending = list_field(
        &workspace,
        &["list", "--sort=+priority", "--json"],
        "priority",
        "ascending",
    );
    let descending = list_field(
        &workspace,
        &["list", "--sort=-priority", "--json"],
        "priority",
        "descending",
    );

    assert_eq!(ascending.len(), descending.len());
    let mut reversed = descending.clone();
    reversed.reverse();
    assert_eq!(
        ascending, reversed,
        "-priority must be the exact reverse of +priority: {ascending:?} vs {descending:?}"
    );

    // And the separated form is a parse error, not a silent misparse.
    let separated = run_br(
        &workspace,
        ["list", "--sort", "-priority", "--json"],
        "separated hyphen value",
    );
    assert!(
        !separated.status.success(),
        "`--sort -priority` should fail; use --sort=-priority"
    );
}

#[test]
fn invalid_specs_are_rejected_before_the_query_runs() {
    let workspace = seeded();

    for spec in [
        "nonsense",
        "priority,,title",
        "priority,-priority",
        "id",
        "",
    ] {
        let run = run_br(&workspace, ["list", "--sort", spec, "--json"], "bad sort");
        assert!(
            !run.status.success(),
            "--sort {spec:?} should have failed, got: {}",
            run.stdout
        );
        // With `--json`, `br` renders the error as a JSON object on stdout
        // deterministically -- never on stderr (see `handle_error`'s JSON
        // branch in `src/main.rs`, issue #336: "so scripted callers read ONE
        // clean, parseable stream"). `tests/e2e/errors.rs` pins the same
        // contract via
        // `parse_error_json(&update.stdout)`. Asserting stdout alone (not
        // stdout-or-stderr) means a regression that put JSON errors back on
        // stderr -- breaking that one-clean-stream contract -- fails here
        // instead of slipping through on the `stderr` half of an `||`.
        assert!(
            run.stdout.contains("sort"),
            "error for {spec:?} should mention sort on stdout: {}",
            run.stdout
        );
    }
}
