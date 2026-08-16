//! Link types resolved by name, and the create/delete identifier asymmetry.
//!
//! Link type ids are instance-local and must never be hardcoded, so they are
//! resolved at runtime from `/api/issueLinkTypes` by exact `name`. Creation
//! accepts `idReadable` in the request body, but deletion 404s on
//! `idReadable` and requires the internal database id in the path — verified
//! live. See `link_remove`'s doc comment for the consequence.

use crate::remote::error::RemoteError;
use crate::remote::http::HttpClient;
use serde_json::{Value, json};

/// The three link types beads emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Subtask,
    Depend,
    Relates,
}

/// Which end of a directed link a beads relation maps onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    SourceToTarget,
    TargetToSource,
    /// An undirected type (`Relates`) takes the bare type id.
    Undirected,
}

/// The reference instance's link type ids, resolved once per run.
///
/// Bare type ids — never suffixed. See [`LinkTypes::link_id`] for the
/// `{linkID}` path segment a request actually uses.
#[derive(Debug, Clone)]
pub struct LinkTypes {
    pub subtask: String,
    pub depend: String,
    pub relates: String,
}

/// One linked issue, carrying both identifiers a caller might need: the
/// `idReadable` a create addresses, and the internal database id a removal
/// requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedIssue {
    pub id: String,
    pub id_readable: String,
}

/// One link bucket from a fetched issue's `links` array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLink {
    pub link_id: String,
    pub kind: LinkKind,
    pub direction: String,
    pub issues: Vec<LinkedIssue>,
}

impl LinkTypes {
    /// Resolve `Subtask`, `Depend` and `Relates` by exact name from
    /// `/api/issueLinkTypes`.
    ///
    /// # Errors
    /// Returns `RemoteError::Config` naming any of the three types that is
    /// absent from the instance, and whatever `http` returns on transport or
    /// HTTP failure.
    ///
    /// # Panics
    /// Never in practice: the `expect`s below are only reachable once the
    /// `missing` check above has already returned early for any of the
    /// three names whose lookup came back `None`.
    pub fn resolve(http: &HttpClient) -> Result<Self, RemoteError> {
        let raw = http.get_json(
            "/api/issueLinkTypes?fields=id,name,sourceToTarget,targetToSource,directed&$top=100",
            "issue link types",
        )?;
        let types = raw.as_array().cloned().unwrap_or_default();
        let find = |name: &str| -> Option<String> {
            types
                .iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
                .and_then(|t| t.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        };

        let subtask = find("Subtask");
        let depend = find("Depend");
        let relates = find("Relates");

        let missing: Vec<&str> = [
            ("Subtask", subtask.is_none()),
            ("Depend", depend.is_none()),
            ("Relates", relates.is_none()),
        ]
        .into_iter()
        .filter_map(|(name, is_missing)| is_missing.then_some(name))
        .collect();

        if !missing.is_empty() {
            return Err(RemoteError::Config(format!(
                "this YouTrack instance has no issue link type named {}; \
                 br remote expects Subtask, Depend and Relates to exist \
                 (create them or rename an existing type to match)",
                missing.join(", ")
            )));
        }

        Ok(Self {
            subtask: subtask.expect("checked above: `missing` would be non-empty otherwise"),
            depend: depend.expect("checked above: `missing` would be non-empty otherwise"),
            relates: relates.expect("checked above: `missing` would be non-empty otherwise"),
        })
    }

    /// The `{linkID}` path segment: the bare type id for an undirected type,
    /// suffixed `s` (source→target) or `t` (target→source) for a directed
    /// one.
    #[must_use]
    pub fn link_id(&self, kind: LinkKind, direction: Direction) -> String {
        let base = match kind {
            LinkKind::Subtask => &self.subtask,
            LinkKind::Depend => &self.depend,
            LinkKind::Relates => &self.relates,
        };
        match direction {
            Direction::SourceToTarget => format!("{base}s"),
            Direction::TargetToSource => format!("{base}t"),
            Direction::Undirected => base.clone(),
        }
    }
}

/// Add a link from `from_readable` to `to_readable` over `link_id`.
///
/// Addressed by `idReadable` in the request body — the shape that works for
/// creation. See [`link_remove`] for why removal cannot use the same
/// identifier.
///
/// # Errors
/// Returns whatever `http` returns on transport or HTTP failure.
pub fn link_add(
    http: &HttpClient,
    from_readable: &str,
    link_id: &str,
    to_readable: &str,
) -> Result<(), RemoteError> {
    let path = format!("/api/issues/{from_readable}/links/{link_id}/issues?fields=idReadable");
    http.post_json(&path, &json!({"idReadable": to_readable}), "issue link")?;
    Ok(())
}

/// Remove the link from `from_readable` over `link_id` to `to_internal_id`.
///
/// `to_internal_id` **must** be the linked issue's internal database id
/// (e.g. `"3-24"`), not its `idReadable` (e.g. `"EM-5"`). Passing an
/// `idReadable` here 404s — verified live — even though the same identifier
/// works for [`link_add`]. Without the internal id in hand a removal cannot
/// be issued at all, and the failure presents as a plain 404 that looks like
/// the link was already gone.
///
/// # Errors
/// Returns whatever `http` returns on transport or HTTP failure.
pub fn link_remove(
    http: &HttpClient,
    from_readable: &str,
    link_id: &str,
    to_internal_id: &str,
) -> Result<(), RemoteError> {
    let path = format!("/api/issues/{from_readable}/links/{link_id}/issues/{to_internal_id}");
    http.delete(&path, "issue link")
}

/// Read every non-empty link bucket out of a fetched issue's `links` array.
#[must_use]
pub fn parse_links(raw: &Value, types: &LinkTypes) -> Vec<RemoteLink> {
    let Some(links) = raw.get("links").and_then(Value::as_array) else {
        return Vec::new();
    };

    links
        .iter()
        .filter_map(|link| {
            let issues: Vec<LinkedIssue> = link
                .get("issues")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|issue| {
                    let id = issue.get("id").and_then(Value::as_str)?.to_string();
                    let id_readable = issue.get("idReadable").and_then(Value::as_str)?.to_string();
                    Some(LinkedIssue { id, id_readable })
                })
                .collect();
            if issues.is_empty() {
                return None;
            }

            let link_type_id = link.get("linkType").and_then(|t| t.get("id"))?.as_str()?;
            let kind = if link_type_id == types.subtask {
                LinkKind::Subtask
            } else if link_type_id == types.depend {
                LinkKind::Depend
            } else if link_type_id == types.relates {
                LinkKind::Relates
            } else {
                return None;
            };
            let direction = link
                .get("direction")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let link_id = link.get("id").and_then(Value::as_str)?.to_string();

            Some(RemoteLink {
                link_id,
                kind,
                direction,
                issues,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::http::{HttpClient, RetryPolicy, Token};
    use test_support::mock_http::MockServer;
    use test_support::youtrack_fixtures::LINK_DELETE_NOT_FOUND;

    const TYPES_PATH: &str =
        "/api/issueLinkTypes?fields=id,name,sourceToTarget,targetToSource,directed&$top=100";

    /// The reference instance's link types. Ids live here, never in `src/`.
    const REFERENCE_TYPES: &str = r#"[
        {"id":"173-0","name":"Relates","sourceToTarget":"relates to","directed":false},
        {"id":"173-1","name":"Depend","sourceToTarget":"is required for","targetToSource":"depends on","directed":true},
        {"id":"173-2","name":"Duplicate","sourceToTarget":"is duplicated by","targetToSource":"duplicates","directed":true},
        {"id":"173-3","name":"Subtask","sourceToTarget":"parent for","targetToSource":"subtask of","directed":true}
    ]"#;

    fn client(server: &MockServer) -> HttpClient {
        HttpClient::new(&server.base_url(), Token::new("t"), RetryPolicy::none())
    }

    #[test]
    fn link_types_resolve_by_name_not_by_id() {
        let server = MockServer::start();
        server.on("GET", TYPES_PATH, 200, REFERENCE_TYPES);

        let types = LinkTypes::resolve(&client(&server)).expect("resolve");
        assert_eq!(types.subtask, "173-3");
        assert_eq!(
            types.depend, "173-1",
            "the type is named Depend, not 'Depends on'"
        );
        assert_eq!(types.relates, "173-0");
    }

    #[test]
    fn an_instance_with_different_ids_still_works() {
        let server = MockServer::start();
        server.on(
            "GET",
            TYPES_PATH,
            200,
            r#"[{"id":"900-7","name":"Subtask","directed":true},
                {"id":"900-8","name":"Depend","directed":true},
                {"id":"900-9","name":"Relates","directed":false}]"#,
        );
        let types = LinkTypes::resolve(&client(&server)).expect("resolve");
        assert_eq!(
            types.subtask, "900-7",
            "ids are instance-local and must not be assumed"
        );
    }

    #[test]
    fn a_missing_link_type_fails_at_resolve_time() {
        let server = MockServer::start();
        server.on(
            "GET",
            TYPES_PATH,
            200,
            r#"[{"id":"173-0","name":"Relates","directed":false}]"#,
        );
        let err = LinkTypes::resolve(&client(&server)).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("Subtask"), "must name what is missing: {msg}");
    }

    #[test]
    fn directed_link_ids_take_the_s_and_t_suffixes() {
        let types = LinkTypes {
            subtask: "173-3".into(),
            depend: "173-1".into(),
            relates: "173-0".into(),
        };
        assert_eq!(
            types.link_id(LinkKind::Subtask, Direction::SourceToTarget),
            "173-3s"
        );
        assert_eq!(
            types.link_id(LinkKind::Subtask, Direction::TargetToSource),
            "173-3t"
        );
        assert_eq!(
            types.link_id(LinkKind::Depend, Direction::SourceToTarget),
            "173-1s"
        );
        assert_eq!(
            types.link_id(LinkKind::Relates, Direction::Undirected),
            "173-0",
            "an undirected type takes the bare id"
        );
    }

    #[test]
    fn a_removal_uses_the_internal_id_never_the_readable_one() {
        let server = MockServer::start();
        server.on(
            "DELETE",
            "/api/issues/EM-4/links/173-3s/issues/3-24",
            200,
            "",
        );
        // If the implementation reaches for idReadable, this route answers and
        // the assertion below catches it.
        server.on(
            "DELETE",
            "/api/issues/EM-4/links/173-3s/issues/EM-5",
            404,
            LINK_DELETE_NOT_FOUND,
        );

        link_remove(&client(&server), "EM-4", "173-3s", "3-24").expect("remove");

        let deletes = server.write_requests();
        assert_eq!(deletes.len(), 1);
        assert!(
            deletes[0].path.ends_with("/3-24"),
            "removal must address the internal id: {}",
            deletes[0].path
        );
        assert!(
            !deletes[0].path.ends_with("/EM-5"),
            "idReadable 404s on delete even though it works on create"
        );
    }

    #[test]
    fn an_addition_uses_the_readable_id_in_the_body() {
        let server = MockServer::start();
        server.on(
            "POST",
            "/api/issues/EM-4/links/173-3s/issues?fields=idReadable",
            200,
            r#"{"idReadable":"EM-5"}"#,
        );

        link_add(&client(&server), "EM-4", "173-3s", "EM-5").expect("add");

        let posts = server.write_requests();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].body.contains("EM-5"), "{}", posts[0].body);
    }

    #[test]
    fn parse_links_carries_both_identifiers() {
        let types = LinkTypes {
            subtask: "173-3".into(),
            depend: "173-1".into(),
            relates: "173-0".into(),
        };
        let raw = serde_json::json!({"links":[
            {"id":"173-3t","direction":"INWARD","linkType":{"id":"173-3","name":"Subtask"},
             "issues":[{"id":"3-20","idReadable":"EM-1"}]},
            {"id":"173-0","direction":"BOTH","linkType":{"id":"173-0","name":"Relates"},
             "issues":[]}
        ]});
        let links = parse_links(&raw, &types);
        assert_eq!(links.len(), 1, "empty link buckets are dropped");
        assert_eq!(links[0].issues[0].id, "3-20");
        assert_eq!(links[0].issues[0].id_readable, "EM-1");
    }
}
