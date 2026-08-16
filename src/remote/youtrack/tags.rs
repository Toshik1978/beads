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
        let by_name = fetch_all_tags(http)?;
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
                //
                // This queries by name (`query=`) rather than re-paging the
                // whole tag list: `load`'s enumeration exists to build a
                // cache of every tag, but recovery already knows the one name
                // it needs, and a full re-fetch would face the exact
                // truncation this recovery exists to survive — a tag beyond
                // the first page would be invisible to it too. A targeted
                // query has no page boundary to fall behind.
                if let Some(id) = fetch_tag_by_name(http, name)? {
                    self.by_name.insert(name.to_string(), id.clone());
                    Ok(Some(id))
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

/// Page size for the tag enumeration. Mirrors
/// `crate::remote::youtrack::fetch`'s issue paging: a page this size or
/// larger means there is more to ask for.
const TAG_PAGE_SIZE: u32 = 500;

/// GET every existing tag's name and id, paged.
///
/// A single `$top=500` request silently dropped every tag past the first
/// page on an instance with more: a label whose tag lived beyond it was
/// invisible to the cache, its create then 409-ed as a duplicate, and
/// nothing after that saw it either. This loops on `$skip`/`$top` the same
/// way `fetch::fetch_snapshot` does, stopping on the first short page.
///
/// # Errors
/// Returns whatever `http` returns on transport or HTTP failure.
fn fetch_all_tags(http: &HttpClient) -> Result<HashMap<String, String>, RemoteError> {
    let mut by_name = HashMap::new();
    let mut skip = 0_u32;
    loop {
        let path = format!("/api/tags?fields=id,name&$skip={skip}&$top={TAG_PAGE_SIZE}");
        let raw = http.get_json(&path, "tags")?;
        let page: Vec<&Value> = raw.as_array().into_iter().flatten().collect();
        let count = u32::try_from(page.len()).unwrap_or(u32::MAX);
        for tag in page {
            if let (Some(name), Some(id)) = (
                tag.get("name").and_then(Value::as_str),
                tag.get("id").and_then(Value::as_str),
            ) {
                by_name.insert(name.to_string(), id.to_string());
            }
        }
        if count < TAG_PAGE_SIZE {
            return Ok(by_name);
        }
        skip += TAG_PAGE_SIZE;
    }
}

/// GET the tag(s) matching `name` exactly, by a name query rather than
/// enumeration.
///
/// YouTrack's `/api/tags` accepts `query=` as a name search — the same
/// affordance `Tag.findByName` exposes — and the caller here already knows
/// the exact name it needs, so this asks for that one tag instead of paging
/// the whole list looking for it. `query=` is a search, not an exact filter
/// (case-insensitive substring matches can come back too), so the result is
/// still checked for a tag whose name matches byte-for-byte.
///
/// Carries an explicit `$top` for the same reason every other admin read in
/// this subsystem does: a name that is a substring of many other tags could
/// otherwise push the exact match past the server's own default page size,
/// which would relocate truncation-reads-as-absence into the recovery path
/// this function exists to serve. `TAG_PAGE_SIZE` is generous for a search
/// this narrow, and the exact-name post-filter below already handles a page
/// carrying more hits than the one being looked for; full pagination is not
/// worth it on a query this targeted.
///
/// # Errors
/// Returns whatever `http` returns on transport or HTTP failure.
fn fetch_tag_by_name(http: &HttpClient, name: &str) -> Result<Option<String>, RemoteError> {
    let path = format!(
        "/api/tags?fields=id,name&query={}&$top={TAG_PAGE_SIZE}",
        percent_encode(name)
    );
    let raw = http.get_json(&path, "tags")?;
    Ok(raw
        .as_array()
        .into_iter()
        .flatten()
        .find(|tag| tag.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|tag| tag.get("id").and_then(Value::as_str))
        .map(str::to_string))
}

/// Percent-encode `value` for use as one query-string value.
///
/// `fetch.rs`'s literal `%20` is fine there because it is only ever spliced
/// into fixed strings this crate wrote itself (`project:`, `sort by:`). A
/// tag name is arbitrary text a user typed — spaces, punctuation, non-ASCII —
/// so it gets a real byte-wise encoder rather than another hand-written
/// substitution.
fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0xF) as usize] as char);
            }
        }
    }
    out
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

    const TAGS_PATH: &str = "/api/tags?fields=id,name&$skip=0&$top=500";

    fn tags_page_path(skip: u32) -> String {
        format!("/api/tags?fields=id,name&$skip={skip}&$top=500")
    }

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
        // the time recovery asks for it, it is there.
        server.on("GET", TAGS_PATH, 200, "[]");
        server.on("POST", "/api/tags?fields=id,name", 409, MUST_BE_UNIQUE);
        server.on(
            "GET",
            "/api/tags?fields=id,name&query=mirror&$top=500",
            200,
            r#"[{"id":"6-5","name":"mirror"}]"#,
        );

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
    fn recovery_finds_a_tag_beyond_the_page_the_cache_ever_saw() {
        // The whole point of `bds-4r2.13`: a tag whose name lives past the
        // first page of `load` must still be recoverable after a
        // `must-be-unique` conflict. `load` here actually pages through a
        // full first page of 500 tags and a short, empty second page — a
        // cache that genuinely finished enumerating and still does not hold
        // "sp-active" — rather than the empty-cache setup
        // `an_already_exists_race_is_recovered_by_re_resolving_the_name`
        // above already covers, so this proves something that test does not:
        // recovery uses a name query rather than re-paging, so it never
        // faces a page boundary at all, and finds a tag created by someone
        // else after `load` completed rather than merely one `load` had not
        // gotten around to yet.
        let server = MockServer::start();
        let first_page: Vec<_> = (0..500)
            .map(|n| serde_json::json!({"id": format!("6-{n}"), "name": format!("tag-{n}")}))
            .collect();
        server.on(
            "GET",
            &tags_page_path(0),
            200,
            &serde_json::Value::Array(first_page).to_string(),
        );
        server.on("GET", &tags_page_path(500), 200, "[]");
        server.on("POST", "/api/tags?fields=id,name", 409, MUST_BE_UNIQUE);
        server.on(
            "GET",
            "/api/tags?fields=id,name&query=sp-active&$top=500",
            200,
            r#"[{"id":"6-501","name":"sp-active"}]"#,
        );

        let http = client(&server);
        let mut cache = TagCache::load(&http).expect("load");
        let id = cache
            .resolve_or_create(&http, "sp-active")
            .expect("must not be fatal");

        assert_eq!(id.as_deref(), Some("6-501"));
    }

    #[test]
    fn an_already_exists_race_that_cannot_be_recovered_is_skipped_not_fatal() {
        let server = MockServer::start();
        // Both the initial load and the post-conflict name query come back
        // empty: the tag exists per YouTrack's own "must be unique" answer,
        // but this token can never see it (e.g. it belongs to a project this
        // token has no access to).
        server.on("GET", TAGS_PATH, 200, "[]");
        server.on("POST", "/api/tags?fields=id,name", 409, MUST_BE_UNIQUE);
        server.on(
            "GET",
            "/api/tags?fields=id,name&query=mirror&$top=500",
            200,
            "[]",
        );

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
    fn recovery_matches_the_name_exactly_not_a_query_substring_hit() {
        // `query=` is a search, not an exact filter — YouTrack's own
        // `Tag.findByName` can return more than one tag. A hit that merely
        // *contains* the searched name must not be accepted as if it were
        // an exact match.
        let server = MockServer::start();
        server.on("GET", TAGS_PATH, 200, "[]");
        server.on("POST", "/api/tags?fields=id,name", 409, MUST_BE_UNIQUE);
        server.on(
            "GET",
            "/api/tags?fields=id,name&query=mirror&$top=500",
            200,
            r#"[{"id":"6-1","name":"mirror-2"},{"id":"6-2","name":"mirror"}]"#,
        );

        let http = client(&server);
        let mut cache = TagCache::load(&http).expect("load");
        let id = cache
            .resolve_or_create(&http, "mirror")
            .expect("must not be fatal");

        assert_eq!(id.as_deref(), Some("6-2"), "only the exact name must match");
    }

    #[test]
    fn a_tag_load_pages_past_the_first_five_hundred() {
        // The original bug: `load` asked for `$top=500` with no `$skip`, so
        // a tag past the first page was invisible to the cache and a create
        // for it 409-ed forever. This pins that `load` itself now pages.
        let server = MockServer::start();
        let first_page: Vec<_> = (0..500)
            .map(|n| serde_json::json!({"id": format!("6-{n}"), "name": format!("tag-{n}")}))
            .collect();
        server.on(
            "GET",
            &tags_page_path(0),
            200,
            &serde_json::Value::Array(first_page).to_string(),
        );
        server.on(
            "GET",
            &tags_page_path(500),
            200,
            r#"[{"id":"6-999","name":"sp-active"}]"#,
        );

        let http = client(&server);
        let mut cache = TagCache::load(&http).expect("load");
        let id = cache
            .resolve_or_create(&http, "sp-active")
            .expect("resolve");

        assert_eq!(
            id.as_deref(),
            Some("6-999"),
            "a tag beyond the first page must be visible to the cache"
        );
        assert!(
            server.write_requests().is_empty(),
            "a tag the paginated load actually found must not be re-created"
        );
    }

    #[test]
    fn a_tag_name_with_spaces_is_percent_encoded_in_the_recovery_query() {
        let server = MockServer::start();
        server.on("GET", TAGS_PATH, 200, "[]");
        server.on("POST", "/api/tags?fields=id,name", 409, MUST_BE_UNIQUE);
        server.on(
            "GET",
            "/api/tags?fields=id,name&query=sp%20active&$top=500",
            200,
            r#"[{"id":"6-7","name":"sp active"}]"#,
        );

        let http = client(&server);
        let mut cache = TagCache::load(&http).expect("load");
        let id = cache
            .resolve_or_create(&http, "sp active")
            .expect("must not be fatal");

        assert_eq!(id.as_deref(), Some("6-7"));
    }

    #[test]
    fn labels_round_trip_through_tags() {
        let raw = serde_json::json!({"tags":[{"id":"6-1","name":"sp-active"},{"id":"6-2","name":"mirror"}]});
        assert_eq!(labels_from_tags(&raw), ["sp-active", "mirror"]);
        let body = tags_body(&["6-1".into(), "6-2".into()]);
        assert_eq!(body[0]["id"], "6-1");
    }
}
