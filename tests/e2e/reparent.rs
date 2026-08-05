//! `br update --parent` — renumber on attach, refuse on clear.
//!
//! The invariant this project maintains is that a dotted prefix always names
//! the real parent, and having a parent always implies a dotted ID. Before
//! this enforcement existed, `br update <child> --parent <new-epic>` moved
//! the `parent-child` dependency without renaming the ID, so the old epic
//! kept an open dot-notation child it could never close without `--force`.

use crate::common;

use beads::model::DependencyType;
use common::cli::{BrWorkspace, extract_json_payload, parse_created_id, run_br};
use common::fixtures::{IssueBuilder, dependency};
use serde_json::Value;
use std::fs;

#[test]
fn e2e_reparenting_renumbers_under_the_new_parent() {
    let _log = common::test_log("e2e_reparenting_renumbers_under_the_new_parent");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic1 = run_br(
        &workspace,
        ["create", "Epic one", "--type", "epic"],
        "create_epic1",
    );
    assert!(epic1.status.success());
    let epic1_id = parse_created_id(&epic1.stdout);

    let epic2 = run_br(
        &workspace,
        ["create", "Epic two", "--type", "epic"],
        "create_epic2",
    );
    assert!(epic2.status.success());
    let epic2_id = parse_created_id(&epic2.stdout);

    let create_child = run_br(
        &workspace,
        ["create", "Child", "--type", "task", "--parent", &epic1_id],
        "create_child",
    );
    assert!(create_child.status.success());
    let child_id = parse_created_id(&create_child.stdout);

    let out = run_br(
        &workspace,
        ["update", &child_id, "--parent", &epic2_id, "--json"],
        "update_parent",
    );
    assert!(out.status.success(), "update failed: {}", out.stderr);

    let payload = extract_json_payload(&out.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse update json");
    assert_eq!(json.len(), 1);

    let new_id = json[0]["id"].as_str().expect("id in payload");
    assert!(
        new_id.starts_with(&format!("{epic2_id}.")),
        "the ID must name its real parent; got {new_id} under {epic2_id}"
    );
    assert_eq!(
        json[0]["new_id"].as_str(),
        Some(new_id),
        "new_id must be reported alongside the renumbered id; got: {payload}"
    );
}

#[test]
fn e2e_the_original_epic_closes_after_its_child_moves_away() {
    let _log = common::test_log("e2e_the_original_epic_closes_after_its_child_moves_away");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic1 = run_br(
        &workspace,
        ["create", "Epic one", "--type", "epic"],
        "create_epic1",
    );
    assert!(epic1.status.success());
    let epic1_id = parse_created_id(&epic1.stdout);

    let epic2 = run_br(
        &workspace,
        ["create", "Epic two", "--type", "epic"],
        "create_epic2",
    );
    assert!(epic2.status.success());
    let epic2_id = parse_created_id(&epic2.stdout);

    let create_child = run_br(
        &workspace,
        ["create", "Child", "--type", "task", "--parent", &epic1_id],
        "create_child",
    );
    assert!(create_child.status.success());
    let child_id = parse_created_id(&create_child.stdout);

    let reparent = run_br(
        &workspace,
        ["update", &child_id, "--parent", &epic2_id],
        "reparent",
    );
    assert!(
        reparent.status.success(),
        "reparent failed: {}",
        reparent.stderr
    );

    // This is the original bug: `epic1` has no children by any dependency
    // measure once the child moved to `epic2`, and must close without
    // `--force` and without the dot-notation-child message.
    let closed = run_br(
        &workspace,
        ["close", &epic1_id, "--reason", "done"],
        "close_epic1",
    );
    assert!(
        closed.status.success(),
        "closing epic1 must not need --force: {}",
        closed.stderr
    );
    assert!(
        !closed.stderr.contains("--force"),
        "closing must not have needed --force; got: {}",
        closed.stderr
    );
    assert!(
        !closed.stderr.contains("open dot-notation child"),
        "epic1 has no children by any measure; got: {}",
        closed.stderr
    );
}

#[test]
fn e2e_clearing_the_parent_of_a_dotted_id_is_refused_and_names_detach() {
    let _log =
        common::test_log("e2e_clearing_the_parent_of_a_dotted_id_is_refused_and_names_detach");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic = run_br(
        &workspace,
        ["create", "Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(epic.status.success());
    let epic_id = parse_created_id(&epic.stdout);

    let create_child = run_br(
        &workspace,
        ["create", "Child", "--type", "task", "--parent", &epic_id],
        "create_child",
    );
    assert!(create_child.status.success());
    let child_id = parse_created_id(&create_child.stdout);
    assert!(
        child_id.contains('.'),
        "precondition: {child_id} should be a dotted ID"
    );

    let out = run_br(
        &workspace,
        ["update", &child_id, "--parent", ""],
        "clear_parent",
    );

    assert!(!out.status.success(), "must not silently rename");
    assert!(
        out.stderr.contains("br detach"),
        "the error must point at the command that does this properly; got: {}",
        out.stderr
    );

    // And the refusal must be a no-op: the dep and the ID are untouched.
    let shown = run_br(&workspace, ["show", &child_id], "show_after_refusal");
    assert!(shown.status.success());
}

#[test]
fn e2e_clearing_the_parent_of_a_flat_id_still_works() {
    let _log = common::test_log("e2e_clearing_the_parent_of_a_flat_id_still_works");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic = run_br(
        &workspace,
        ["create", "Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(epic.status.success());
    let epic_id = parse_created_id(&epic.stdout);

    // `dep add` now renumbers a `parent-child` target the same way `update
    // --parent` does (that is this task's whole point), so there is no
    // longer a CLI path that leaves an issue with a flat ID and a parent.
    // Build that shape the only way it can still arise -- importing JSONL,
    // which is also how legacy repositories and hand-edited files reach it.
    let flat_id = "zz-flatparent";
    let mut flat_child = IssueBuilder::new("Flat child").with_id(flat_id).build();
    let mut edge = dependency(flat_id, &epic_id);
    edge.dep_type = DependencyType::ParentChild;
    flat_child.dependencies.push(edge);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let mut contents = fs::read_to_string(&jsonl_path).unwrap_or_default();
    contents.push_str(&serde_json::to_string(&flat_child).expect("serialize flat child fixture"));
    contents.push('\n');
    fs::write(&jsonl_path, contents).expect("write flat child fixture");

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "import_flat_child",
    );
    assert!(import.status.success(), "import failed: {}", import.stderr);
    assert!(
        !flat_id.contains('.'),
        "precondition: {flat_id} should be a flat ID"
    );

    // Confirm the fixture actually carries the edge before acting on it --
    // `update --parent ""` on an already-parentless issue is a documented
    // no-op, so without this check a JSONL import that silently dropped the
    // dependency would let this test pass vacuously.
    let dep_list_before = run_br(&workspace, ["dep", "list", flat_id], "dep_list_before");
    assert!(dep_list_before.status.success());
    assert!(
        dep_list_before.stdout.contains(&epic_id),
        "precondition: the imported fixture must carry the parent-child edge; got: {}",
        dep_list_before.stdout
    );

    let clear = run_br(
        &workspace,
        ["update", flat_id, "--parent", ""],
        "clear_parent",
    );
    assert!(
        clear.status.success(),
        "clearing a flat parent must succeed: {}",
        clear.stderr
    );

    let dep_list = run_br(&workspace, ["dep", "list", flat_id], "dep_list");
    assert!(dep_list.status.success());
    assert!(
        !dep_list.stdout.contains(&epic_id),
        "the parent-child edge must be gone; got: {}",
        dep_list.stdout
    );
}

#[test]
fn e2e_reparenting_carries_the_grandchildren() {
    let _log = common::test_log("e2e_reparenting_carries_the_grandchildren");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic1 = run_br(
        &workspace,
        ["create", "Epic one", "--type", "epic"],
        "create_epic1",
    );
    assert!(epic1.status.success());
    let epic1_id = parse_created_id(&epic1.stdout);

    let epic2 = run_br(
        &workspace,
        ["create", "Epic two", "--type", "epic"],
        "create_epic2",
    );
    assert!(epic2.status.success());
    let epic2_id = parse_created_id(&epic2.stdout);

    let create_child = run_br(
        &workspace,
        ["create", "Child", "--type", "task", "--parent", &epic1_id],
        "create_child",
    );
    assert!(create_child.status.success());
    let child_id = parse_created_id(&create_child.stdout);

    let create_grandchild = run_br(
        &workspace,
        [
            "create",
            "Grandchild",
            "--type",
            "task",
            "--parent",
            &child_id,
        ],
        "create_grandchild",
    );
    assert!(create_grandchild.status.success());
    let grandchild_id = parse_created_id(&create_grandchild.stdout);
    let grandchild_suffix = grandchild_id
        .strip_prefix(&child_id)
        .expect("grandchild id is child id + suffix")
        .to_string();

    let reparent = run_br(
        &workspace,
        ["update", &child_id, "--parent", &epic2_id, "--json"],
        "reparent",
    );
    assert!(
        reparent.status.success(),
        "reparent failed: {}",
        reparent.stderr
    );
    let payload = extract_json_payload(&reparent.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse update json");
    let new_child_id = json[0]["id"].as_str().expect("id in payload").to_string();

    // `rename_issue` cascades through the whole subtree: the grandchild's
    // row is rewritten in place to `<new_child_id><suffix>` rather than
    // resolved via `former_ids` (that fallback only redirects the directly
    // renamed issue, not every descendant carried along with it -- see
    // `detach.rs`'s equivalent test). So the grandchild moved with its
    // parent, but it must now be looked up under its new, spliced id.
    let new_grandchild_id = format!("{new_child_id}{grandchild_suffix}");
    let shown = run_br(&workspace, ["show", &new_grandchild_id], "show_grandchild");
    assert!(
        shown.status.success(),
        "the grandchild must have moved with its parent: {}",
        shown.stderr
    );
    assert!(shown.stdout.contains("Grandchild"), "got: {}", shown.stdout);
}

#[test]
fn e2e_last_touched_id_after_reparent_is_the_new_id() {
    let _log = common::test_log("e2e_last_touched_id_after_reparent_is_the_new_id");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic1 = run_br(
        &workspace,
        ["create", "Epic one", "--type", "epic"],
        "create_epic1",
    );
    assert!(epic1.status.success());
    let epic1_id = parse_created_id(&epic1.stdout);

    let epic2 = run_br(
        &workspace,
        ["create", "Epic two", "--type", "epic"],
        "create_epic2",
    );
    assert!(epic2.status.success());
    let epic2_id = parse_created_id(&epic2.stdout);

    let create_child = run_br(
        &workspace,
        ["create", "Child", "--type", "task", "--parent", &epic1_id],
        "create_child",
    );
    assert!(create_child.status.success());
    let child_id = parse_created_id(&create_child.stdout);

    let out = run_br(
        &workspace,
        ["update", &child_id, "--parent", &epic2_id, "--json"],
        "reparent",
    );
    assert!(out.status.success(), "reparent failed: {}", out.stderr);
    let payload = extract_json_payload(&out.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse update json");
    let new_id = json[0]["id"].as_str().expect("id in payload").to_string();

    // A later bare-ID command (no explicit ID) must target the renamed
    // issue, not the tombstone left at the vacated ID. `--claim` is a
    // convenient no-conflict mutation to prove the resolved target.
    let claim = run_br(
        &workspace,
        ["update", "--claim", "--json"],
        "claim_last_touched",
    );
    assert!(claim.status.success(), "claim failed: {}", claim.stderr);
    let claim_payload = extract_json_payload(&claim.stdout);
    let claim_json: Vec<Value> = serde_json::from_str(&claim_payload).expect("parse claim json");
    assert_eq!(
        claim_json[0]["id"].as_str(),
        Some(new_id.as_str()),
        "the last-touched id must be the renamed id, not the vacated one; got: {claim_payload}"
    );
}

#[test]
fn e2e_a_second_reparent_into_the_same_parent_succeeds() {
    let _log = common::test_log("e2e_a_second_reparent_into_the_same_parent_succeeds");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic = run_br(
        &workspace,
        ["create", "Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(epic.status.success());
    let epic_id = parse_created_id(&epic.stdout);

    // One pre-existing child under the epic, so the epic's child counter is
    // already at 1 before either reparent below.
    let existing_child = run_br(
        &workspace,
        [
            "create",
            "Existing child",
            "--type",
            "task",
            "--parent",
            &epic_id,
        ],
        "create_existing_child",
    );
    assert!(existing_child.status.success());

    let other_epic = run_br(
        &workspace,
        ["create", "Other epic", "--type", "epic"],
        "create_other_epic",
    );
    assert!(other_epic.status.success());
    let other_epic_id = parse_created_id(&other_epic.stdout);

    let first_movable = run_br(
        &workspace,
        [
            "create",
            "First movable",
            "--type",
            "task",
            "--parent",
            &other_epic_id,
        ],
        "create_first_movable",
    );
    assert!(first_movable.status.success());
    let first_movable_id = parse_created_id(&first_movable.stdout);

    let second_movable = run_br(
        &workspace,
        [
            "create",
            "Second movable",
            "--type",
            "task",
            "--parent",
            &other_epic_id,
        ],
        "create_second_movable",
    );
    assert!(second_movable.status.success());
    let second_movable_id = parse_created_id(&second_movable.stdout);

    let first_reparent = run_br(
        &workspace,
        ["update", &first_movable_id, "--parent", &epic_id],
        "first_reparent",
    );
    assert!(
        first_reparent.status.success(),
        "first reparent failed: {}",
        first_reparent.stderr
    );

    // The bug: nothing bumped the target parent's child counter on attach,
    // so this second, independent reparent into the SAME parent recomputed
    // the exact slot the first reparent just took and collided with it --
    // `"<epic>.2 already exists"`.
    let second_reparent = run_br(
        &workspace,
        ["update", &second_movable_id, "--parent", &epic_id],
        "second_reparent",
    );
    assert!(
        second_reparent.status.success(),
        "a second reparent into the same parent must succeed: {}",
        second_reparent.stderr
    );
}

// `br dep add X Y --type parent-child` and `br dep remove` on a
// `parent-child` edge -- the same rule as `br update --parent` above, under
// the `dep` command's name. They must behave identically or the invariant
// has a hole with a different spelling.

#[test]
fn e2e_dep_add_parent_child_renumbers_the_same_way_update_does() {
    let _log = common::test_log("e2e_dep_add_parent_child_renumbers_the_same_way_update_does");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic = run_br(
        &workspace,
        ["create", "Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(epic.status.success());
    let epic_id = parse_created_id(&epic.stdout);

    let flat = run_br(
        &workspace,
        ["create", "Loose issue", "--type", "task"],
        "create_flat",
    );
    assert!(flat.status.success());
    let flat_id = parse_created_id(&flat.stdout);

    let out = run_br(
        &workspace,
        [
            "dep",
            "add",
            &flat_id,
            &epic_id,
            "--type",
            "parent-child",
            "--json",
        ],
        "dep_add",
    );
    assert!(out.status.success(), "dep add failed: {}", out.stderr);

    let payload: Value =
        serde_json::from_str(&extract_json_payload(&out.stdout)).expect("parse dep add json");
    let new_id = payload["new_id"].as_str().expect("new_id in payload");
    assert!(
        new_id.starts_with(&format!("{epic_id}.")),
        "dep add must renumber exactly as update --parent does; got {new_id}"
    );
    assert_eq!(
        payload["issue_id"].as_str(),
        Some(new_id),
        "issue_id must report the post-renumber id, not the requested one; got: {payload}"
    );
}

#[test]
fn e2e_dep_remove_on_a_parent_child_edge_is_refused_and_names_detach() {
    let _log =
        common::test_log("e2e_dep_remove_on_a_parent_child_edge_is_refused_and_names_detach");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic = run_br(
        &workspace,
        ["create", "Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(epic.status.success());
    let epic_id = parse_created_id(&epic.stdout);

    let create_child = run_br(
        &workspace,
        ["create", "Child", "--type", "task", "--parent", &epic_id],
        "create_child",
    );
    assert!(create_child.status.success());
    let child_id = parse_created_id(&create_child.stdout);
    assert!(
        child_id.contains('.'),
        "precondition: {child_id} should be a dotted ID"
    );

    let out = run_br(
        &workspace,
        ["dep", "remove", &child_id, &epic_id],
        "dep_remove_refused",
    );

    assert!(!out.status.success(), "must not silently drop the edge");
    assert!(
        out.stderr.contains("br detach"),
        "the error must point at the command that does this properly; got: {}",
        out.stderr
    );

    // And the refusal must be a no-op: the edge is untouched.
    let dep_list = run_br(
        &workspace,
        ["dep", "list", &child_id],
        "dep_list_after_refusal",
    );
    assert!(dep_list.status.success());
    assert!(
        dep_list.stdout.contains(&epic_id),
        "the parent-child edge must still be there; got: {}",
        dep_list.stdout
    );
}

#[test]
fn e2e_dep_remove_still_works_on_ordinary_edges() {
    let _log = common::test_log("e2e_dep_remove_still_works_on_ordinary_edges");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let a = run_br(&workspace, ["create", "A", "--type", "task"], "create_a");
    assert!(a.status.success());
    let a_id = parse_created_id(&a.stdout);

    let b = run_br(&workspace, ["create", "B", "--type", "task"], "create_b");
    assert!(b.status.success());
    let b_id = parse_created_id(&b.stdout);

    let add = run_br(
        &workspace,
        ["dep", "add", &a_id, &b_id, "--type", "blocks"],
        "dep_add_blocks",
    );
    assert!(add.status.success(), "dep add failed: {}", add.stderr);

    let remove = run_br(&workspace, ["dep", "remove", &a_id, &b_id], "dep_remove");
    assert!(
        remove.status.success(),
        "removing an ordinary edge must still work: {}",
        remove.stderr
    );

    let dep_list = run_br(&workspace, ["dep", "list", &a_id], "dep_list");
    assert!(dep_list.status.success());
    assert!(
        !dep_list.stdout.contains(&b_id),
        "the blocks edge must be gone; got: {}",
        dep_list.stdout
    );
}

#[test]
fn e2e_dep_remove_on_a_flat_child_still_works() {
    let _log = common::test_log("e2e_dep_remove_on_a_flat_child_still_works");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic = run_br(
        &workspace,
        ["create", "Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(epic.status.success());
    let epic_id = parse_created_id(&epic.stdout);

    // Once `dep add` renumbers, no CLI path can produce a flat issue that has
    // a parent any more -- so this fixture has to be imported. That is also
    // the only way this shape arrives in reality: legacy repositories and
    // hand-edited JSONL.
    let flat_id = "zz-flatchild";
    let mut flat_child = IssueBuilder::new("Flat child").with_id(flat_id).build();
    let mut edge = dependency(flat_id, &epic_id);
    edge.dep_type = DependencyType::ParentChild;
    flat_child.dependencies.push(edge);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let mut contents = fs::read_to_string(&jsonl_path).unwrap_or_default();
    contents.push_str(&serde_json::to_string(&flat_child).expect("serialize flat child fixture"));
    contents.push('\n');
    fs::write(&jsonl_path, contents).expect("write flat child fixture");

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "import_flat_child",
    );
    assert!(import.status.success(), "import failed: {}", import.stderr);
    assert!(
        !flat_id.contains('.'),
        "precondition: {flat_id} should be a flat ID"
    );

    // Confirm the fixture actually carries the edge before acting on it --
    // `dep remove` on a non-existent edge still exits 0 (`status:
    // "not_found"`), so without this check a JSONL import that silently
    // dropped the dependency would let this test pass vacuously.
    let dep_list_before = run_br(&workspace, ["dep", "list", flat_id], "dep_list_before");
    assert!(dep_list_before.status.success());
    assert!(
        dep_list_before.stdout.contains(&epic_id),
        "precondition: the imported fixture must carry the parent-child edge; got: {}",
        dep_list_before.stdout
    );

    let remove = run_br(
        &workspace,
        ["dep", "remove", flat_id, &epic_id, "--json"],
        "dep_remove_flat",
    );
    assert!(
        remove.status.success(),
        "a flat ID makes no hierarchy claim, so removing its parent edge is coherent \
         and must not be refused: {}",
        remove.stderr
    );
    let remove_payload: Value =
        serde_json::from_str(&extract_json_payload(&remove.stdout)).expect("parse dep remove json");
    assert_eq!(
        remove_payload["action"], "removed",
        "the edge must actually have been removed, not merely absent already; got: {}",
        remove.stdout
    );

    let dep_list = run_br(&workspace, ["dep", "list", flat_id], "dep_list");
    assert!(dep_list.status.success());
    assert!(
        !dep_list.stdout.contains(&epic_id),
        "the parent-child edge must be gone; got: {}",
        dep_list.stdout
    );
}

#[test]
fn e2e_dep_add_parent_child_rejects_metadata_instead_of_silently_dropping_it() {
    let _log = common::test_log(
        "e2e_dep_add_parent_child_rejects_metadata_instead_of_silently_dropping_it",
    );
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic = run_br(
        &workspace,
        ["create", "Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(epic.status.success());
    let epic_id = parse_created_id(&epic.stdout);

    let flat = run_br(
        &workspace,
        ["create", "Loose issue", "--type", "task"],
        "create_flat",
    );
    assert!(flat.status.success());
    let flat_id = parse_created_id(&flat.stdout);

    // `attach_to_parent` sets the edge via `set_parent_in_tx`, which has no
    // metadata column -- unlike the generic `add_dependency_with_metadata`
    // path every other `--type` still uses. Silently dropping `--metadata`
    // here would be silent data loss, so this combination must be refused
    // rather than accepted with the metadata discarded.
    let out = run_br(
        &workspace,
        [
            "dep",
            "add",
            &flat_id,
            &epic_id,
            "--type",
            "parent-child",
            "--metadata",
            "{\"note\":\"should not be silently dropped\"}",
        ],
        "dep_add_parent_child_metadata",
    );
    assert!(
        !out.status.success(),
        "must not silently accept and discard --metadata"
    );
    assert!(
        out.stderr.contains("--metadata"),
        "the error must name the offending flag; got: {}",
        out.stderr
    );

    // And the refusal must be a no-op: no edge, no rename.
    assert!(
        !flat_id.contains('.'),
        "the refused add must not have renumbered the issue"
    );
    let dep_list = run_br(
        &workspace,
        ["dep", "list", &flat_id],
        "dep_list_after_refusal",
    );
    assert!(dep_list.status.success());
    assert!(
        !dep_list.stdout.contains(&epic_id),
        "the refusal must not have created the edge; got: {}",
        dep_list.stdout
    );
}

#[test]
fn e2e_create_with_a_parent_child_dep_is_refused_and_names_the_parent_flag() {
    let _log =
        common::test_log("e2e_create_with_a_parent_child_dep_is_refused_and_names_the_parent_flag");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic = run_br(
        &workspace,
        ["create", "Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(epic.status.success());
    let epic_id = parse_created_id(&epic.stdout);

    // `--deps parent-child:<epic>` would mint a flat ID while claiming a
    // parent in the same breath -- the exact state the invariant forbids.
    // `--parent` already mints the issue as `<epic>.n` correctly.
    let out = run_br(
        &workspace,
        [
            "create",
            "Child",
            "--deps",
            &format!("parent-child:{epic_id}"),
        ],
        "create_with_parent_child_dep",
    );

    assert!(
        !out.status.success(),
        "this would create a flat ID with a parent"
    );
    assert!(
        out.stderr.contains("--parent"),
        "the error must name the flag that does this correctly; got: {}",
        out.stderr
    );
}

#[test]
fn e2e_create_with_an_ordinary_dep_still_works() {
    let _log = common::test_log("e2e_create_with_an_ordinary_dep_still_works");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let other = run_br(
        &workspace,
        ["create", "Other", "--type", "task"],
        "create_other",
    );
    assert!(other.status.success());
    let other_id = parse_created_id(&other.stdout);

    let out = run_br(
        &workspace,
        [
            "create",
            "Blocked thing",
            "--deps",
            &format!("blocks:{other_id}"),
        ],
        "create_with_ordinary_dep",
    );
    assert!(
        out.status.success(),
        "an ordinary --deps entry must still work: {}",
        out.stderr
    );
}

#[test]
fn e2e_dep_import_renumbers_parent_child_rows() {
    let _log = common::test_log("e2e_dep_import_renumbers_parent_child_rows");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic = run_br(
        &workspace,
        ["create", "Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(epic.status.success());
    let epic_id = parse_created_id(&epic.stdout);

    let loose = run_br(
        &workspace,
        ["create", "Loose", "--type", "task"],
        "create_loose",
    );
    assert!(loose.status.success());
    let loose_id = parse_created_id(&loose.stdout);

    let import_path = workspace.root.join("parent_child.jsonl");
    fs::write(
        &import_path,
        format!(
            "{{\"issue_id\":\"{loose_id}\",\"depends_on_id\":\"{epic_id}\",\"type\":\"parent-child\"}}\n"
        ),
    )
    .expect("write parent-child dependency import jsonl");

    let import_arg = import_path.to_string_lossy().to_string();
    let import = run_br(
        &workspace,
        ["dep", "import", import_arg.as_str(), "--json"],
        "dep_import_parent_child",
    );
    assert!(
        import.status.success(),
        "dep import failed: {}",
        import.stderr
    );

    let import_result: Value =
        serde_json::from_str(&extract_json_payload(&import.stdout)).expect("parse dep import json");
    assert_eq!(
        import_result["imported"].as_u64(),
        Some(1),
        "the parent-child row must count as imported; got: {import_result}"
    );

    // The pre-import (now former) ID must still resolve, via the
    // `former_ids` fallback, to the issue renumbered under its new parent.
    let show = run_br(
        &workspace,
        ["show", &loose_id, "--json"],
        "show_old_id_after_import",
    );
    assert!(
        show.status.success(),
        "the old ID must still resolve after the import renumbered it: {}",
        show.stderr
    );
    let show_payload: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show.stdout)).expect("parse show json");
    let new_id = show_payload[0]["id"].as_str().expect("id in show payload");
    assert!(
        new_id.starts_with(&format!("{epic_id}.")),
        "dep import must renumber the child under its new parent; got {new_id}"
    );
}

#[test]
fn e2e_dep_import_resolves_a_parent_renamed_earlier_in_the_same_file() {
    let _log =
        common::test_log("e2e_dep_import_resolves_a_parent_renamed_earlier_in_the_same_file");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic = run_br(
        &workspace,
        ["create", "Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(epic.status.success());
    let epic_id = parse_created_id(&epic.stdout);

    let a = run_br(&workspace, ["create", "A", "--type", "task"], "create_a");
    assert!(a.status.success());
    let a_id = parse_created_id(&a.stdout);

    let b = run_br(&workspace, ["create", "B", "--type", "task"], "create_b");
    assert!(b.status.success());
    let b_id = parse_created_id(&b.stdout);

    // Row 1 renumbers A under the epic, leaving a rename tombstone at the
    // old `a_id`. Row 2 names that now-stale `a_id` as *its own* parent --
    // not just the child side of the batch getting renamed, the target side
    // does too. `depends_on_id` must be re-resolved through the same
    // `former_ids` fallback as `issue_id`, or row 2 hits
    // `ensure_dependency_target_exists_in_tx` on a tombstone and aborts the
    // batch after row 1 already committed.
    let import_path = workspace.root.join("chained_parent_child.jsonl");
    fs::write(
        &import_path,
        format!(
            "{{\"issue_id\":\"{a_id}\",\"depends_on_id\":\"{epic_id}\",\"type\":\"parent-child\"}}\n\
             {{\"issue_id\":\"{b_id}\",\"depends_on_id\":\"{a_id}\",\"type\":\"parent-child\"}}\n"
        ),
    )
    .expect("write chained parent-child dependency import jsonl");

    let import_arg = import_path.to_string_lossy().to_string();
    let import = run_br(
        &workspace,
        ["dep", "import", import_arg.as_str(), "--json"],
        "dep_import_chained_parent_child",
    );
    assert!(
        import.status.success(),
        "dep import must not abort mid-batch when a later row's parent was \
         renamed by an earlier row: {}",
        import.stderr
    );

    let import_result: Value =
        serde_json::from_str(&extract_json_payload(&import.stdout)).expect("parse dep import json");
    assert_eq!(
        import_result["imported"].as_u64(),
        Some(2),
        "both rows must count as imported; got: {import_result}"
    );

    let show_a = run_br(&workspace, ["show", &a_id, "--json"], "show_a_after_import");
    assert!(show_a.status.success(), "show a failed: {}", show_a.stderr);
    let a_payload: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_a.stdout)).expect("parse show a json");
    let new_a_id = a_payload[0]["id"].as_str().expect("id in show a payload");
    assert!(
        new_a_id.starts_with(&format!("{epic_id}.")),
        "row 1 must renumber A under the epic; got {new_a_id}"
    );

    let show_b = run_br(&workspace, ["show", &b_id, "--json"], "show_b_after_import");
    assert!(show_b.status.success(), "show b failed: {}", show_b.stderr);
    let b_payload: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_b.stdout)).expect("parse show b json");
    let new_b_id = b_payload[0]["id"].as_str().expect("id in show b payload");
    assert!(
        new_b_id.starts_with(&format!("{new_a_id}.")),
        "row 2 must renumber B under A's *post-rename* id, resolved via \
         former_ids; got {new_b_id} (A is now {new_a_id})"
    );
}

/// bds-a23.10: a descendant carried along by an ancestor's rename gets the
/// same `former_ids` provenance as the node renamed directly, so a reference
/// to its pre-move id still resolves after the move.
#[test]
fn e2e_a_grandchild_still_resolves_by_its_old_id_after_the_subtree_moves() {
    let _log =
        common::test_log("e2e_a_grandchild_still_resolves_by_its_old_id_after_the_subtree_moves");
    let workspace = BrWorkspace::new();

    assert!(run_br(&workspace, ["init"], "init").status.success());

    let epic1 = run_br(
        &workspace,
        ["create", "Epic one", "--type", "epic"],
        "create_epic1",
    );
    assert!(epic1.status.success());
    let epic1_id = parse_created_id(&epic1.stdout);

    let epic2 = run_br(
        &workspace,
        ["create", "Epic two", "--type", "epic"],
        "create_epic2",
    );
    assert!(epic2.status.success());
    let epic2_id = parse_created_id(&epic2.stdout);

    let create_child = run_br(
        &workspace,
        ["create", "Child", "--type", "task", "--parent", &epic1_id],
        "create_child",
    );
    assert!(create_child.status.success());
    let child_id = parse_created_id(&create_child.stdout);

    let create_grandchild = run_br(
        &workspace,
        [
            "create",
            "Grandchild",
            "--type",
            "task",
            "--parent",
            &child_id,
        ],
        "create_grandchild",
    );
    assert!(create_grandchild.status.success());
    // Captured before the move: a reference written before the move -- in a
    // commit message, a PR body, another issue's prose -- must still
    // resolve, even though the grandchild only moved as a side effect of its
    // parent's rename.
    let grandchild_id = parse_created_id(&create_grandchild.stdout);

    let reparent = run_br(
        &workspace,
        ["update", &child_id, "--parent", &epic2_id, "--json"],
        "reparent",
    );
    assert!(
        reparent.status.success(),
        "reparent failed: {}",
        reparent.stderr
    );

    let shown = run_br(
        &workspace,
        ["show", &grandchild_id],
        "show_grandchild_by_old_id",
    );
    assert!(
        shown.status.success(),
        "the grandchild's pre-move id must still resolve: {}",
        shown.stderr
    );
    assert!(shown.stdout.contains("Grandchild"), "got: {}", shown.stdout);
}
