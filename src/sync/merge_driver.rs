//! Three-way merge of `issues.jsonl` files, for `br merge-driver`.
//!
//! Git's line merge conflicts on neighbouring lines, and an issue per line in
//! id order puts unrelated issues next to each other. This merges by id
//! instead, with the rules the rest of sync already holds:
//!
//! - Each record is parsed as import parses it, so a difference in
//!   representation alone (a tombstone's missing `closed_at`) is not a change.
//! - Per issue, [`merge_issue`] with [`ConflictResolution::PreferNewer`]: a
//!   one-sided change wins, and when both sides changed an issue its fields
//!   come from the later `updated_at`.
//! - When both sides changed an issue, its labels, dependencies and comments
//!   are merged as sets, so an addition on either side survives and a removal
//!   stays removed. Taking the newer side wholesale would drop, say, one of
//!   two comments made on the same epic from two branches.
//! - A tombstone beats a live copy on the other side whatever the timestamps,
//!   as import never lets a JSONL record resurrect one. Otherwise an edit to a
//!   renamed-away id would revive it next to the id it was renamed to.
//!
//! The result is in id order, as a full export writes it. A record taken
//! unchanged from one side keeps that side's bytes, so the merge rewrites only
//! the lines it had to.

use super::{
    ConflictResolution, MergeResult, ensure_no_conflict_markers, merge_issue, normalize_issue,
    normalize_issue_for_export, parse_normalized_import_issue,
};
use crate::error::{BeadsError, Result};
use crate::model::{Comment, Dependency, Issue, Status};
use std::collections::{BTreeMap, HashSet};
use std::hash::Hash;
use std::io::Write;
use std::path::Path;

/// One side of the merge: each record parsed, next to the line it came from.
struct Side {
    records: BTreeMap<String, (Issue, String)>,
}

impl Side {
    fn read(path: &Path) -> Result<Self> {
        ensure_no_conflict_markers(path)?;
        let content = std::fs::read_to_string(path)?;
        let mut records = BTreeMap::new();
        for (index, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let issue = parse_normalized_import_issue(trimmed, index + 1)
                .map_err(|err| BeadsError::Config(format!("{}: {err}", path.display())))?;
            let id = issue.id.clone();
            if records
                .insert(id.clone(), (issue, trimmed.to_string()))
                .is_some()
            {
                return Err(BeadsError::Config(format!(
                    "{}: duplicate issue id '{id}' at line {}",
                    path.display(),
                    index + 1
                )));
            }
        }
        Ok(Self { records })
    }

    fn issue(&self, id: &str) -> Option<&Issue> {
        self.records.get(id).map(|(issue, _)| issue)
    }

    fn line_if_same(&self, merged: &Issue) -> Option<&str> {
        self.records
            .get(&merged.id)
            .filter(|(issue, _)| issue == merged)
            .map(|(_, line)| line.as_str())
    }
}

/// What a merge produced: the file body, and anything worth telling the user.
pub struct MergedFile {
    pub content: String,
    pub notes: Vec<String>,
}

/// Merge `ours` and `theirs` against their common ancestor `base`.
///
/// # Errors
///
/// Returns an error if a side cannot be read, holds conflict markers, a record
/// that does not parse or validate, or two records with one id. Nothing is
/// merged from input that cannot be trusted.
pub fn merge_jsonl_files(base: &Path, ours: &Path, theirs: &Path) -> Result<MergedFile> {
    let base = Side::read(base)?;
    let ours = Side::read(ours)?;
    let theirs = Side::read(theirs)?;

    let ids: std::collections::BTreeSet<&String> = base
        .records
        .keys()
        .chain(ours.records.keys())
        .chain(theirs.records.keys())
        .collect();

    let mut content = String::new();
    let mut notes = Vec::new();
    for id in ids {
        let Some(merged) = merge_one(base.issue(id), ours.issue(id), theirs.issue(id), &mut notes)
        else {
            continue;
        };

        if let Some(line) = ours
            .line_if_same(&merged)
            .or_else(|| theirs.line_if_same(&merged))
        {
            content.push_str(line);
        } else {
            let mut merged = merged;
            normalize_issue_for_export(&mut merged);
            content.push_str(&super::jsonl_format::to_line(&merged)?);
        }
        content.push('\n');
    }

    Ok(MergedFile { content, notes })
}

fn merge_one(
    base: Option<&Issue>,
    ours: Option<&Issue>,
    theirs: Option<&Issue>,
    notes: &mut Vec<String>,
) -> Option<Issue> {
    if let (Some(o), Some(t)) = (ours, theirs) {
        let (stone, live, live_side) =
            match (o.status == Status::Tombstone, t.status == Status::Tombstone) {
                (true, false) => (o, t, "theirs"),
                (false, true) => (t, o, "ours"),
                _ => return Some(merge_both_present(base, o, t)),
            };
        if base.is_none_or(|b| !live.sync_equals(b)) {
            notes.push(format!(
                "{}: kept the tombstone; dropped the edit made on {live_side}",
                stone.id
            ));
        }
        return Some(stone.clone());
    }

    match merge_issue(base, ours, theirs, ConflictResolution::PreferNewer) {
        MergeResult::Keep(issue) | MergeResult::KeepWithNote(issue, _) => Some(issue),
        MergeResult::Delete | MergeResult::NoAction | MergeResult::Conflict(_) => None,
    }
}

fn merge_both_present(base: Option<&Issue>, ours: &Issue, theirs: &Issue) -> Issue {
    if ours.sync_equals(theirs) {
        return ours.clone();
    }
    if let Some(base) = base {
        if ours.sync_equals(base) {
            return theirs.clone();
        }
        if theirs.sync_equals(base) {
            return ours.clone();
        }
    }

    // Both sides changed it. Ties go to ours, as in `merge_issue`.
    let ours_is_newer = ours.updated_at >= theirs.updated_at;
    let newer = if ours_is_newer { ours } else { theirs };
    let mut merged = match base {
        Some(base) => {
            merge_fields(base, ours, theirs, ours_is_newer).unwrap_or_else(|| newer.clone())
        }
        // Created on both sides with no common ancestor: nothing says which
        // fields each side changed, so the newer record stands.
        None => newer.clone(),
    };

    let empty = Issue {
        labels: Vec::new(),
        dependencies: Vec::new(),
        comments: Vec::new(),
        ..ours.clone()
    };
    let base = base.unwrap_or(&empty);
    merged.labels = merge_sets(&base.labels, &ours.labels, &theirs.labels, Clone::clone);
    merged.dependencies = merge_sets(
        &base.dependencies,
        &ours.dependencies,
        &theirs.dependencies,
        dependency_key,
    );
    merged.comments = merge_sets(
        &base.comments,
        &ours.comments,
        &theirs.comments,
        comment_key,
    );
    merged.updated_at = ours.updated_at.max(theirs.updated_at);
    normalize_issue(&mut merged);

    merged
}

/// Merge field by field: a field only one side changed takes that side's
/// value, and one both sides changed takes the newer side's. Taking the newer record
/// whole would drop the other side's edit to a field the newer never touched.
/// The relation lists are left to [`merge_sets`].
fn merge_fields(base: &Issue, ours: &Issue, theirs: &Issue, ours_is_newer: bool) -> Option<Issue> {
    let as_object = |issue: &Issue| match serde_json::to_value(issue) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    };
    let (base, ours, theirs) = (as_object(base)?, as_object(ours)?, as_object(theirs)?);

    let mut merged = serde_json::Map::new();
    let keys: std::collections::BTreeSet<&String> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    for key in keys {
        let (b, o, t) = (base.get(key), ours.get(key), theirs.get(key));
        let value = if o == t || t == b {
            o
        } else if o == b {
            t
        } else if ours_is_newer {
            o
        } else {
            t
        };
        if let Some(value) = value {
            merged.insert(key.clone(), value.clone());
        }
    }

    serde_json::from_value(serde_json::Value::Object(merged)).ok()
}

/// Three-way set merge: an item stays if both sides have it, or if either side
/// added it since `base`. A removal on one side therefore sticks, and an
/// addition on either side survives. Order follows `ours`, then what only
/// `theirs` added.
fn merge_sets<T: Clone, K: Eq + Hash>(
    base: &[T],
    ours: &[T],
    theirs: &[T],
    key: impl Fn(&T) -> K,
) -> Vec<T> {
    let base_keys: HashSet<K> = base.iter().map(&key).collect();
    let ours_keys: HashSet<K> = ours.iter().map(&key).collect();
    let theirs_keys: HashSet<K> = theirs.iter().map(&key).collect();

    let keep = |k: &K| (ours_keys.contains(k) && theirs_keys.contains(k)) || !base_keys.contains(k);

    let mut merged: Vec<T> = ours
        .iter()
        .filter(|item| keep(&key(item)))
        .cloned()
        .collect();
    merged.extend(
        theirs
            .iter()
            .filter(|item| {
                let k = key(item);
                !ours_keys.contains(&k) && keep(&k)
            })
            .cloned(),
    );
    merged
}

fn dependency_key(dep: &Dependency) -> (String, String, String) {
    (
        dep.issue_id.clone(),
        dep.depends_on_id.clone(),
        dep.dep_type.as_str().to_string(),
    )
}

/// Comments are matched by what they say, not by `id`: a file written before
/// v1.9.1 carries each clone's database counter as the id, so two branches'
/// new comments can share one.
fn comment_key(comment: &Comment) -> (String, chrono::DateTime<chrono::Utc>, String) {
    (
        comment.author.clone(),
        comment.created_at,
        comment.body.clone(),
    )
}

/// Replace `path` with `content`, via a temporary file in the same directory.
///
/// # Errors
///
/// Returns an error if the temporary file cannot be written or renamed.
pub fn write_merged_file(path: &Path, content: &str) -> Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(content.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|err| BeadsError::Io(err.error))?;
    Ok(())
}
