//! `br detach` — moving an issue out from under its parent.

use crate::common;

use common::cli::{BrWorkspace, extract_json_payload, parse_created_id, run_br};
use serde_json::Value;

#[test]
fn e2e_detaching_every_child_lets_the_epic_close_without_force() {
    let _log = common::test_log("e2e_detaching_every_child_lets_the_epic_close_without_force");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create_epic = run_br(
        &workspace,
        ["create", "The Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(create_epic.status.success());
    let epic_id = parse_created_id(&create_epic.stdout);

    let create_child = run_br(
        &workspace,
        [
            "create",
            "Child one",
            "--type",
            "task",
            "--parent",
            &epic_id,
        ],
        "create_child",
    );
    assert!(
        create_child.status.success(),
        "create child failed: {}",
        create_child.stderr
    );
    let child_id = parse_created_id(&create_child.stdout);
    assert!(
        child_id.contains('.'),
        "precondition: {child_id} should be a dotted child ID"
    );

    let detach = run_br(&workspace, ["detach", &child_id], "detach");
    assert!(detach.status.success(), "detach failed: {}", detach.stderr);

    let closed = run_br(
        &workspace,
        ["close", &epic_id, "--reason", "done"],
        "close_epic",
    );
    assert!(
        closed.status.success(),
        "closing the epic must not need --force: {}",
        closed.stderr
    );
    assert!(
        !closed.stderr.contains("--force"),
        "closing must not have needed --force; got: {}",
        closed.stderr
    );
}

#[test]
fn e2e_detach_renames_a_dotted_child_to_a_flat_id() {
    let _log = common::test_log("e2e_detach_renames_a_dotted_child_to_a_flat_id");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_epic = run_br(
        &workspace,
        ["create", "The Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(create_epic.status.success());
    let epic_id = parse_created_id(&create_epic.stdout);

    let create_child = run_br(
        &workspace,
        [
            "create",
            "Child one",
            "--type",
            "task",
            "--parent",
            &epic_id,
        ],
        "create_child",
    );
    assert!(create_child.status.success());
    let child_id = parse_created_id(&create_child.stdout);
    assert!(
        child_id.contains('.'),
        "precondition: {child_id} is a dotted ID"
    );

    let out = run_br(&workspace, ["detach", &child_id, "--json"], "detach_json");
    assert!(out.status.success(), "detach failed: {}", out.stderr);

    let payload = extract_json_payload(&out.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse detach json");
    assert_eq!(json.len(), 1);

    let new_id = json[0]["new_id"].as_str().expect("new_id in payload");
    assert!(
        !new_id.contains('.'),
        "detached ID must be flat; got {new_id}"
    );
    assert_eq!(json[0]["old_id"].as_str(), Some(child_id.as_str()));
}

#[test]
fn e2e_the_old_id_still_resolves_after_detach() {
    let _log = common::test_log("e2e_the_old_id_still_resolves_after_detach");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_epic = run_br(
        &workspace,
        ["create", "The Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(create_epic.status.success());
    let epic_id = parse_created_id(&create_epic.stdout);

    let create_child = run_br(
        &workspace,
        [
            "create",
            "Child one",
            "--type",
            "task",
            "--parent",
            &epic_id,
        ],
        "create_child",
    );
    assert!(create_child.status.success());
    let child_id = parse_created_id(&create_child.stdout);

    let detach = run_br(&workspace, ["detach", &child_id], "detach");
    assert!(detach.status.success());

    let shown = run_br(&workspace, ["show", &child_id], "show_old_id");
    assert!(
        shown.status.success(),
        "showing the pre-rename ID must still work: {}",
        shown.stderr
    );
    assert!(
        shown.stdout.contains("Child one"),
        "a reference written before the rename must still resolve; got: {}",
        shown.stdout
    );
}

#[test]
fn e2e_detaching_an_issue_with_no_parent_is_a_successful_no_op() {
    let _log = common::test_log("e2e_detaching_an_issue_with_no_parent_is_a_successful_no_op");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_orphan = run_br(
        &workspace,
        ["create", "Standalone", "--type", "task"],
        "create_orphan",
    );
    assert!(create_orphan.status.success());
    let orphan_id = parse_created_id(&create_orphan.stdout);

    let first = run_br(&workspace, ["detach", &orphan_id], "detach_first");
    assert!(first.status.success(), "detach failed: {}", first.stderr);
    assert!(
        first.stdout.contains("no parent"),
        "must say why nothing happened; got: {}",
        first.stdout
    );

    let second = run_br(&workspace, ["detach", &orphan_id], "detach_second");
    assert!(second.status.success(), "detach must be idempotent");
}

#[test]
fn e2e_detach_drops_the_dep_on_a_flat_child_without_renaming_it() {
    let _log = common::test_log("e2e_detach_drops_the_dep_on_a_flat_child_without_renaming_it");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_epic = run_br(
        &workspace,
        ["create", "The Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(create_epic.status.success());
    let epic_id = parse_created_id(&create_epic.stdout);

    let create_flat = run_br(
        &workspace,
        ["create", "Flat child", "--type", "task"],
        "create_flat",
    );
    assert!(create_flat.status.success());
    let flat_id = parse_created_id(&create_flat.stdout);
    assert!(
        !flat_id.contains('.'),
        "precondition: {flat_id} should be a flat ID"
    );

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &flat_id, &epic_id, "--type", "parent-child"],
        "dep_add",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let detach = run_br(&workspace, ["detach", &flat_id], "detach");
    assert!(detach.status.success(), "detach failed: {}", detach.stderr);

    let shown = run_br(&workspace, ["show", &flat_id], "show_flat");
    assert!(shown.status.success());
    assert!(
        shown.stdout.contains("Flat child"),
        "a flat ID makes no hierarchy claim, so detaching must not rename it; got: {}",
        shown.stdout
    );

    let dep_list = run_br(&workspace, ["dep", "list", &flat_id], "dep_list");
    assert!(dep_list.status.success());
    assert!(
        !dep_list.stdout.contains(&epic_id),
        "the parent-child edge must be gone; got: {}",
        dep_list.stdout
    );
}
