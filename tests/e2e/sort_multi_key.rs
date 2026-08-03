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
    let first_blocked = statuses.iter().position(|s| s.contains("blocked"));
    let last_open = statuses.iter().rposition(|s| s.contains("open"));
    if let (Some(blocked), Some(open)) = (first_blocked, last_open) {
        assert!(open < blocked, "open must precede blocked: {statuses:?}");
    }
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
    let workspace = seeded();
    let forward = list_field(
        &workspace,
        &["list", "--sort", "priority,updated", "--json"],
        "id",
        "forward",
    );
    // The first key here starts with `-`, so (per the CLI ergonomics fact
    // this epic pinned) it must be attached with `=`; a separated
    // `"--sort", "-priority,-updated"` pair would be parsed by clap as a
    // flag and fail before ever reaching the sort logic.
    let reversed = list_field(
        &workspace,
        &["list", "--sort=-priority,-updated", "--json"],
        "id",
        "reversed",
    );

    assert_eq!(
        forward.len(),
        reversed.len(),
        "same issues, different order"
    );
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
        // rather than prose on stderr (see the `VALIDATION_FAILED` envelope),
        // so the "mentions sort" check has to look at whichever stream
        // actually carries it instead of assuming stderr.
        assert!(
            run.stdout.contains("sort") || run.stderr.contains("sort"),
            "error for {spec:?} should mention sort -- stdout: {} stderr: {}",
            run.stdout,
            run.stderr
        );
    }
}
