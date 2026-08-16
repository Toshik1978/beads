//! Labels mapped onto YouTrack tags, resolved-or-created by database id.
//!
//! Tags must already exist to be attached and are permission-gated. A tag
//! br cannot create or attach is reported at the end of the run rather than
//! failing the whole issue write — a label is not worth losing a mirrored
//! issue over. See [`TagCache::resolve_or_create`].

use crate::remote::error::RemoteError;
use crate::remote::http::HttpClient;
use serde_json::{Value, json};
use std::collections::HashMap;

/// Name → database id, loaded once per run and grown as new tags are
/// created.
#[derive(Debug, Clone, Default)]
pub struct TagCache {
    by_name: HashMap<String, String>,
    skipped: Vec<String>,
}

impl TagCache {
    /// Load every existing tag's name and id.
    ///
    /// # Errors
    /// Returns whatever `http` returns on transport or HTTP failure.
    pub fn load(http: &HttpClient) -> Result<Self, RemoteError> {
        let by_name = fetch_tag_map(http)?;
        Ok(Self {
            by_name,
            skipped: Vec::new(),
        })
    }

    /// The cached id for `name`, creating the tag first if it does not yet
    /// exist.
    ///
    /// Returns `Ok(None)` in either of two cases, both reported via
    /// [`TagCache::skipped`] rather than failing the issue write — a label is
    /// not worth losing a mirrored issue over:
    ///
    /// - tag creation is permission-gated (a 401/403);
    /// - a concurrent creator won the race (`RemoteError::AlreadyExists`,
    ///   from YouTrack's `must-be-unique` body) and the tag still cannot be
    ///   found by re-resolving its name — e.g. it exists but is not visible
    ///   to this token.
    ///
    /// The recoverable half of that second case — the race resolves to a
    /// tag this token *can* see — re-fetches the tag list and returns the
    /// id it finds, rather than skipping a tag that in fact exists.
    ///
    /// Any other error propagates.
    ///
    /// # Errors
    /// Returns whatever `http` returns on transport or HTTP failure, other
    /// than a 401/403 or an unrecoverable `AlreadyExists` from tag creation.
    pub fn resolve_or_create(
        &mut self,
        http: &HttpClient,
        name: &str,
    ) -> Result<Option<String>, RemoteError> {
        if let Some(id) = self.by_name.get(name) {
            return Ok(Some(id.clone()));
        }

        match http.post_json("/api/tags?fields=id,name", &json!({"name": name}), "tag") {
            Ok(response) => {
                let id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.by_name.insert(name.to_string(), id.clone());
                Ok(Some(id))
            }
            Err(RemoteError::Http { status, .. }) if status == 401 || status == 403 => {
                self.skipped.push(name.to_string());
                Ok(None)
            }
            Err(RemoteError::AlreadyExists { .. }) => {
                // Someone else created this tag between our `load` and this
                // create attempt. Re-resolve by name rather than treating a
                // tag that now exists as a failure to write the issue.
                let refreshed = fetch_tag_map(http)?;
                if let Some(id) = refreshed.get(name) {
                    self.by_name.insert(name.to_string(), id.clone());
                    Ok(Some(id.clone()))
                } else {
                    // Exists per YouTrack, but not visible to this token (or
                    // the list endpoint's own view is inconsistent) — still
                    // not worth failing the run.
                    self.skipped.push(name.to_string());
                    Ok(None)
                }
            }
            Err(err) => Err(err),
        }
    }

    /// Tag names that could not be created or attached, in the order they
    /// were skipped. Reported at the end of the run.
    #[must_use]
    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }
}

/// GET every existing tag's name and id.
fn fetch_tag_map(http: &HttpClient) -> Result<HashMap<String, String>, RemoteError> {
    let raw = http.get_json("/api/tags?fields=id,name&$top=500", "tags")?;
    Ok(raw
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tag| {
            let name = tag.get("name").and_then(Value::as_str)?.to_string();
            let id = tag.get("id").and_then(Value::as_str)?.to_string();
            Some((name, id))
        })
        .collect())
}

/// The `tags` array of an issue create/update body, from resolved ids.
#[must_use]
pub fn tags_body(ids: &[String]) -> Value {
    Value::Array(ids.iter().map(|id| json!({"id": id})).collect())
}

/// Read a fetched issue's tag names back out.
#[must_use]
pub fn labels_from_tags(raw: &Value) -> Vec<String> {
    raw.get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tag| tag.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::http::{HttpClient, RetryPolicy, Token};
    use test_support::mock_http::MockServer;
    use test_support::youtrack_fixtures::MUST_BE_UNIQUE;

    const TAGS_PATH: &str = "/api/tags?fields=id,name&$top=500";

    fn client(server: &MockServer) -> HttpClient {
        HttpClient::new(&server.base_url(), Token::new("t"), RetryPolicy::none())
    }

    #[test]
    fn an_existing_tag_is_reused_and_never_recreated() {
        let server = MockServer::start();
        server.on(
            "GET",
            TAGS_PATH,
            200,
            r#"[{"id":"6-1","name":"sp-active"}]"#,
        );

        let mut cache = TagCache::load(&client(&server)).expect("load");
        let id = cache
            .resolve_or_create(&client(&server), "sp-active")
            .expect("resolve");

        assert_eq!(id.as_deref(), Some("6-1"));
        assert!(
            server.write_requests().is_empty(),
            "an existing tag must not be re-created"
        );
    }

    #[test]
    fn a_new_tag_is_created_once_and_cached() {
        let server = MockServer::start();
        server.on("GET", TAGS_PATH, 200, "[]");
        server.on(
            "POST",
            "/api/tags?fields=id,name",
            200,
            r#"{"id":"6-9","name":"mirror"}"#,
        );

        let http = client(&server);
        let mut cache = TagCache::load(&http).expect("load");
        let first = cache.resolve_or_create(&http, "mirror").expect("first");
        let second = cache.resolve_or_create(&http, "mirror").expect("second");

        assert_eq!(first, second);
        assert_eq!(
            server.write_requests().len(),
            1,
            "the cache must prevent a second POST"
        );
    }

    #[test]
    fn a_permission_denied_tag_is_skipped_and_reported_not_fatal() {
        let server = MockServer::start();
        server.on("GET", TAGS_PATH, 200, "[]");
        server.on(
            "POST",
            "/api/tags?fields=id,name",
            403,
            r#"{"error":"Forbidden"}"#,
        );

        let http = client(&server);
        let mut cache = TagCache::load(&http).expect("load");
        let result = cache
            .resolve_or_create(&http, "restricted")
            .expect("must not be fatal");

        assert_eq!(result, None, "a gated tag yields None rather than an error");
        assert_eq!(
            cache.skipped(),
            ["restricted"],
            "and is reported at the end of the run"
        );
    }

    #[test]
    fn an_already_exists_race_is_recovered_by_re_resolving_the_name() {
        let server = MockServer::start();
        // `load`'s GET sees no tag named "mirror" yet; a concurrent creator
        // wins the race between that load and our own create attempt, so by
        // the time we re-fetch to recover, it is there.
        server.on_sequence(
            "GET",
            TAGS_PATH,
            vec![
                (200, "[]".to_string()),
                (200, r#"[{"id":"6-5","name":"mirror"}]"#.to_string()),
            ],
        );
        server.on("POST", "/api/tags?fields=id,name", 409, MUST_BE_UNIQUE);

        let http = client(&server);
        let mut cache = TagCache::load(&http).expect("load");
        let id = cache
            .resolve_or_create(&http, "mirror")
            .expect("must not be fatal");

        assert_eq!(
            id.as_deref(),
            Some("6-5"),
            "a concurrent create must be recovered by re-resolving the name"
        );
        assert!(
            cache.skipped().is_empty(),
            "a recovered tag must not also be reported as skipped"
        );
    }

    #[test]
    fn an_already_exists_race_that_cannot_be_recovered_is_skipped_not_fatal() {
        let server = MockServer::start();
        // Every GET — the initial load and the post-conflict re-fetch —
        // comes back empty: the tag exists per YouTrack's own "must be
        // unique" answer, but this token can never see it (e.g. it belongs
        // to a project this token has no access to).
        server.on("GET", TAGS_PATH, 200, "[]");
        server.on("POST", "/api/tags?fields=id,name", 409, MUST_BE_UNIQUE);

        let http = client(&server);
        let mut cache = TagCache::load(&http).expect("load");
        let result = cache
            .resolve_or_create(&http, "mirror")
            .expect("must not be fatal");

        assert_eq!(
            result, None,
            "an unrecoverable AlreadyExists must degrade, not fail the run"
        );
        assert_eq!(cache.skipped(), ["mirror"]);
    }

    #[test]
    fn labels_round_trip_through_tags() {
        let raw = serde_json::json!({"tags":[{"id":"6-1","name":"sp-active"},{"id":"6-2","name":"mirror"}]});
        assert_eq!(labels_from_tags(&raw), ["sp-active", "mirror"]);
        let body = tags_body(&["6-1".into(), "6-2".into()]);
        assert_eq!(body[0]["id"], "6-1");
    }
}
