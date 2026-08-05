//! Reparenting a child left the original epic permanently unclosable.
//!
//! Observed transcript:
//!
//! ```text
//! $ br update ab-l04.1 --parent ab-fb6   -> child now depends on ab-fb6
//! $ br close ab-l04                      -> epic has 1 open dot-notation child: ab-l04.1
//! ```
//!
//! `ab-l04` had no children by any dependency measure, and `--force` was the
//! only way past it. The cause was two independent records of parentage that
//! were free to disagree: the `parent-child` dep row, which the update moved,
//! and the issue ID, which it did not. `get_open_dot_notation_children` reads
//! only the second.
//!
//! The fix makes the ID authoritative -- reparenting renumbers -- so the two
//! can no longer disagree. See bds-a23's design for the full invariant.

use crate::common;

use common::cli::{BrWorkspace, parse_created_id, run_br};

#[test]
fn reparented_child_no_longer_blocks_the_epic_it_left() {
    let _log = common::test_log("reparented_child_no_longer_blocks_the_epic_it_left");
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

    let closed = run_br(
        &workspace,
        ["close", &epic1_id, "--reason", "done"],
        "close_epic1",
    );
    assert!(
        closed.status.success(),
        "the epic it left has no children and must close without --force; got: {}",
        closed.stderr
    );
}

#[test]
fn detaching_the_only_child_no_longer_blocks_the_epic() {
    let _log = common::test_log("detaching_the_only_child_no_longer_blocks_the_epic");
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

    // The other half of the original report: clearing the parent removed the
    // dep row and the epic still refused, because the ID had not moved.
    let detach = run_br(&workspace, ["detach", &child_id], "detach");
    assert!(detach.status.success(), "detach failed: {}", detach.stderr);

    let closed = run_br(
        &workspace,
        ["close", &epic_id, "--reason", "done"],
        "close_epic",
    );
    assert!(closed.status.success(), "got: {}", closed.stderr);
}

#[test]
fn the_close_guard_still_catches_a_genuinely_open_child() {
    let _log = common::test_log("the_close_guard_still_catches_a_genuinely_open_child");
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

    let closed = run_br(
        &workspace,
        ["close", &epic_id, "--reason", "done"],
        "close_epic",
    );
    assert!(
        !closed.status.success(),
        "the guard must still do its original job -- this fix narrows what counts \
         as a child, it does not remove the check"
    );
}
