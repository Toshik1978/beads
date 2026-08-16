//! Deciding whether a bundle is shared, as a fact rather than a guess.
//!
//! One request returns every `(field, project)` pair that uses every bundle.
//! A bundle whose only user is the field we are about to modify is provably
//! private; anything else is shared. An earlier design decided this by whether
//! the bundle was named `"<project>: <Field>"`, which on the reference
//! instance would have cloned three bundles that each had exactly one user:
//! `Types`, `States` and `Priorities` all carry stock names and all have
//! exactly one user apiece.
//!
//! Two projects is not the only way to be shared. On the reference instance
//! `EasyMoney: Versions` is used by two fields of the *same* project
//! (`Fix versions` and `Affected versions`), and a value added for one would
//! appear in the other — so the verdict keys on `(field, project)` pairs, not
//! on the project set.
//!
//! `Unavailable` is not `Private`. The scan only sees projects the token may
//! read, so a 403 — or any other failure — must refuse rather than fall
//! through to the branch that writes.

use crate::remote::error::RemoteError;
use crate::remote::youtrack::admin::{AdminClient, json_array, json_str};
use std::collections::{BTreeMap, BTreeSet};

const SCAN_FIELDS: &str = "id,name,instances(id,project(shortName),bundle(id,name))";

/// Page size for [`AdminClient::scan_bundle_usage`].
const SCAN_PAGE_SIZE: u32 = 500;

/// One `(field, project)` pair that uses a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleUser {
    pub field_name: String,
    pub project_short_name: String,
    pub instance_id: String,
}

impl BundleUser {
    /// How this user is named in a refusal message.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}@{}", self.field_name, self.project_short_name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sharedness {
    /// The only user is the field we are about to modify. Safe to add values.
    Private,
    /// Someone else uses this bundle. `others` are their labels.
    Shared { others: Vec<String> },
    /// The scan could not be completed, so nothing is known. Refuses.
    Unavailable { reason: String },
}

#[derive(Debug, Clone, Default)]
pub struct BundleUsage {
    pub users: BTreeMap<String, Vec<BundleUser>>,
    pub projects_seen: BTreeSet<String>,
    unavailable: Option<String>,
}

impl BundleUsage {
    /// A usage map that knows nothing, because the scan failed.
    ///
    /// Takes the error by value, against `needless_pass_by_value`, so it can
    /// be named directly as `result.unwrap_or_else(BundleUsage::unavailable_from)`
    /// — the one call shape this constructor exists for. A `&RemoteError`
    /// parameter would force every caller to write a closure instead.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn unavailable_from(err: RemoteError) -> Self {
        Self {
            unavailable: Some(err.to_string()),
            ..Self::default()
        }
    }

    /// Whether the scan itself failed, as opposed to succeeding and simply
    /// not mentioning some bundle.
    ///
    /// Both produce `Sharedness::Unavailable`, and they have opposite
    /// remedies: a forbidden scan is fixed by widening the token's read
    /// access, while a bundle absent from a scan that *did* complete is not —
    /// there is nothing to grant. A caller writing a refusal must ask this
    /// before it advises anything.
    #[must_use]
    pub fn scan_failed(&self) -> bool {
        self.unavailable.is_some()
    }

    /// Whether `bundle_id` is safe for br to add values to.
    #[must_use]
    pub fn verdict(&self, bundle_id: &str, field_name: &str, project: &str) -> Sharedness {
        if let Some(reason) = &self.unavailable {
            return Sharedness::Unavailable {
                reason: reason.clone(),
            };
        }
        let Some(users) = self.users.get(bundle_id) else {
            return Sharedness::Unavailable {
                reason: format!("bundle {bundle_id} did not appear in the scan"),
            };
        };
        let others: Vec<String> = users
            .iter()
            .filter(|u| !(u.field_name == field_name && u.project_short_name == project))
            .map(BundleUser::label)
            .collect();
        if others.is_empty() {
            Sharedness::Private
        } else {
            Sharedness::Shared { others }
        }
    }
}

impl AdminClient {
    /// Read the complete bundle-to-user map, paged.
    ///
    /// An unpaginated `$top=500` read here fails in the one direction this
    /// scan exists to prevent: a bundle whose only user is a field past the
    /// first page would be missing from `usage.users` exactly as if it had
    /// no users at all, and `BundleUsage::verdict` reports that as
    /// `Unavailable`, not `Private` — so the immediate effect is a spurious
    /// refusal, not a wrongly-permitted write. This loops on `$skip`/`$top`
    /// the same way `fetch::fetch_snapshot` does, stopping on the first short
    /// page, so a truncated-looking absence never reaches `verdict` at all:
    /// what it sees is either a *complete* scan or `Err` from this method
    /// (which callers convert to `BundleUsage::unavailable_from`), never a
    /// partial one silently passed off as whole. It carries no sort clause:
    /// the field/instance list this scans is a small, effectively static
    /// collection for the life of one run, not the issue list
    /// `fetch::fetch_snapshot` is guarding against concurrent edits to.
    ///
    /// # Errors
    /// Returns `RemoteError` if any page's request fails. Callers must
    /// convert that into `BundleUsage::unavailable_from` rather than
    /// proceeding — a scan the token cannot complete is not evidence of
    /// privacy.
    pub fn scan_bundle_usage(&self) -> Result<BundleUsage, RemoteError> {
        let mut usage = BundleUsage::default();
        let mut skip = 0_u32;
        loop {
            let path = format!(
                "/api/admin/customFieldSettings/customFields?fields={SCAN_FIELDS}&$skip={skip}&$top={SCAN_PAGE_SIZE}"
            );
            let value = self.http().get_json(&path, "bundle usage scan")?;
            let page = json_array(Some(&value));
            let count = u32::try_from(page.len()).unwrap_or(u32::MAX);
            for field in page {
                let field_name = json_str(field, "name");
                for instance in json_array(field.get("instances")) {
                    let Some(bundle_id) = instance.get("bundle").map(|b| json_str(b, "id")) else {
                        continue;
                    };
                    if bundle_id.is_empty() {
                        continue;
                    }
                    let project = instance
                        .get("project")
                        .map(|p| json_str(p, "shortName"))
                        .unwrap_or_default();
                    usage.projects_seen.insert(project.to_string());
                    usage
                        .users
                        .entry(bundle_id.to_string())
                        .or_default()
                        .push(BundleUser {
                            field_name: field_name.to_string(),
                            project_short_name: project.to_string(),
                            instance_id: json_str(instance, "id").to_string(),
                        });
                }
            }
            if count < SCAN_PAGE_SIZE {
                return Ok(usage);
            }
            skip += SCAN_PAGE_SIZE;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::http::{HttpClient, RetryPolicy, Token};
    use crate::remote::youtrack::admin::AdminClient;
    use test_support::mock_http::MockServer;

    const SCAN_PATH: &str = "/api/admin/customFieldSettings/customFields?fields=id,name,instances(id,project(shortName),bundle(id,name))&$skip=0&$top=500";

    fn admin(server: &MockServer) -> AdminClient {
        AdminClient::new(HttpClient::new(
            &server.base_url(),
            Token::new("t"),
            RetryPolicy::none(),
        ))
    }

    /// The reference instance's real shape: stock bundle names, one user each.
    fn reference_scan() -> String {
        serde_json::json!([
            {"id":"161-1","name":"Type","instances":[
                {"id":"189-8","project":{"shortName":"EM"},"bundle":{"id":"163-1","name":"Types"}}]},
            {"id":"161-2","name":"State","instances":[
                {"id":"189-9","project":{"shortName":"EM"},"bundle":{"id":"165-0","name":"States"}}]},
            {"id":"161-5","name":"Fix versions","instances":[
                {"id":"189-11","project":{"shortName":"EM"},"bundle":{"id":"168-2","name":"EasyMoney: Versions"}}]},
            {"id":"161-6","name":"Affected versions","instances":[
                {"id":"189-12","project":{"shortName":"EM"},"bundle":{"id":"168-2","name":"EasyMoney: Versions"}}]}
        ])
        .to_string()
    }

    #[test]
    fn a_stock_named_bundle_with_one_user_is_private() {
        let server = MockServer::start();
        server.on("GET", SCAN_PATH, 200, &reference_scan());

        let usage = admin(&server).scan_bundle_usage().expect("scan");
        assert_eq!(
            usage.verdict("163-1", "Type", "EM"),
            Sharedness::Private,
            "a stock NAME does not make a bundle shared; only a second user does"
        );
    }

    #[test]
    fn a_bundle_used_by_a_second_project_is_shared() {
        let server = MockServer::start();
        let scan = serde_json::json!([
            {"id":"161-1","name":"Type","instances":[
                {"id":"189-8","project":{"shortName":"EM"},"bundle":{"id":"163-1","name":"Types"}},
                {"id":"189-20","project":{"shortName":"OTHER"},"bundle":{"id":"163-1","name":"Types"}}]}
        ]);
        server.on("GET", SCAN_PATH, 200, &scan.to_string());

        let usage = admin(&server).scan_bundle_usage().expect("scan");
        match usage.verdict("163-1", "Type", "EM") {
            Sharedness::Shared { others } => assert!(
                others.iter().any(|o| o.contains("OTHER")),
                "must name the other user: {others:?}"
            ),
            other => panic!("expected Shared, got {other:?}"),
        }
    }

    #[test]
    fn a_bundle_used_by_two_fields_of_one_project_is_shared() {
        let server = MockServer::start();
        server.on("GET", SCAN_PATH, 200, &reference_scan());

        let usage = admin(&server).scan_bundle_usage().expect("scan");
        match usage.verdict("168-2", "Fix versions", "EM") {
            Sharedness::Shared { others } => assert!(
                others.iter().any(|o| o.contains("Affected versions")),
                "a second FIELD of the same project shares the bundle too: {others:?}"
            ),
            other => panic!("expected Shared, got {other:?}"),
        }
    }

    #[test]
    fn a_forbidden_scan_is_unavailable_not_private() {
        let server = MockServer::start();
        server.on("GET", SCAN_PATH, 403, r#"{"error":"Forbidden"}"#);

        let result = admin(&server).scan_bundle_usage();
        let usage = result.unwrap_or_else(BundleUsage::unavailable_from);
        assert!(
            matches!(
                usage.verdict("163-1", "Type", "EM"),
                Sharedness::Unavailable { .. }
            ),
            "a scan the token cannot complete must refuse, never fall through to Private"
        );
    }

    #[test]
    fn a_bundle_absent_from_the_scan_is_unavailable_not_private() {
        let server = MockServer::start();
        server.on("GET", SCAN_PATH, 200, &reference_scan());

        let usage = admin(&server).scan_bundle_usage().expect("scan");
        assert!(
            matches!(
                usage.verdict("999-9", "Type", "EM"),
                Sharedness::Unavailable { .. }
            ),
            "a bundle the scan never mentioned is unknown, not proven private"
        );
        assert!(
            !usage.scan_failed(),
            "the scan completed; only this one bundle was missing from it"
        );
    }

    #[test]
    fn a_failed_scan_and_a_missing_bundle_are_distinguishable() {
        // Both answer Unavailable, and their remedies are opposites: one is
        // fixed by widening the token, the other cannot be.
        let server = MockServer::start();
        server.on("GET", SCAN_PATH, 403, r#"{"error":"Forbidden"}"#);
        let forbidden = admin(&server)
            .scan_bundle_usage()
            .unwrap_or_else(BundleUsage::unavailable_from);
        assert!(forbidden.scan_failed());

        let ok = MockServer::start();
        ok.on("GET", SCAN_PATH, 200, &reference_scan());
        assert!(!admin(&ok).scan_bundle_usage().expect("scan").scan_failed());
    }

    #[test]
    fn the_projects_seen_are_reported() {
        let server = MockServer::start();
        server.on("GET", SCAN_PATH, 200, &reference_scan());

        let usage = admin(&server).scan_bundle_usage().expect("scan");
        assert!(usage.projects_seen.contains("EM"));
    }

    #[test]
    fn a_bundle_used_only_by_a_field_past_the_first_page_is_still_seen() {
        // The original bug: an unpaginated `$top=500` scan made a bundle
        // whose only user lived past the first page indistinguishable from a
        // bundle with no users at all — `verdict` reports both as
        // `Unavailable`, a spurious refusal rather than the correct `Shared`
        // answer. This pins that the scan itself now pages, and that a
        // second-page user is counted exactly as a first-page one would be.
        let server = MockServer::start();
        let first_page: Vec<_> = (0..500)
            .map(|n| {
                serde_json::json!({"id": format!("161-{n}"), "name": format!("Field {n}"),
                "instances": []})
            })
            .collect();
        server.on(
            "GET",
            SCAN_PATH,
            200,
            &serde_json::Value::Array(first_page).to_string(),
        );
        server.on(
            "GET",
            "/api/admin/customFieldSettings/customFields?fields=id,name,instances(id,project(shortName),bundle(id,name))&$skip=500&$top=500",
            200,
            &serde_json::json!([
                {"id":"161-999","name":"Type","instances":[
                    {"id":"189-8","project":{"shortName":"EM"},"bundle":{"id":"163-1","name":"Types"}},
                    {"id":"189-80","project":{"shortName":"OTHER"},"bundle":{"id":"163-1","name":"Types"}}]}
            ])
            .to_string(),
        );

        let usage = admin(&server)
            .scan_bundle_usage()
            .expect("scan must page past the boundary");
        match usage.verdict("163-1", "Type", "EM") {
            Sharedness::Shared { others } => assert!(
                others.iter().any(|o| o.contains("OTHER")),
                "the second-page user must be counted: {others:?}"
            ),
            other => panic!(
                "expected Shared from a user on the second page, got {other:?} \
                 (a truncated scan would report Unavailable instead)"
            ),
        }
    }

    #[test]
    fn a_bundles_sole_user_on_the_second_page_makes_it_private_not_unavailable() {
        // The direction the previous test does not cover, and the one that
        // actually licenses a write: a bundle whose *only* user lives past
        // the first page. Before pagination that read as no user at all —
        // `Unavailable`, a refusal — which was at least safe. A scan that
        // pages but still gets this wrong in the other direction would be
        // worse than the bug it replaced: it would let `--allow-shared-bundle`
        // sit unnecessary and a plain mutating write proceed against a bundle
        // whose one real user was simply on a later page, not because it
        // is genuinely private. This pins that a sole second-page user still
        // resolves to `Private`, the same as a sole first-page user would.
        let server = MockServer::start();
        let first_page: Vec<_> = (0..500)
            .map(|n| {
                serde_json::json!({"id": format!("161-{n}"), "name": format!("Field {n}"),
                "instances": []})
            })
            .collect();
        server.on(
            "GET",
            SCAN_PATH,
            200,
            &serde_json::Value::Array(first_page).to_string(),
        );
        server.on(
            "GET",
            "/api/admin/customFieldSettings/customFields?fields=id,name,instances(id,project(shortName),bundle(id,name))&$skip=500&$top=500",
            200,
            &serde_json::json!([
                {"id":"161-999","name":"Type","instances":[
                    {"id":"189-8","project":{"shortName":"EM"},"bundle":{"id":"163-1","name":"Types"}}]}
            ])
            .to_string(),
        );

        let usage = admin(&server)
            .scan_bundle_usage()
            .expect("scan must page past the boundary");
        assert_eq!(
            usage.verdict("163-1", "Type", "EM"),
            Sharedness::Private,
            "a sole user found only on the second page is still exactly one user, \
             so the bundle is private — reporting Unavailable here would be a \
             regression from the pre-pagination behaviour, and reporting Shared \
             would license a write the bundle's real usage does not justify"
        );
    }
}
