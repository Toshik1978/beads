//! Parsing and validation of `.beads/remote.yaml`.

use crate::remote::error::RemoteError;
use indexmap::IndexMap;
use serde::Deserialize;
use std::path::Path;

/// Every built-in beads issue type. All must appear in `type_map`.
pub const BUILTIN_TYPES: [&str; 7] = [
    "task", "bug", "feature", "epic", "chore", "docs", "question",
];

/// Every built-in status the mirror carries. `tombstone` is deliberately
/// absent: the tombstone rule owns it, and mapping it to a state would make
/// a forwarding pointer look like an ordinary status change.
pub const MIRRORED_STATUSES: [&str; 7] = [
    "open",
    "in_progress",
    "blocked",
    "deferred",
    "draft",
    "pinned",
    "closed",
];

/// Every built-in beads priority, as the string keys `priority_map` carries
/// (serde deserializes the YAML's bare integers into `String` keys). A fixed
/// five-value range, so — like `BUILTIN_TYPES` and `MIRRORED_STATUSES` —
/// it is a closed set that can be checked for total coverage the same way.
pub const BUILTIN_PRIORITIES: [&str; 5] = ["0", "1", "2", "3", "4"];

fn default_page_size() -> u32 {
    100
}

/// Upper bound on `page_size`.
///
/// YouTrack's REST API does not publish a hard server-side ceiling on
/// `$top`, but an unbounded value turns one paged fetch into one request
/// that tries to return the entire project at once — on a large project
/// that risks timing out the request rather than failing fast. 1000 is ten
/// times this config's own default (100), which leaves comfortable headroom
/// for a deliberately large page while still refusing a value that is
/// effectively "no paging at all".
const MAX_PAGE_SIZE: u32 = 1000;

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteConfig {
    pub backend: String,
    pub url: String,
    pub project: String,
    pub status_map: IndexMap<String, String>,
    pub type_map: IndexMap<String, String>,
    pub priority_map: IndexMap<String, String>,
    pub deleted_state: String,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}

impl RemoteConfig {
    /// Read and validate `<beads_dir>/remote.yaml`.
    ///
    /// # Errors
    /// Returns `RemoteError::Config` if the file is missing, unparseable, or
    /// fails validation.
    pub fn load(beads_dir: &Path) -> Result<Self, RemoteError> {
        let path = beads_dir.join("remote.yaml");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| RemoteError::Config(format!("cannot read {}: {e}", path.display())))?;
        Self::from_yaml_str(&text)
    }

    /// Parse and validate from YAML text.
    ///
    /// # Errors
    /// Returns `RemoteError::Config` on a parse failure or any validation
    /// failure described in `validate`.
    pub fn from_yaml_str(text: &str) -> Result<Self, RemoteError> {
        let cfg: Self = serde_yml::from_str(text)
            .map_err(|e| RemoteError::Config(format!("remote.yaml is not valid YAML: {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), RemoteError> {
        if self.backend != "youtrack" {
            return Err(RemoteError::Config(format!(
                "backend '{}' is not supported; the only backend is 'youtrack'",
                self.backend
            )));
        }
        if self.status_map.contains_key("tombstone") {
            return Err(RemoteError::Config(
                "status_map must not name 'tombstone': the tombstone rule owns it, \
                 re-pointing forwarding pointers and setting deleted_state for a real \
                 delete. Remove the entry."
                    .to_string(),
            ));
        }
        check_total("type_map", &self.type_map, &BUILTIN_TYPES)?;
        check_total("status_map", &self.status_map, &MIRRORED_STATUSES)?;
        check_total("priority_map", &self.priority_map, &BUILTIN_PRIORITIES)?;
        check_injective("type_map", &self.type_map)?;
        check_injective("status_map", &self.status_map)?;
        check_injective("priority_map", &self.priority_map)?;
        if self.page_size == 0 || self.page_size > MAX_PAGE_SIZE {
            return Err(RemoteError::Config(format!(
                "page_size must be between 1 and {MAX_PAGE_SIZE}, got {}. A page_size of \
                 0 would ask YouTrack for a page of nothing on every request and never \
                 make progress; a value above {MAX_PAGE_SIZE} risks a single paged fetch \
                 timing out instead of failing fast.",
                self.page_size
            )));
        }
        Ok(())
    }

    /// The `idReadable` prefix this configuration claims, e.g. `"EM-"`.
    #[must_use]
    pub fn issue_prefix(&self) -> String {
        format!("{}-", self.project)
    }

    #[must_use]
    pub fn reverse_type(&self, remote: &str) -> Option<&str> {
        reverse(&self.type_map, remote)
    }

    #[must_use]
    pub fn reverse_status(&self, remote: &str) -> Option<&str> {
        reverse(&self.status_map, remote)
    }

    #[must_use]
    pub fn reverse_priority(&self, remote: &str) -> Option<u8> {
        reverse(&self.priority_map, remote).and_then(|k| k.parse().ok())
    }
}

/// First key in declaration order wins. With a validated config this is never
/// ambiguous, but the config is user-editable and the rule must still be
/// deterministic — which is why the maps are `IndexMap`.
fn reverse<'a>(map: &'a IndexMap<String, String>, remote: &str) -> Option<&'a str> {
    map.iter()
        .find(|(_, value)| value.as_str() == remote)
        .map(|(key, _)| key.as_str())
}

fn check_total(
    name: &str,
    map: &IndexMap<String, String>,
    required: &[&str],
) -> Result<(), RemoteError> {
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|key| !map.contains_key(*key))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(RemoteError::Config(format!(
        "{name} does not name every built-in value: missing {}. \
         Every built-in must be mapped, because a value absent from the map \
         cannot be pushed and cannot be recognised on the way back.",
        missing.join(", ")
    )))
}

fn check_injective(name: &str, map: &IndexMap<String, String>) -> Result<(), RemoteError> {
    let mut seen: IndexMap<&str, &str> = IndexMap::new();
    for (key, value) in map {
        if let Some(first) = seen.insert(value.as_str(), key.as_str()) {
            return Err(RemoteError::Config(format!(
                "{name} maps both '{first}' and '{key}' onto '{value}'. \
                 A collapse is indistinguishable from full coverage when init \
                 computes its set difference, so it is refused here: give each \
                 key its own value."
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
backend: youtrack
url: https://example.youtrack.cloud
project: EM
status_map:
  open: Open
  in_progress: In Progress
  blocked: Blocked
  deferred: Deferred
  draft: Draft
  pinned: Pinned
  closed: Fixed
type_map:
  task: Task
  bug: Bug
  feature: Feature
  epic: Epic
  chore: Chore
  docs: Docs
  question: Question
priority_map: { 0: Show-stopper, 1: Critical, 2: Major, 3: Normal, 4: Minor }
deleted_state: "Won't fix"
page_size: 100
"#;

    #[test]
    fn a_complete_config_parses_and_reverses() {
        let cfg = RemoteConfig::from_yaml_str(GOOD).expect("parse");
        assert_eq!(cfg.project, "EM");
        assert_eq!(cfg.issue_prefix(), "EM-");
        assert_eq!(cfg.reverse_type("Question"), Some("question"));
        assert_eq!(cfg.reverse_status("In Progress"), Some("in_progress"));
        assert_eq!(cfg.reverse_priority("Major"), Some(2));
        assert_eq!(cfg.reverse_type("User Story"), None);
    }

    #[test]
    fn an_omitted_builtin_type_is_rejected() {
        let yaml = GOOD.replace("  docs: Docs\n", "");
        let err = RemoteConfig::from_yaml_str(&yaml).expect_err("must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("docs"),
            "message must name the missing key: {msg}"
        );
        assert!(msg.contains("type_map"), "message must name the map: {msg}");
    }

    #[test]
    fn an_omitted_mirrored_status_is_rejected() {
        let yaml = GOOD.replace("  blocked: Blocked\n", "");
        let err = RemoteConfig::from_yaml_str(&yaml).expect_err("must reject");
        assert!(err.to_string().contains("blocked"), "{err}");
    }

    #[test]
    fn an_omitted_builtin_priority_is_rejected() {
        let yaml = GOOD.replace(
            "priority_map: { 0: Show-stopper, 1: Critical, 2: Major, 3: Normal, 4: Minor }",
            "priority_map: { 0: Show-stopper, 1: Critical, 2: Major, 3: Normal }",
        );
        let err = RemoteConfig::from_yaml_str(&yaml).expect_err("must reject");
        let msg = err.to_string();
        assert!(
            msg.contains('4'),
            "message must name the missing key: {msg}"
        );
        assert!(
            msg.contains("priority_map"),
            "message must name the map: {msg}"
        );
    }

    #[test]
    fn a_collapsed_map_is_rejected_naming_both_keys() {
        let yaml = GOOD.replace("  question: Question\n", "  question: Task\n");
        let err = RemoteConfig::from_yaml_str(&yaml).expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.contains("task"), "must name the first key: {msg}");
        assert!(msg.contains("question"), "must name the second key: {msg}");
        assert!(msg.contains("Task"), "must name the shared value: {msg}");
    }

    #[test]
    fn tombstone_in_status_map_is_rejected() {
        let yaml = GOOD.replace(
            "  closed: Fixed\n",
            "  closed: Fixed\n  tombstone: Obsolete\n",
        );
        let err = RemoteConfig::from_yaml_str(&yaml).expect_err("must reject");
        assert!(err.to_string().contains("tombstone"), "{err}");
    }

    #[test]
    fn a_page_size_of_zero_is_rejected() {
        let yaml = GOOD.replace("page_size: 100\n", "page_size: 0\n");
        let err = RemoteConfig::from_yaml_str(&yaml).expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.contains("page_size"), "must name the field: {msg}");
        assert!(msg.contains('0'), "must name the offending value: {msg}");
    }

    #[test]
    fn a_page_size_above_the_ceiling_is_rejected() {
        let yaml = GOOD.replace("page_size: 100\n", "page_size: 1001\n");
        let err = RemoteConfig::from_yaml_str(&yaml).expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.contains("page_size"), "must name the field: {msg}");
        assert!(msg.contains("1001"), "must name the offending value: {msg}");
    }

    #[test]
    fn the_page_size_ceiling_itself_is_accepted() {
        let yaml = GOOD.replace("page_size: 100\n", "page_size: 1000\n");
        assert!(
            RemoteConfig::from_yaml_str(&yaml).is_ok(),
            "1000 is the documented ceiling"
        );
    }

    #[test]
    fn declaration_order_survives_parse() {
        let cfg = RemoteConfig::from_yaml_str(GOOD).expect("parse");
        let keys: Vec<&str> = cfg.type_map.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "task", "bug", "feature", "epic", "chore", "docs", "question"
            ]
        );
    }
}
