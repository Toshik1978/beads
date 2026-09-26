//! Comment ids were each clone's SQLite counter, written into the shared JSONL.
//!
//! Two clones commenting independently handed out the same numbers, so a
//! merged `issues.jsonl` held one id for two different comments. Import kept
//! the id for whichever it met first and renumbered the other, and a clone that
//! had created the other one locally resolved it the opposite way: the two
//! clones exported different ids for the same comments, and every rebuild or
//! forced export rewrote records whose content had not changed. Where the two
//! comments sat on the same issue — the merge driver keeps both — import
//! rejected the file outright as a duplicate id.
//!
//! A comment's id is now derived from its issue, author, time and text, so
//! every clone assigns the same id to the same comment.

use crate::common;

use beads::model::Comment;
use beads::storage::SqliteStorage;
use beads::sync::{
    ExportConfig, ImportConfig, auto_flush, export_to_jsonl, finalize_export, import_from_jsonl,
};
use beads::util::comment_content_id;
use common::{fixtures, test_db};
use std::path::Path;
use tempfile::TempDir;

fn issue(id: &str) -> beads::model::Issue {
    let mut issue = fixtures::issue(id);
    issue.id = id.to_string();
    issue
}

fn export(storage: &SqliteStorage, path: &Path) -> String {
    export_to_jsonl(
        storage,
        path,
        &ExportConfig {
            force: true,
            ..ExportConfig::default()
        },
    )
    .unwrap();
    std::fs::read_to_string(path).unwrap()
}

fn import(storage: &mut SqliteStorage, path: &Path) {
    import_from_jsonl(storage, path, &ImportConfig::default(), Some("bd-")).unwrap();
}

fn clone_with_commented_issue(id: &str, text: &str, path: &Path) -> (SqliteStorage, String) {
    let mut storage = test_db();
    storage.create_issue(&issue(id), "tester").unwrap();
    storage.add_comment(id, "tester", text).unwrap();
    let line = export(&storage, path);
    (storage, line)
}

#[test]
fn two_clones_export_the_same_comment_ids() {
    let temp = TempDir::new().unwrap();
    let (_a, a_lines) = clone_with_commented_issue("bd-a", "from a", &temp.path().join("a"));
    let (mut b, b_lines) = clone_with_commented_issue("bd-b", "from b", &temp.path().join("b"));

    let merged = temp.path().join("merged.jsonl");
    std::fs::write(&merged, format!("{a_lines}{b_lines}")).unwrap();

    import(&mut b, &merged);
    let mut fresh = test_db();
    import(&mut fresh, &merged);

    assert_eq!(
        export(&b, &temp.path().join("b-out")),
        export(&fresh, &temp.path().join("fresh-out")),
        "the clone that wrote a comment and a clone that imported it must \
         agree on its id"
    );
}

#[test]
fn a_new_comment_id_is_its_content_id() {
    let mut storage = test_db();
    storage.create_issue(&issue("bd-a"), "tester").unwrap();

    let comment = storage.add_comment("bd-a", "tester", "hello").unwrap();

    assert_eq!(
        comment.id,
        comment_content_id(
            &comment.issue_id,
            &comment.author,
            comment.created_at,
            &comment.body
        )
    );
    assert!(comment.id > 0 && comment.id < 1 << 52);
}

/// The merge driver keeps both branches' comments on an issue, and each
/// branch's counter could give its comment the same number.
#[test]
fn two_comments_sharing_an_id_on_one_issue_both_import() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    let mut record = issue("bd-a");
    let comment = |text: &str, seconds: i64| Comment {
        id: 3485,
        issue_id: "bd-a".to_string(),
        author: "tester".to_string(),
        body: text.to_string(),
        created_at: record.updated_at + chrono::Duration::seconds(seconds),
    };
    record.comments = vec![comment("from ours", 1), comment("from theirs", 2)];
    std::fs::write(
        &path,
        format!("{}\n", beads::sync::jsonl_format::to_line(&record).unwrap()),
    )
    .unwrap();

    let mut storage = test_db();
    import(&mut storage, &path);

    let texts: Vec<String> = storage
        .get_comments("bd-a")
        .unwrap()
        .into_iter()
        .map(|comment| comment.body)
        .collect();
    assert_eq!(texts.len(), 2, "{texts:?}");
}

/// A file written before this carries counter ids. Importing it marks those
/// issues for export, so the canonical ids land in one flush rather than
/// whenever some later full export happens to rewrite the record.
#[test]
fn importing_counter_ids_flushes_content_ids_once() {
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    std::fs::create_dir_all(&beads_dir).unwrap();
    let path = beads_dir.join("issues.jsonl");

    let mut record = issue("bd-a");
    record.comments = vec![Comment {
        id: 7,
        issue_id: "bd-a".to_string(),
        author: "tester".to_string(),
        body: "hello".to_string(),
        created_at: record.updated_at,
    }];
    std::fs::write(
        &path,
        format!("{}\n", beads::sync::jsonl_format::to_line(&record).unwrap()),
    )
    .unwrap();

    let mut storage = test_db();
    import(&mut storage, &path);
    assert_eq!(storage.get_dirty_issue_ids().unwrap(), vec!["bd-a"]);

    auto_flush(&mut storage, &beads_dir, &path, false).unwrap();
    let written: beads::model::Issue =
        serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
    let comment = &written.comments[0];
    assert_eq!(
        comment.id,
        comment_content_id(
            &comment.issue_id,
            &comment.author,
            comment.created_at,
            &comment.body
        )
    );

    // And once canonical, importing it again leaves nothing to flush.
    let exported = export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    finalize_export(&mut storage, &exported, Some(&exported.issue_hashes), &path).unwrap();
    import(&mut storage, &path);
    assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
}

/// The same note posted to several issues in one go shares author, time and
/// text. With the issue left out of the id those collided, and which comment
/// kept the id depended on insertion order.
#[test]
fn the_same_comment_on_two_issues_gets_its_own_content_id_on_each() {
    let mut storage = test_db();
    let at = chrono::Utc::now();
    for id in ["bd-a", "bd-b"] {
        let mut record = issue(id);
        record.comments = vec![Comment {
            id: 0,
            issue_id: id.to_string(),
            author: "tester".to_string(),
            body: "posted to both".to_string(),
            created_at: at,
        }];
        storage.create_issue(&record, "tester").unwrap();
    }

    for id in ["bd-a", "bd-b"] {
        let comment = &storage.get_comments(id).unwrap()[0];
        assert_eq!(
            comment.id,
            comment_content_id(id, &comment.author, comment.created_at, &comment.body),
            "{id}"
        );
    }
}

#[test]
fn a_rename_rekeys_the_moved_issues_comments() {
    let mut storage = test_db();
    storage.create_issue(&issue("bd-old"), "tester").unwrap();
    storage.add_comment("bd-old", "tester", "hello").unwrap();

    storage.rename_issue("bd-old", "bd-new", "tester").unwrap();

    let comment = &storage.get_comments("bd-new").unwrap()[0];
    assert_eq!(
        comment.id,
        comment_content_id("bd-new", &comment.author, comment.created_at, &comment.body)
    );
}
