//! Driving one `br remote` reconciliation from a test.
//!
//! `br remote pull`/`push`/`sync` are still stubs, so every test of the
//! adoption engine has to assemble the pipeline the verbs will assemble:
//! fetch, pair, classify, order, create parent-first, then import links in a
//! second pass. That is forty lines, it was copied into two test files
//! already, and the verb itself will be the third copy.
//!
//! It lives here so there is exactly one description of the order those calls
//! go in. When the verbs land, [`adopt_everything`] is the shape to compare
//! them against — and the tests that call it can be re-pointed at the real
//! subprocess without changing a single assertion.

use beads::error::Result;
use beads::model::Issue;
use beads::remote::adopt::{
    AdoptContext, AdoptionCandidate, AdoptionOutcome, DeferredAdoption, RefusedAdoption, adopt_one,
    classify_adoption, creation_parent, import_links, topological_order,
};
use beads::remote::config::RemoteConfig;
use beads::remote::http::{HttpClient, RetryPolicy, Token};
use beads::remote::reconcile::pair_workspace;
use beads::remote::youtrack::fetch::fetch_snapshot;
use beads::remote::youtrack::links::LinkTypes;
use beads::storage::SqliteStorage;
use beads::util::id::IdConfig;
use std::collections::HashMap;
use std::path::Path;

/// An `HttpClient` pointed at a loopback mock, with retries off so a test
/// never waits on a backoff.
#[must_use]
pub fn client(base_url: &str) -> HttpClient {
    HttpClient::new(base_url, Token::new("t"), RetryPolicy::none())
}

/// The id settings `br init --prefix <prefix>` leaves behind.
#[must_use]
pub fn id_config(prefix: &str) -> IdConfig {
    IdConfig {
        prefix: prefix.to_string(),
        min_hash_length: 3,
        max_hash_length: 8,
        max_collision_prob: 0.25,
    }
}

/// The workspace database `br init` created under `beads_dir`.
///
/// # Panics
/// Panics if the database cannot be opened.
#[must_use]
pub fn open_storage(beads_dir: &Path) -> SqliteStorage {
    SqliteStorage::open(&beads_dir.join("beads.db")).expect("open workspace db")
}

/// Every issue with its relations and labels attached.
///
/// `list_issues` and `get_all_issues_for_export` return bare rows — no
/// `dependencies`, no `labels` — and the link differ is only as good as the
/// relations it is handed, so a test that skips this hydration reports every
/// mirrored link as locally absent. Mirrors `cli::commands::remote`.
///
/// # Panics
/// Panics if any of the three export reads fails.
#[must_use]
pub fn hydrated_issues(storage: &SqliteStorage) -> Vec<Issue> {
    let mut issues = storage.get_all_issues_for_export().expect("issues");
    let mut dependencies = storage.get_all_dependency_records().expect("dependencies");
    let mut labels = storage.get_labels_for_export().expect("labels");
    for issue in &mut issues {
        if let Some(rows) = dependencies.remove(&issue.id) {
            issue.dependencies = rows;
        }
        if let Some(names) = labels.remove(&issue.id) {
            issue.labels = names;
        }
    }
    issues
}

/// Everything one adoption pass decided and did.
#[derive(Debug, Default)]
pub struct AdoptionRun {
    /// Adopted, in creation order.
    pub adopted: Vec<AdoptionOutcome>,
    /// Refused: br cannot read these with this `remote.yaml`.
    pub refused: Vec<RefusedAdoption>,
    /// Deferred: readable, but their parent is not available this run.
    pub deferred: Vec<DeferredAdoption>,
    /// Remote `idReadable` → bead id, holding both the pre-existing pairings
    /// and every id minted by this run.
    pub id_map: HashMap<String, String>,
}

/// Run the adoption half of one reconciliation against `base_url`.
///
/// The call order here is the contract, and it is why this function exists
/// rather than four copies of it:
///
/// 1. fetch and pair, seeding `id_map` from the existing pairings;
/// 2. `classify_adoption` each unpaired issue — refusals are collected, not
///    fatal, and remove only their own issue from the run;
/// 3. `topological_order`, which yields parents before children and names
///    whatever it cannot place;
/// 4. `creation_parent` then `adopt_one` per candidate **in that order**,
///    inserting each new id into `id_map` as it goes, so the next child can
///    resolve its parent;
/// 5. `import_links` for the whole batch **only once step 4 has finished** —
///    a `Depend` or `Relates` may point at an adoptee that did not exist when
///    the other end was created.
///
/// # Errors
/// Returns whatever storage returns from `adopt_one` or `import_links`.
///
/// # Panics
/// Panics if the config, the link-type resolution or the fetch fails — those
/// are the test's setup, not the behaviour under test.
pub fn adopt_everything(beads_dir: &Path, base_url: &str, prefix: &str) -> Result<AdoptionRun> {
    let cfg = RemoteConfig::load(beads_dir).expect("remote.yaml");
    let http = client(base_url);
    let types = LinkTypes::resolve(&http).expect("link types");
    let snapshot = fetch_snapshot(&http, &cfg, &types).expect("fetch");

    let mut storage = open_storage(beads_dir);
    let beads = storage.get_all_issues_for_export().expect("beads");
    let pairing = pair_workspace(&cfg, &beads, snapshot.issues);

    let mut run = AdoptionRun {
        id_map: pairing
            .paired
            .iter()
            .map(|pair| (pair.remote.id_readable.clone(), pair.bead_id.clone()))
            .collect(),
        ..AdoptionRun::default()
    };

    let mut candidates: Vec<AdoptionCandidate> = Vec::new();
    for issue in &pairing.unpaired_remote {
        match classify_adoption(&cfg, issue) {
            Ok(candidate) => candidates.push(candidate),
            Err(refusal) => run.refused.push(refusal),
        }
    }

    let order = topological_order(candidates, &run.id_map);
    run.deferred = order.deferred;

    let ctx = AdoptContext {
        id_config: &id_config(prefix),
        actor: "youtrack",
    };
    for candidate in &order.ordered {
        let parent = creation_parent(candidate, &run.id_map);
        let outcome = adopt_one(&mut storage, &ctx, candidate, parent.as_deref())?;
        run.id_map.insert(
            candidate.remote.id_readable.clone(),
            outcome.bead_id.clone(),
        );
        run.adopted.push(outcome);
    }
    for candidate in &order.ordered {
        import_links(&mut storage, candidate, &run.id_map)?;
    }

    Ok(run)
}
