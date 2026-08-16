//! `br remote init` — schema reconciliation for a YouTrack project.
//!
//! Two of the things this command does are visible to people who never run
//! `br`, and both are deliberate rather than accidental:
//!
//! * Custom field **prototypes are instance-wide**. Creating `Design` makes it
//!   appear in every other project's administration UI. There is no
//!   per-project field namespace to use instead, so `init` says so rather than
//!   hiding it.
//! * `init` **rewrites the project's `Type`/`State`/`Priority` defaults** to
//!   the beads defaults. Stock YouTrack adopts a new issue as `Bug` /
//!   `Submitted` / `Normal`, which reverse-maps to *a deferred bug at P3* —
//!   the adoption case landing wrong on all three axes. Setting them to
//!   `Task` / `Open` / `Major` fixes that, and `--keep-project-defaults`
//!   turns it off for anyone who would rather keep the web UI as it was.
//!
//! The vocabulary pre-flight runs **before any network write**, and that
//! ordering is the whole point of it: both beads vocabularies are open, so a
//! workspace can hold an `issue_type` or `status` the maps do not cover, and
//! discovering that several hundred writes into a first run is a poor trade
//! against one local read here.

use crate::remote::config::RemoteConfig;
use crate::remote::error::RemoteError;
use crate::remote::youtrack::admin::{ALL_FIELDS, AdminClient, ProjectRef, json_array, json_str};
use crate::remote::youtrack::bundles::{BundleRef, PendingValue, plan_missing};
use crate::remote::youtrack::clone::{Emptiness, project_field_type};
use crate::remote::youtrack::sharedness::{BundleUsage, Sharedness};
use std::collections::BTreeSet;

/// The bundle-backed fields `init` reconciles, in the order it visits them.
pub const RECONCILED_FIELDS: [&str; 3] = ["Type", "State", "Priority"];

/// The distinct `issue_type` and `status` values a workspace actually holds.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceVocabulary {
    pub types: BTreeSet<String>,
    pub statuses: BTreeSet<String>,
}

/// What the two flags on `br remote init` turn off.
#[derive(Debug, Clone, Copy, Default)]
pub struct InitOptions {
    /// Add values to a demonstrably shared bundle anyway, rather than cloning
    /// or refusing.
    pub allow_shared_bundle: bool,
    /// Leave the project's `defaultValues` exactly as they are.
    pub keep_project_defaults: bool,
    /// Plan without writing anything.
    pub dry_run: bool,
}

/// One `defaultValues` rewrite, as `old → new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultChange {
    pub field_name: String,
    pub old: Vec<String>,
    pub new: String,
}

/// One bundle that was too shared to modify in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleClone {
    pub field_name: String,
    pub source: String,
    pub clone: String,
}

/// Everything `init` did, for the CLI layer to print.
#[derive(Debug, Clone, Default)]
pub struct InitReport {
    pub prototypes_created: Vec<String>,
    pub fields_attached: Vec<String>,
    /// `(field name, values added)`, in visit order.
    pub values_added: Vec<(String, Vec<String>)>,
    pub clones: Vec<BundleClone>,
    pub defaults_changed: Vec<DefaultChange>,
    pub projects_seen: Vec<String>,
    /// Things a human should know that are not counted anywhere else.
    pub notes: Vec<String>,
    pub dry_run: bool,
}

/// One entry of the project's custom-field list, read once and used three
/// times: to decide which of the five fields still need attaching, to read the
/// current `defaultValues` for the alignment step, and to learn each field
/// instance's own id.
#[derive(Debug, Clone)]
pub struct ProjectFieldSummary {
    pub instance_id: String,
    pub field_name: String,
    pub default_values: Vec<String>,
}

impl AdminClient {
    /// Read every custom field attached to `project`, with its current
    /// defaults.
    ///
    /// # Errors
    /// Returns `RemoteError` on transport or HTTP failure.
    pub fn read_project_field_summaries(
        &self,
        project: &ProjectRef,
    ) -> Result<Vec<ProjectFieldSummary>, RemoteError> {
        let path = format!(
            "/api/admin/projects/{}/customFields?fields=id,field(name),defaultValues(name)&$top=200",
            project.id
        );
        let value = self.http().get_json(&path, "project field summaries")?;
        Ok(json_array(Some(&value))
            .iter()
            .filter_map(|raw| {
                let field_name = raw.get("field").map(|f| json_str(f, "name"))?;
                if field_name.is_empty() {
                    return None;
                }
                Some(ProjectFieldSummary {
                    instance_id: json_str(raw, "id").to_string(),
                    field_name: field_name.to_string(),
                    default_values: json_array(raw.get("defaultValues"))
                        .iter()
                        .map(|v| json_str(v, "name").to_string())
                        .collect(),
                })
            })
            .collect())
    }

    /// The five field prototypes, as the instance currently holds them.
    ///
    /// Public so `init` can name what it is about to create instance-wide
    /// before it creates it. `ensure_field_prototypes` performs the same read
    /// internally, but discards the difference.
    ///
    /// # Errors
    /// Returns `RemoteError` on transport or HTTP failure.
    pub fn missing_field_prototypes(&self) -> Result<Vec<String>, RemoteError> {
        let value = self.http().get_json(
            "/api/admin/customFieldSettings/customFields?fields=id,name,fieldType(id)&$top=500",
            "custom field list",
        )?;
        let present: BTreeSet<&str> = json_array(Some(&value))
            .iter()
            .map(|f| json_str(f, "name"))
            .collect();
        Ok(ALL_FIELDS
            .iter()
            .filter(|(name, _)| !present.contains(name))
            .map(|(name, _)| (*name).to_string())
            .collect())
    }
}

/// Fail if the workspace holds a value no map names.
///
/// This runs before any write. Both beads vocabularies are open — `br` accepts
/// any `issue_type`, and any `status` unless `workflow.strict` narrows it — so
/// a bead can carry a value the maps do not cover. Discovering that 300 writes
/// into a first run is a poor trade against one read here.
///
/// # Errors
/// Returns `RemoteError::Config` naming every uncovered value and the config
/// key that would cover it.
pub fn preflight_vocabulary(
    cfg: &RemoteConfig,
    vocab: &WorkspaceVocabulary,
) -> Result<(), RemoteError> {
    let mut problems = Vec::new();
    for value in &vocab.types {
        if !cfg.type_map.contains_key(value) {
            problems.push(format!("  issue_type '{value}' — add it to type_map"));
        }
    }
    for value in &vocab.statuses {
        // `tombstone` is owned by the tombstone rule and is deliberately
        // absent from status_map; it is not an uncovered value.
        if value != "tombstone" && !cfg.status_map.contains_key(value) {
            problems.push(format!("  status '{value}' — add it to status_map"));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(RemoteError::Config(format!(
        "this workspace holds values that .beads/remote.yaml does not map:\n{}\n\
         Every value must be mapped before a push, because an unmapped value \
         cannot be written to the mirror and cannot be recognised on the way back.",
        problems.join("\n")
    )))
}

/// Provision `cfg.project` so this workspace can mirror into it.
///
/// The sequence is fixed, and each step is ordered against a failure it is
/// there to prevent — the pre-flight before the first write, the sharedness
/// scan before the first bundle write, the defaults alignment after any
/// re-point (which invalidates the old defaults' element ids).
///
/// # Errors
/// Returns `RemoteError::Config` for every refusal — an unmapped vocabulary
/// value, a case-only bundle collision, a shared bundle on a project that
/// already holds issues, a sharedness scan the token could not complete — and
/// the underlying `RemoteError` for a transport or HTTP failure.
pub fn run(
    cfg: &RemoteConfig,
    admin: &AdminClient,
    vocab: &WorkspaceVocabulary,
    opts: InitOptions,
) -> Result<InitReport, RemoteError> {
    let mut report = InitReport {
        dry_run: opts.dry_run,
        ..InitReport::default()
    };

    // 1. Refuse before touching the network at all.
    preflight_vocabulary(cfg, vocab)?;

    // 2. Resolve the project.
    let project = admin.resolve_project(&cfg.project)?;

    // 3. Prototypes, then the attach.
    provision_fields(admin, &project, opts, &mut report)?;

    // 4. Sharedness, as a fact. A scan that fails is Unavailable, never
    //    Private: the branch that assumes privacy is the branch that writes.
    let usage = admin
        .scan_bundle_usage()
        .unwrap_or_else(BundleUsage::unavailable_from);

    // 5. Say which projects the scan could see, *before* any bundle write —
    //    a token that cannot read a project cannot see that project's use of
    //    a bundle, and the reader is the only one who can notice a gap.
    report.projects_seen = usage.projects_seen.iter().cloned().collect();
    announce(&format!(
        "bundle sharedness scanned across {} project(s): {}",
        report.projects_seen.len(),
        if report.projects_seen.is_empty() {
            "none".to_string()
        } else {
            report.projects_seen.join(", ")
        }
    ));

    // 6. Reconcile the three bundle-backed fields.
    let summaries = admin.read_project_field_summaries(&project)?;
    let bundle_fields = admin.read_project_bundle_fields(&project)?;
    let mut final_bundles: Vec<(String, BundleRef)> = Vec::new();

    for field_name in RECONCILED_FIELDS {
        let Some(field) = bundle_fields.iter().find(|f| f.field_name == field_name) else {
            continue;
        };
        let missing = plan_missing(cfg, field_name, &field.bundle)?;
        let mut bundle = field.bundle.clone();
        if missing.is_empty() {
            // Nothing missing means zero writes: an existing value is never
            // re-added, reordered or recoloured.
            final_bundles.push((field_name.to_string(), bundle));
            continue;
        }

        let verdict = usage.verdict(&field.bundle.id, field_name, &project.short_name);
        match verdict {
            Sharedness::Private => {
                add_values(admin, &bundle, &missing, field_name, opts, &mut report)?;
            }
            Sharedness::Shared { ref others } if opts.allow_shared_bundle => {
                // Announced *before* the write, not filed in the report for
                // afterwards: this is the most consequential thing init does —
                // it changes a vocabulary another project reads — and the rule
                // stated on `announce` is that a disclosure printed after the
                // write has missed its moment.
                let note = format!(
                    "--allow-shared-bundle: adding {} to '{}', which {} also use(s) — \
                     those values will appear in their vocabulary too",
                    quoted_list(&missing),
                    field.bundle.name,
                    others.join(", ")
                );
                announce(&note);
                report.notes.push(note);
                add_values(admin, &bundle, &missing, field_name, opts, &mut report)?;
            }
            Sharedness::Unavailable { ref reason } if opts.allow_shared_bundle => {
                let note = format!(
                    "--allow-shared-bundle: adding {} to '{}' without a completed sharedness \
                     scan ({reason}) — if another project uses this bundle, those values will \
                     appear in its vocabulary too",
                    quoted_list(&missing),
                    field.bundle.name
                );
                announce(&note);
                report.notes.push(note);
                add_values(admin, &bundle, &missing, field_name, opts, &mut report)?;
            }
            Sharedness::Shared { ref others } => {
                bundle = reconcile_shared(
                    admin,
                    &project,
                    field,
                    &missing,
                    others,
                    &summaries,
                    &usage,
                    opts,
                    &mut report,
                )?;
            }
            Sharedness::Unavailable { ref reason } => {
                return Err(unavailable_refusal(
                    field_name,
                    &field.bundle.name,
                    reason,
                    &missing,
                    usage.scan_failed(),
                ));
            }
        }
        final_bundles.push((field_name.to_string(), bundle));
    }

    // 7. Align the defaults, last, because a re-point invalidates the old ones.
    if !opts.keep_project_defaults {
        align_defaults(
            admin,
            cfg,
            &project,
            &summaries,
            &final_bundles,
            opts,
            &mut report,
        )?;
    }

    Ok(report)
}

/// The refusal for `Sharedness::Unavailable`, split on its two causes.
///
/// They arrive as the same variant and have opposite remedies. A scan the
/// token could not complete is fixed by widening the token's read access. A
/// bundle that did not appear in a scan which *succeeded* is not: every
/// project the token can see was read, and this bundle was in none of them, so
/// there is no access to grant. Advising the first remedy for the second case
/// sends the reader after something that cannot work, and contradicts the
/// `{reason}` printed beside it.
fn unavailable_refusal(
    field_name: &str,
    bundle_name: &str,
    reason: &str,
    missing: &[PendingValue],
    scan_failed: bool,
) -> RemoteError {
    let head = format!(
        "cannot tell whether the bundle '{bundle_name}' behind '{field_name}' is shared: \
         {reason}. Adding {} to a bundle another project uses would change that project's \
         vocabulary, so this refuses rather than guessing.",
        quoted_list(missing)
    );
    let remedy = if scan_failed {
        "The scan itself was refused, so nothing is known about any bundle. Grant the token \
         read access to every project — a token that cannot read a project cannot see that \
         project's use of this bundle — or re-run with --allow-shared-bundle to add the \
         values anyway."
    } else {
        "The scan completed; this bundle was simply not in it, so no field of any project \
         the token can read uses it. That usually means the project field was re-pointed \
         between the scan and now, or the bundle is reachable only through a project the \
         token cannot see. Re-run to take a fresh scan, or use --allow-shared-bundle to add \
         the values anyway. Widening the token's access will not change this reading if the \
         bundle is genuinely unreferenced."
    };
    RemoteError::Config(format!("{head} {remedy}"))
}

fn provision_fields(
    admin: &AdminClient,
    project: &ProjectRef,
    opts: InitOptions,
    report: &mut InitReport,
) -> Result<(), RemoteError> {
    let missing = admin.missing_field_prototypes()?;
    if !missing.is_empty() {
        announce(&format!(
            "creating {} custom field prototype(s) — these are INSTANCE-WIDE and will \
             appear in every project's administration: {}",
            missing.len(),
            missing.join(", ")
        ));
    }
    report.prototypes_created.clone_from(&missing);

    let fields = if opts.dry_run {
        // A dry run must not create anything, so it also cannot learn the ids
        // of what it did not create. Attach planning below is by name only.
        Vec::new()
    } else {
        // Called whether or not anything is missing: it is also the only
        // source of the field ids the attach needs, and it issues no write
        // when the instance already has all five.
        admin.ensure_field_prototypes()?
    };

    let attached: BTreeSet<String> = admin
        .read_project_field_summaries(project)?
        .into_iter()
        .map(|s| s.field_name)
        .collect();
    let pending: Vec<_> = fields
        .iter()
        .filter(|f| !attached.contains(&f.name))
        .cloned()
        .collect();
    report.fields_attached = if opts.dry_run {
        ALL_FIELDS
            .iter()
            .filter(|(name, _)| !attached.contains(*name))
            .map(|(name, _)| (*name).to_string())
            .collect()
    } else {
        pending.iter().map(|f| f.name.clone()).collect()
    };
    if !opts.dry_run && !pending.is_empty() {
        admin.attach_fields_to_project(project, &pending)?;
    }
    Ok(())
}

fn add_values(
    admin: &AdminClient,
    bundle: &BundleRef,
    values: &[PendingValue],
    field_name: &str,
    opts: InitOptions,
    report: &mut InitReport,
) -> Result<(), RemoteError> {
    if !opts.dry_run {
        admin.add_bundle_values(bundle, values)?;
    }
    report.values_added.push((
        field_name.to_string(),
        values.iter().map(|v| v.name.clone()).collect(),
    ));
    Ok(())
}

/// The clone-and-re-point path, taken only when the bundle is provably shared
/// **and** the project is provably empty.
///
/// The emptiness probe is `POST /api/issuesGetter/count`, and a `--dry-run`
/// still issues it: YouTrack takes the query in a body so there is no GET
/// form, it changes no state, and the branch a dry run is reporting on cannot
/// be chosen without the answer. Every genuine write below is behind the
/// dry-run short-circuit.
#[allow(clippy::too_many_arguments)]
fn reconcile_shared(
    admin: &AdminClient,
    project: &ProjectRef,
    field: &crate::remote::youtrack::bundles::ProjectFieldBundle,
    missing: &[PendingValue],
    others: &[String],
    summaries: &[ProjectFieldSummary],
    usage: &BundleUsage,
    opts: InitOptions,
    report: &mut InitReport,
) -> Result<BundleRef, RemoteError> {
    let emptiness = admin.project_is_empty(project)?;
    match emptiness {
        Emptiness::HasIssues(n) => {
            return Err(RemoteError::Config(format!(
                "'{}' needs {} added to the bundle '{}', but {} also use(s) that bundle and \
                 the project '{}' already holds {n} issue(s). Cloning the bundle now would \
                 re-point every existing issue's field at a copy, so this refuses. Either add \
                 those values in YouTrack's administration yourself, or re-run with \
                 --allow-shared-bundle to accept that they become visible to {}.",
                field.field_name,
                quoted_list(missing),
                field.bundle.name,
                others.join(", "),
                project.short_name,
                others.join(", ")
            )));
        }
        Emptiness::Unknown => {
            return Err(RemoteError::Config(format!(
                "'{}' needs {} added to the shared bundle '{}', and YouTrack's issue count for \
                 '{}' never settled — it kept answering -1. Cloning a project that turns out to \
                 hold issues is exactly what the count guards against, so this refuses rather \
                 than assuming the project is empty. Re-run in a moment, or use \
                 --allow-shared-bundle to add the values to the shared bundle instead.",
                field.field_name,
                quoted_list(missing),
                field.bundle.name,
                project.short_name
            )));
        }
        Emptiness::Empty => {}
    }

    let clone_name = clone_name(project, &field.bundle);
    report.clones.push(BundleClone {
        field_name: field.field_name.clone(),
        source: field.bundle.name.clone(),
        clone: clone_name.clone(),
    });
    if opts.dry_run {
        report.values_added.push((
            field.field_name.clone(),
            missing.iter().map(|v| v.name.clone()).collect(),
        ));
        return Ok(field.bundle.clone());
    }

    // A previous run may have created this clone and died before re-pointing
    // the project at it. Adopt it rather than making a second one — see
    // `adopt_existing_clone`.
    let clone = match adopt_existing_clone(admin, field, missing, usage, &clone_name)? {
        Some(existing) => {
            let note = format!(
                "adopted the bundle '{}' left behind by an interrupted earlier run \
                 instead of creating a second copy",
                existing.name
            );
            announce(&note);
            report.notes.push(note);
            existing
        }
        None => admin.clone_bundle(&field.bundle, &clone_name)?,
    };

    admin.repoint_field_bundle(field, project, &clone)?;

    // An adopted clone may already hold some of what is missing, because the
    // interrupted run may have died partway through filling it. A fresh clone
    // holds none of it, so this is the full set there.
    let still_missing: Vec<PendingValue> = missing
        .iter()
        .filter(|v| !clone.values.iter().any(|existing| existing.name == v.name))
        .cloned()
        .collect();
    if !still_missing.is_empty() {
        admin.add_bundle_values(&clone, &still_missing)?;
    }
    report.values_added.push((
        field.field_name.clone(),
        still_missing.iter().map(|v| v.name.clone()).collect(),
    ));

    // Re-pointing dropped the project's defaults with the old bundle. Restore
    // them by name against the clone before step 7 has its say. Read after the
    // fill, so a default that is itself one of the new values resolves.
    let clone = admin.read_bundle(&clone)?;
    let previous: Vec<String> = summaries
        .iter()
        .find(|s| s.field_name == field.field_name)
        .map(|s| s.default_values.clone())
        .unwrap_or_default();
    if !previous.is_empty() {
        let names: Vec<&str> = previous.iter().map(String::as_str).collect();
        admin.set_default_values(
            project,
            &field.instance_id,
            project_field_type(clone.kind),
            &clone,
            &names,
        )?;
    }
    Ok(clone)
}

/// Find a bundle already named `clone_name` that is safe to reuse.
///
/// A run that dies between `clone_bundle` and `repoint_field_bundle` leaves a
/// bundle nothing references. The project's field still points at the shared
/// source, so the next run takes this same path and `clone_name` produces the
/// same name — and YouTrack permits duplicate bundle names, so creating again
/// *succeeds*, leaving the first copy orphaned forever and mentioning it
/// nowhere. One more copy accumulates per interruption, invisibly.
///
/// Adoption is only safe when the candidate is demonstrably the artefact of
/// such a run rather than somebody's own bundle that happens to share a name,
/// so all four of these must hold:
///
/// 1. **Exactly one** candidate carries the name. Two means an earlier version
///    already leaked more than once, and picking one of them is a guess.
/// 2. **Nothing references it.** Its id is absent from the sharedness scan,
///    which lists every bundle any readable project's field instance uses. An
///    adopted bundle is about to be re-pointed at, so reusing one that is
///    already in service would hand this project someone else's vocabulary.
/// 3. **It contains every value the source has**, by name — a clone is a
///    verbatim copy, so anything missing means it is not one.
/// 4. **It contains nothing else** except values from the pending set. A
///    bundle carrying a name we do not recognise was not produced by this
///    code path.
///
/// Failing 2, 3 or 4 returns `Ok(None)` only when there was no candidate at
/// all; a candidate that fails any check is an error, because the alternative
/// — creating a second bundle beside it — is the silent leak this exists to
/// stop. The refusal names the bundle so it can be dealt with by hand.
///
/// The unreferenced check is exactly as strong as the sharedness verdict that
/// brought us here: both read the same scan, and both are blind to projects
/// the token cannot see. This path is only reached on a `Shared` verdict,
/// which means the scan completed.
fn adopt_existing_clone(
    admin: &AdminClient,
    field: &crate::remote::youtrack::bundles::ProjectFieldBundle,
    missing: &[PendingValue],
    usage: &BundleUsage,
    clone_name: &str,
) -> Result<Option<BundleRef>, RemoteError> {
    let candidates: Vec<BundleRef> = admin
        .list_bundles(field.bundle.kind)?
        .into_iter()
        .filter(|b| b.name == clone_name)
        .collect();
    let candidate = match candidates.len() {
        0 => return Ok(None),
        1 => candidates.into_iter().next().unwrap_or_else(|| {
            unreachable!("a one-element vector always yields its element");
        }),
        n => {
            return Err(RemoteError::Config(format!(
                "{n} bundles are already named '{clone_name}'. That is the fingerprint of \
                 more than one interrupted `br remote init`, and choosing between them here \
                 would be a guess. Delete the unused ones in YouTrack's administration \
                 (Custom Fields → bundles), leaving at most one, and re-run."
            )));
        }
    };

    let refuse = |why: &str| {
        Err(RemoteError::Config(format!(
            "a bundle named '{clone_name}' already exists, but {why}, so `br remote init` \
             will not reuse it — and it will not create a second bundle with the same name \
             either, because YouTrack allows that and the extra copy would be orphaned with \
             nothing to point at it. Rename or delete '{clone_name}' in YouTrack's \
             administration and re-run."
        )))
    };

    if usage.users.contains_key(&candidate.id) {
        return refuse("something already uses it");
    }
    let has_every_source_value = field
        .bundle
        .values
        .iter()
        .all(|source| candidate.values.iter().any(|v| v.name == source.name));
    if !has_every_source_value {
        return refuse(&format!(
            "it does not carry every value of '{}', so it is not a copy of it",
            field.bundle.name
        ));
    }
    let carries_only_known_values = candidate.values.iter().all(|v| {
        field.bundle.values.iter().any(|s| s.name == v.name)
            || missing.iter().any(|p| p.name == v.name)
    });
    if !carries_only_known_values {
        return refuse("it carries values neither the source nor this run would have put there");
    }
    Ok(Some(candidate))
}

#[allow(clippy::too_many_arguments)]
fn align_defaults(
    admin: &AdminClient,
    cfg: &RemoteConfig,
    project: &ProjectRef,
    summaries: &[ProjectFieldSummary],
    bundles: &[(String, BundleRef)],
    opts: InitOptions,
    report: &mut InitReport,
) -> Result<(), RemoteError> {
    for (field_name, beads_key) in wanted_defaults() {
        let Some(wanted) = default_for(cfg, field_name, beads_key) else {
            continue;
        };
        let Some(summary) = summaries.iter().find(|s| s.field_name == field_name) else {
            continue;
        };
        if summary.default_values.len() == 1 && summary.default_values[0] == wanted {
            continue;
        }
        let Some((_, bundle)) = bundles.iter().find(|(name, _)| name == field_name) else {
            continue;
        };
        report.defaults_changed.push(DefaultChange {
            field_name: field_name.to_string(),
            old: summary.default_values.clone(),
            new: wanted.to_string(),
        });
        announce(&format!(
            "default for '{field_name}': {} → {wanted}",
            if summary.default_values.is_empty() {
                "(none)".to_string()
            } else {
                summary.default_values.join(", ")
            }
        ));
        if opts.dry_run {
            continue;
        }
        // The bundle read at step 6 predates any value this run added, so
        // re-read it: the wanted default may be one of the new values.
        let bundle = if bundle.values.iter().any(|v| v.name == wanted) {
            bundle.clone()
        } else {
            admin.read_bundle(bundle)?
        };
        admin.set_default_values(
            project,
            &summary.instance_id,
            project_field_type(bundle.kind),
            &bundle,
            &[wanted],
        )?;
    }
    Ok(())
}

/// The beads defaults, as `(YouTrack field, beads map key)`.
///
/// `task` / `open` / P2. Stock YouTrack is `Bug` / `Submitted` / `Normal`,
/// which reverse-maps to a deferred bug at P3.
fn wanted_defaults() -> [(&'static str, &'static str); 3] {
    [("Type", "task"), ("State", "open"), ("Priority", "2")]
}

fn default_for<'a>(cfg: &'a RemoteConfig, field_name: &str, key: &str) -> Option<&'a str> {
    let map = match field_name {
        "Type" => &cfg.type_map,
        "State" => &cfg.status_map,
        "Priority" => &cfg.priority_map,
        _ => return None,
    };
    map.get(key).map(String::as_str)
}

fn clone_name(project: &ProjectRef, bundle: &BundleRef) -> String {
    let prefix = format!("{}: ", project.name);
    if bundle.name.starts_with(&prefix) {
        format!("{} (br)", bundle.name)
    } else {
        format!("{prefix}{}", bundle.name)
    }
}

fn quoted_list(values: &[PendingValue]) -> String {
    values
        .iter()
        .map(|v| format!("'{}'", v.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Disclosures that must reach the operator *before* the write they describe,
/// so `init` cannot silently do something instance-wide.
///
/// Written directly rather than folded into `InitReport`, which the CLI prints
/// only after the whole run returns — by which point the write has already
/// happened and the warning has missed its moment. Everything said here is
/// also recorded in the report, so nothing is lost by not reading stderr.
///
/// stderr rather than stdout, because `br remote init --json` must leave one
/// parseable document on stdout and a progress line in the middle of it would
/// break every scripted caller.
fn announce(line: &str) {
    eprintln!("{line}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::config::RemoteConfig;
    use crate::remote::http::{HttpClient, RetryPolicy, Token};
    use std::collections::BTreeSet;
    use test_support::mock_http::MockServer;
    use test_support::youtrack_fixtures::COUNT_SETTLED;

    fn config() -> RemoteConfig {
        RemoteConfig::from_yaml_str(include_str!("../../../tests/fixtures/remote_em.yaml"))
            .expect("valid config")
    }

    fn vocab(types: &[&str], statuses: &[&str]) -> WorkspaceVocabulary {
        WorkspaceVocabulary {
            types: types
                .iter()
                .map(|s| (*s).to_string())
                .collect::<BTreeSet<_>>(),
            statuses: statuses
                .iter()
                .map(|s| (*s).to_string())
                .collect::<BTreeSet<_>>(),
        }
    }

    fn admin(server: &MockServer) -> AdminClient {
        AdminClient::new(HttpClient::new(
            &server.base_url(),
            Token::new("t"),
            RetryPolicy::none(),
        ))
    }

    #[test]
    fn a_workspace_using_only_mapped_values_passes_preflight() {
        let result = preflight_vocabulary(
            &config(),
            &vocab(&["task", "bug", "epic"], &["open", "closed"]),
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn an_unmapped_type_fails_preflight_naming_the_config_key() {
        let err = preflight_vocabulary(&config(), &vocab(&["task", "spike"], &["open"]))
            .expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("spike"), "must name the value: {msg}");
        assert!(
            msg.contains("type_map"),
            "must name the config key to add: {msg}"
        );
    }

    #[test]
    fn an_unmapped_status_fails_preflight() {
        let err = preflight_vocabulary(&config(), &vocab(&["task"], &["open", "on_hold"]))
            .expect_err("must fail");
        assert!(err.to_string().contains("on_hold"), "{err}");
    }

    #[test]
    fn tombstone_is_not_treated_as_an_unmapped_status() {
        // The tombstone rule owns it; it is deliberately absent from status_map.
        let result = preflight_vocabulary(&config(), &vocab(&["task"], &["open", "tombstone"]));
        assert!(
            result.is_ok(),
            "tombstone must not trip the pre-flight: {result:?}"
        );
    }

    /// Every route a fully-provisioned reference project answers.
    fn provisioned_server() -> MockServer {
        let server = MockServer::start();
        server.on(
            "GET",
            "/api/admin/projects?fields=id,name,shortName&$top=500",
            200,
            r#"[{"id":"0-1","name":"EasyMoney","shortName":"EM"}]"#,
        );
        server.on(
            "GET",
            "/api/admin/customFieldSettings/customFields?fields=id,name,fieldType(id)&$top=500",
            200,
            &serde_json::json!([
                {"id":"161-11","name":"Beads ID","fieldType":{"id":"string"}},
                {"id":"161-12","name":"Design","fieldType":{"id":"text"}},
                {"id":"161-13","name":"Acceptance Criteria","fieldType":{"id":"text"}},
                {"id":"161-14","name":"Notes","fieldType":{"id":"text"}},
                {"id":"161-15","name":"Close Reason","fieldType":{"id":"text"}},
            ])
            .to_string(),
        );
        server.on(
            "GET",
            "/api/admin/projects/0-1/customFields?fields=id,field(name),defaultValues(name)&$top=200",
            200,
            &serde_json::json!([
                {"id":"189-8","field":{"name":"Type"},"defaultValues":[{"name":"Task"}]},
                {"id":"189-9","field":{"name":"State"},"defaultValues":[{"name":"Open"}]},
                {"id":"189-10","field":{"name":"Priority"},"defaultValues":[{"name":"Major"}]},
                {"id":"189-30","field":{"name":"Beads ID"},"defaultValues":[]},
                {"id":"189-31","field":{"name":"Design"},"defaultValues":[]},
                {"id":"189-32","field":{"name":"Acceptance Criteria"},"defaultValues":[]},
                {"id":"189-33","field":{"name":"Notes"},"defaultValues":[]},
                {"id":"189-34","field":{"name":"Close Reason"},"defaultValues":[]},
            ])
            .to_string(),
        );
        server.on(
            "GET",
            "/api/admin/customFieldSettings/customFields?fields=id,name,instances(id,project(shortName),bundle(id,name))&$top=500",
            200,
            &serde_json::json!([
                {"id":"161-1","name":"Type","instances":[
                    {"id":"189-8","project":{"shortName":"EM"},"bundle":{"id":"163-1","name":"Types"}}]},
                {"id":"161-2","name":"State","instances":[
                    {"id":"189-9","project":{"shortName":"EM"},"bundle":{"id":"165-0","name":"States"}}]},
                {"id":"161-3","name":"Priority","instances":[
                    {"id":"189-10","project":{"shortName":"EM"},"bundle":{"id":"167-0","name":"Priorities"}}]}
            ])
            .to_string(),
        );
        server
    }

    fn bundle_fields_route(server: &MockServer, body: &str) {
        server.on(
            "GET",
            "/api/admin/projects/0-1/customFields?fields=id,$type,canBeEmpty,field(id,name),bundle(id,name,$type,values(id,name,ordinal,color(id,background,foreground),isResolved))&$top=100",
            200,
            body,
        );
    }

    fn complete_bundles() -> String {
        let enum_values = |names: &[&str]| {
            names
                .iter()
                .enumerate()
                .map(
                    |(i, n)| serde_json::json!({"id": format!("164-{i}"), "name": n, "ordinal": i}),
                )
                .collect::<Vec<_>>()
        };
        let state_values = |names: &[(&str, bool)]| {
            names
                .iter()
                .enumerate()
                .map(|(i, (n, r))| {
                    serde_json::json!({"id": format!("166-{i}"), "name": n, "ordinal": i, "isResolved": r})
                })
                .collect::<Vec<_>>()
        };
        serde_json::json!([
            {"id":"189-8","field":{"id":"161-1","name":"Type"},
             "bundle":{"id":"163-1","name":"Types","$type":"EnumBundle",
                "values": enum_values(&["Task","Bug","Feature","Epic","Chore","Docs","Question"])}},
            {"id":"189-9","field":{"id":"161-2","name":"State"},
             "bundle":{"id":"165-0","name":"States","$type":"StateBundle",
                "values": state_values(&[("Open",false),("In Progress",false),("Blocked",false),
                    ("Deferred",false),("Draft",false),("Pinned",false),("Fixed",true),
                    ("Won't fix",true)])}},
            {"id":"189-10","field":{"id":"161-3","name":"Priority"},
             "bundle":{"id":"167-0","name":"Priorities","$type":"EnumBundle",
                "values": enum_values(&["Show-stopper","Critical","Major","Normal","Minor"])}}
        ])
        .to_string()
    }

    #[test]
    fn a_fully_provisioned_project_issues_no_write_at_all() {
        let server = provisioned_server();
        bundle_fields_route(&server, &complete_bundles());

        let report = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions::default(),
        )
        .expect("init");

        assert!(
            server.write_requests().is_empty(),
            "a reconciled project must issue zero writes, got {:?}",
            server.write_requests()
        );
        assert!(report.prototypes_created.is_empty());
        assert!(report.fields_attached.is_empty());
        assert!(report.values_added.is_empty());
        assert!(report.defaults_changed.is_empty());
        assert_eq!(report.projects_seen, ["EM"]);
    }

    #[test]
    fn the_reference_project_needs_exactly_seven_value_posts() {
        let server = provisioned_server();
        // Stock Types and States, private (one user each in the scan above).
        bundle_fields_route(
            &server,
            &serde_json::json!([
                {"id":"189-8","field":{"id":"161-1","name":"Type"},
                 "bundle":{"id":"163-1","name":"Types","$type":"EnumBundle","values":[
                    {"id":"164-0","name":"Bug","ordinal":0},
                    {"id":"164-1","name":"Cosmetics","ordinal":1},
                    {"id":"164-2","name":"Exception","ordinal":2},
                    {"id":"164-3","name":"Feature","ordinal":3},
                    {"id":"164-4","name":"Task","ordinal":4},
                    {"id":"164-5","name":"Usability Problem","ordinal":5},
                    {"id":"164-6","name":"Performance Problem","ordinal":6},
                    {"id":"164-7","name":"Epic","ordinal":7}]}},
                {"id":"189-9","field":{"id":"161-2","name":"State"},
                 "bundle":{"id":"165-0","name":"States","$type":"StateBundle","values":[
                    {"id":"166-0","name":"Submitted","ordinal":0,"isResolved":false},
                    {"id":"166-1","name":"Open","ordinal":1,"isResolved":false},
                    {"id":"166-2","name":"In Progress","ordinal":2,"isResolved":false},
                    {"id":"166-3","name":"Fixed","ordinal":3,"isResolved":true},
                    {"id":"166-4","name":"Won't fix","ordinal":4,"isResolved":true}]}},
                {"id":"189-10","field":{"id":"161-3","name":"Priority"},
                 "bundle":{"id":"167-0","name":"Priorities","$type":"EnumBundle","values":[
                    {"id":"168-0","name":"Show-stopper","ordinal":0},
                    {"id":"168-1","name":"Critical","ordinal":1},
                    {"id":"168-2","name":"Major","ordinal":2},
                    {"id":"168-3","name":"Normal","ordinal":3},
                    {"id":"168-4","name":"Minor","ordinal":4}]}}
            ])
            .to_string(),
        );
        server.on(
            "POST",
            "/api/admin/customFieldSettings/bundles/enum/163-1/values?fields=id,name",
            200,
            r#"{"id":"164-20","name":"Chore"}"#,
        );
        server.on(
            "POST",
            "/api/admin/customFieldSettings/bundles/state/165-0/values?fields=id,name",
            200,
            r#"{"id":"166-20","name":"Blocked"}"#,
        );

        let report = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions::default(),
        )
        .expect("init");

        let posts = server.write_requests();
        assert_eq!(
            posts.len(),
            7,
            "three types and four states, nothing else: {posts:?}"
        );
        let added: Vec<&str> = report
            .values_added
            .iter()
            .flat_map(|(_, values)| values.iter().map(String::as_str))
            .collect();
        assert_eq!(
            added,
            [
                "Chore", "Docs", "Question", "Blocked", "Deferred", "Draft", "Pinned"
            ]
        );
        assert!(
            report.defaults_changed.is_empty(),
            "the defaults already match: {:?}",
            report.defaults_changed
        );
    }

    #[test]
    fn a_shared_bundle_on_a_populated_project_refuses_with_both_ways_forward() {
        let server = provisioned_server();
        // Two projects use the Types bundle.
        server.on(
            "GET",
            "/api/admin/customFieldSettings/customFields?fields=id,name,instances(id,project(shortName),bundle(id,name))&$top=500",
            200,
            &serde_json::json!([
                {"id":"161-1","name":"Type","instances":[
                    {"id":"189-8","project":{"shortName":"EM"},"bundle":{"id":"163-1","name":"Types"}},
                    {"id":"189-80","project":{"shortName":"OTHER"},"bundle":{"id":"163-1","name":"Types"}}]}
            ])
            .to_string(),
        );
        bundle_fields_route(
            &server,
            &serde_json::json!([
                {"id":"189-8","field":{"id":"161-1","name":"Type"},
                 "bundle":{"id":"163-1","name":"Types","$type":"EnumBundle","values":[
                    {"id":"164-0","name":"Task","ordinal":0}]}}
            ])
            .to_string(),
        );
        server.on(
            "POST",
            "/api/issuesGetter/count?fields=count",
            200,
            r#"{"count":17,"$type":"IssueCountResponse"}"#,
        );

        let err = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions::default(),
        )
        .expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("OTHER"), "must name the other user: {msg}");
        assert!(
            msg.contains("17"),
            "must say how many issues block it: {msg}"
        );
        assert!(
            msg.contains("--allow-shared-bundle"),
            "must name the flag that overrides it: {msg}"
        );
        assert!(
            server
                .write_requests()
                .iter()
                .all(|r| r.path.starts_with("/api/issuesGetter/count")),
            "the only write-method request may be the count probe: {:?}",
            server.write_requests()
        );
    }

    const BUNDLE_LIST_PATH: &str =
        "/api/admin/customFieldSettings/bundles/enum?fields=id,name,values(id,name)&$top=500";

    /// A server whose `Types` bundle is shared with `OTHER`, on an empty
    /// project, with every route the clone path walks.
    fn shared_but_empty_server() -> MockServer {
        let server = provisioned_server();
        server.on(
            "GET",
            "/api/admin/customFieldSettings/customFields?fields=id,name,instances(id,project(shortName),bundle(id,name))&$top=500",
            200,
            &serde_json::json!([
                {"id":"161-1","name":"Type","instances":[
                    {"id":"189-8","project":{"shortName":"EM"},"bundle":{"id":"163-1","name":"Types"}},
                    {"id":"189-80","project":{"shortName":"OTHER"},"bundle":{"id":"163-1","name":"Types"}}]}
            ])
            .to_string(),
        );
        bundle_fields_route(
            &server,
            &serde_json::json!([
                {"id":"189-8","field":{"id":"161-1","name":"Type"},
                 "bundle":{"id":"163-1","name":"Types","$type":"EnumBundle","values":[
                    {"id":"164-0","name":"Task","ordinal":0},
                    {"id":"164-1","name":"Bug","ordinal":1},
                    {"id":"164-2","name":"Feature","ordinal":2},
                    {"id":"164-3","name":"Epic","ordinal":3},
                    {"id":"164-4","name":"Chore","ordinal":4},
                    {"id":"164-5","name":"Docs","ordinal":5}]}}
            ])
            .to_string(),
        );
        server.on(
            "POST",
            "/api/issuesGetter/count?fields=count",
            200,
            COUNT_SETTLED,
        );
        // No bundle is yet named "EasyMoney: Types", so nothing to adopt.
        server.on("GET", BUNDLE_LIST_PATH, 200, "[]");
        server.on(
            "POST",
            "/api/admin/customFieldSettings/bundles/enum?fields=id,name",
            200,
            r#"{"id":"163-9","name":"EasyMoney: Types"}"#,
        );
        server.on(
            "POST",
            "/api/admin/projects/0-1/customFields/189-8?fields=id,bundle(id,name)",
            200,
            r#"{"id":"189-8"}"#,
        );
        server.on(
            "GET",
            "/api/admin/customFieldSettings/bundles/enum/163-9?fields=id,name,values(id,name,ordinal,color(id,background,foreground),isResolved)",
            200,
            &serde_json::json!({"id":"163-9","name":"EasyMoney: Types","values":[
                {"id":"170-0","name":"Task","ordinal":0},
                {"id":"170-1","name":"Bug","ordinal":1},
                {"id":"170-2","name":"Feature","ordinal":2},
                {"id":"170-3","name":"Epic","ordinal":3},
                {"id":"170-4","name":"Chore","ordinal":4},
                {"id":"170-5","name":"Docs","ordinal":5},
                {"id":"170-6","name":"Question","ordinal":6}]})
            .to_string(),
        );
        server.on(
            "POST",
            "/api/admin/customFieldSettings/bundles/enum/163-9/values?fields=id,name",
            200,
            r#"{"id":"170-6","name":"Question"}"#,
        );
        server.on(
            "POST",
            "/api/admin/projects/0-1/customFields/189-8?fields=field(name),defaultValues(id,name)",
            200,
            r#"{"defaultValues":[]}"#,
        );
        server
    }

    #[test]
    fn a_shared_bundle_on_an_empty_project_is_cloned_re_pointed_and_filled() {
        let server = shared_but_empty_server();

        let report = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions::default(),
        )
        .expect("init");

        assert_eq!(
            report.clones,
            [BundleClone {
                field_name: "Type".into(),
                source: "Types".into(),
                clone: "EasyMoney: Types".into(),
            }],
            "a shared bundle is copied, never mutated"
        );
        let writes = server.write_requests();
        let paths: Vec<&str> = writes
            .iter()
            .map(|r| {
                // Trim the query string; the order of calls is what matters here.
                let p = r.path.as_str();
                p.split_once('?').map_or(p, |(head, _)| head)
            })
            .collect();
        assert_eq!(
            paths,
            [
                "/api/issuesGetter/count",
                "/api/admin/customFieldSettings/bundles/enum",
                "/api/admin/projects/0-1/customFields/189-8",
                "/api/admin/customFieldSettings/bundles/enum/163-9/values",
                "/api/admin/projects/0-1/customFields/189-8",
            ],
            "count, clone, re-point, fill, restore the defaults — in that order"
        );
    }

    /// The orphan an interrupted run leaves behind: a bundle carrying the
    /// clone name, holding every source value, referenced by nothing.
    fn orphan_clone(extra: &[&str]) -> String {
        let mut values = vec![
            serde_json::json!({"id":"170-0","name":"Task"}),
            serde_json::json!({"id":"170-1","name":"Bug"}),
            serde_json::json!({"id":"170-2","name":"Feature"}),
            serde_json::json!({"id":"170-3","name":"Epic"}),
            serde_json::json!({"id":"170-4","name":"Chore"}),
            serde_json::json!({"id":"170-5","name":"Docs"}),
        ];
        for (i, name) in extra.iter().enumerate() {
            values.push(serde_json::json!({"id": format!("170-{}", 10 + i), "name": name}));
        }
        serde_json::json!([{"id":"163-9","name":"EasyMoney: Types","values": values}]).to_string()
    }

    #[test]
    fn a_rerun_after_an_interrupted_clone_adopts_it_instead_of_leaking_a_second() {
        // The first run died between creating the bundle and re-pointing the
        // project at it: the clone exists, nothing references it, and the
        // project's field still points at the shared source.
        let server = shared_but_empty_server();
        server.on("GET", BUNDLE_LIST_PATH, 200, &orphan_clone(&[]));

        let report = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions::default(),
        )
        .expect("init");

        let writes = server.write_requests();
        assert!(
            !writes
                .iter()
                .any(|r| r.path == "/api/admin/customFieldSettings/bundles/enum?fields=id,name"),
            "the orphan must be adopted, not duplicated: {writes:?}"
        );
        assert_eq!(
            report.clones,
            [BundleClone {
                field_name: "Type".into(),
                source: "Types".into(),
                clone: "EasyMoney: Types".into(),
            }]
        );
        assert!(
            report.notes.iter().any(|n| n.contains("adopted")),
            "the operator must be told which bundle was reused: {:?}",
            report.notes
        );
        // Still filled and re-pointed, just without a second create.
        assert_eq!(
            report.values_added,
            [("Type".to_string(), vec!["Question".to_string()])]
        );
    }

    #[test]
    fn a_rerun_after_an_interruption_partway_through_the_fill_adds_only_the_remainder() {
        // The first run got as far as adding `Question` before dying.
        let server = shared_but_empty_server();
        server.on("GET", BUNDLE_LIST_PATH, 200, &orphan_clone(&["Question"]));

        let report = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions::default(),
        )
        .expect("init");

        let writes = server.write_requests();
        assert!(
            !writes
                .iter()
                .any(|r| r.path.contains("/bundles/enum/163-9/values")),
            "a value already present must never be re-added: {writes:?}"
        );
        assert_eq!(
            report.values_added,
            [("Type".to_string(), Vec::<String>::new())]
        );
    }

    #[test]
    fn a_same_named_bundle_that_is_already_in_use_refuses_rather_than_duplicating() {
        let server = shared_but_empty_server();
        server.on("GET", BUNDLE_LIST_PATH, 200, &orphan_clone(&[]));
        // The scan says a field of OTHER already points at 163-9.
        server.on(
            "GET",
            "/api/admin/customFieldSettings/customFields?fields=id,name,instances(id,project(shortName),bundle(id,name))&$top=500",
            200,
            &serde_json::json!([
                {"id":"161-1","name":"Type","instances":[
                    {"id":"189-8","project":{"shortName":"EM"},"bundle":{"id":"163-1","name":"Types"}},
                    {"id":"189-80","project":{"shortName":"OTHER"},"bundle":{"id":"163-1","name":"Types"}},
                    {"id":"189-81","project":{"shortName":"OTHER"},"bundle":{"id":"163-9","name":"EasyMoney: Types"}}]}
            ])
            .to_string(),
        );

        let err = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions::default(),
        )
        .expect_err("must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("EasyMoney: Types"),
            "must name the bundle: {msg}"
        );
        assert!(
            msg.contains("already uses it"),
            "must say why it was not adopted: {msg}"
        );
        assert!(
            !server
                .write_requests()
                .iter()
                .any(|r| r.path == "/api/admin/customFieldSettings/bundles/enum?fields=id,name"),
            "and must not create a second one anyway"
        );
    }

    #[test]
    fn a_same_named_bundle_holding_foreign_values_refuses() {
        let server = shared_but_empty_server();
        server.on("GET", BUNDLE_LIST_PATH, 200, &orphan_clone(&["Spike"]));

        let err = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions::default(),
        )
        .expect_err("must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("values neither the source nor this run"),
            "a bundle someone else built is not ours to take over: {msg}"
        );
    }

    #[test]
    fn two_bundles_with_the_clone_name_refuse_rather_than_guessing() {
        let server = shared_but_empty_server();
        server.on(
            "GET",
            BUNDLE_LIST_PATH,
            200,
            &serde_json::json!([
                {"id":"163-9","name":"EasyMoney: Types","values":[]},
                {"id":"163-10","name":"EasyMoney: Types","values":[]}
            ])
            .to_string(),
        );

        let err = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions::default(),
        )
        .expect_err("must refuse");
        assert!(err.to_string().contains("2 bundles"), "{err}");
    }

    #[test]
    fn an_unavailable_scan_refuses_rather_than_writing() {
        let server = provisioned_server();
        server.on(
            "GET",
            "/api/admin/customFieldSettings/customFields?fields=id,name,instances(id,project(shortName),bundle(id,name))&$top=500",
            403,
            r#"{"error":"Forbidden"}"#,
        );
        bundle_fields_route(
            &server,
            &serde_json::json!([
                {"id":"189-8","field":{"id":"161-1","name":"Type"},
                 "bundle":{"id":"163-1","name":"Types","$type":"EnumBundle","values":[
                    {"id":"164-0","name":"Task","ordinal":0}]}}
            ])
            .to_string(),
        );

        let err = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions::default(),
        )
        .expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("--allow-shared-bundle"), "{msg}");
        assert!(
            msg.contains("Grant the token read access"),
            "a scan the token could not complete IS fixed by widening it: {msg}"
        );
        assert!(
            server.write_requests().is_empty(),
            "{:?}",
            server.write_requests()
        );
    }

    #[test]
    fn a_bundle_absent_from_a_successful_scan_refuses_with_a_different_remedy() {
        // The scan completed and simply did not mention 163-1. Advising the
        // operator to grant more read access would be a dead end here: every
        // project the token can see was already read.
        let server = provisioned_server();
        server.on(
            "GET",
            "/api/admin/customFieldSettings/customFields?fields=id,name,instances(id,project(shortName),bundle(id,name))&$top=500",
            200,
            &serde_json::json!([
                {"id":"161-2","name":"State","instances":[
                    {"id":"189-9","project":{"shortName":"EM"},"bundle":{"id":"165-0","name":"States"}}]}
            ])
            .to_string(),
        );
        bundle_fields_route(
            &server,
            &serde_json::json!([
                {"id":"189-8","field":{"id":"161-1","name":"Type"},
                 "bundle":{"id":"163-1","name":"Types","$type":"EnumBundle","values":[
                    {"id":"164-0","name":"Task","ordinal":0}]}}
            ])
            .to_string(),
        );

        let err = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions::default(),
        )
        .expect_err("must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("The scan completed"),
            "must say the scan succeeded: {msg}"
        );
        assert!(
            !msg.contains("Grant the token read access"),
            "must not advise a remedy that cannot apply here: {msg}"
        );
        assert!(
            msg.contains("Widening the token's access will not change this"),
            "must say plainly why the other remedy is not it: {msg}"
        );
        assert!(msg.contains("--allow-shared-bundle"), "{msg}");
        assert!(
            server.write_requests().is_empty(),
            "{:?}",
            server.write_requests()
        );
    }

    #[test]
    fn allowing_a_shared_bundle_announces_before_it_writes() {
        let server = shared_but_empty_server();
        server.on(
            "POST",
            "/api/admin/customFieldSettings/bundles/enum/163-1/values?fields=id,name",
            200,
            r#"{"id":"164-20","name":"Question"}"#,
        );

        let report = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions {
                allow_shared_bundle: true,
                ..InitOptions::default()
            },
        )
        .expect("init");

        let note = report
            .notes
            .iter()
            .find(|n| n.contains("--allow-shared-bundle"))
            .expect("the override must be disclosed");
        assert!(note.contains("OTHER"), "must name who else sees it: {note}");
        assert!(
            note.contains("'Question'"),
            "must name what they will see: {note}"
        );
        // The values went into the shared bundle in place, not into a clone.
        assert!(report.clones.is_empty(), "{:?}", report.clones);
        assert!(
            server
                .write_requests()
                .iter()
                .any(|r| r.path.contains("/bundles/enum/163-1/values")),
            "{:?}",
            server.write_requests()
        );
    }

    #[test]
    fn the_defaults_are_realigned_from_the_stock_ones() {
        let server = provisioned_server();
        server.on(
            "GET",
            "/api/admin/projects/0-1/customFields?fields=id,field(name),defaultValues(name)&$top=200",
            200,
            &serde_json::json!([
                {"id":"189-8","field":{"name":"Type"},"defaultValues":[{"name":"Bug"}]},
                {"id":"189-9","field":{"name":"State"},"defaultValues":[{"name":"Submitted"}]},
                {"id":"189-10","field":{"name":"Priority"},"defaultValues":[{"name":"Normal"}]},
                {"id":"189-30","field":{"name":"Beads ID"},"defaultValues":[]},
                {"id":"189-31","field":{"name":"Design"},"defaultValues":[]},
                {"id":"189-32","field":{"name":"Acceptance Criteria"},"defaultValues":[]},
                {"id":"189-33","field":{"name":"Notes"},"defaultValues":[]},
                {"id":"189-34","field":{"name":"Close Reason"},"defaultValues":[]},
            ])
            .to_string(),
        );
        bundle_fields_route(&server, &complete_bundles());
        for instance in ["189-8", "189-9", "189-10"] {
            server.on(
                "POST",
                &format!(
                    "/api/admin/projects/0-1/customFields/{instance}?fields=field(name),defaultValues(id,name)"
                ),
                200,
                r#"{"defaultValues":[]}"#,
            );
        }

        let report = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions::default(),
        )
        .expect("init");

        let changes: Vec<(&str, &str, &str)> = report
            .defaults_changed
            .iter()
            .map(|c| (c.field_name.as_str(), c.old[0].as_str(), c.new.as_str()))
            .collect();
        assert_eq!(
            changes,
            [
                ("Type", "Bug", "Task"),
                ("State", "Submitted", "Open"),
                ("Priority", "Normal", "Major"),
            ],
            "stock YouTrack adopts a deferred bug at P3; beads adopts an open task at P2"
        );
        assert_eq!(server.write_requests().len(), 3, "one POST per field");
        for request in server.write_requests() {
            let body: serde_json::Value = serde_json::from_str(&request.body).expect("json body");
            let defaults = body["defaultValues"].as_array().expect("array");
            assert!(
                defaults[0].get("name").is_none() && defaults[0].get("id").is_some(),
                "defaults must go out by element id, never by name: {}",
                request.body
            );
        }
    }

    #[test]
    fn keep_project_defaults_skips_the_alignment_entirely() {
        let server = provisioned_server();
        server.on(
            "GET",
            "/api/admin/projects/0-1/customFields?fields=id,field(name),defaultValues(name)&$top=200",
            200,
            &serde_json::json!([
                {"id":"189-8","field":{"name":"Type"},"defaultValues":[{"name":"Bug"}]},
                {"id":"189-9","field":{"name":"State"},"defaultValues":[{"name":"Submitted"}]},
                {"id":"189-10","field":{"name":"Priority"},"defaultValues":[{"name":"Normal"}]},
                {"id":"189-30","field":{"name":"Beads ID"},"defaultValues":[]},
                {"id":"189-31","field":{"name":"Design"},"defaultValues":[]},
                {"id":"189-32","field":{"name":"Acceptance Criteria"},"defaultValues":[]},
                {"id":"189-33","field":{"name":"Notes"},"defaultValues":[]},
                {"id":"189-34","field":{"name":"Close Reason"},"defaultValues":[]},
            ])
            .to_string(),
        );
        bundle_fields_route(&server, &complete_bundles());

        let report = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions {
                keep_project_defaults: true,
                ..InitOptions::default()
            },
        )
        .expect("init");

        assert!(report.defaults_changed.is_empty());
        assert!(
            server.write_requests().is_empty(),
            "{:?}",
            server.write_requests()
        );
    }

    #[test]
    fn a_dry_run_writes_nothing_but_still_reports_the_plan() {
        let server = provisioned_server();
        server.on(
            "GET",
            "/api/admin/customFieldSettings/customFields?fields=id,name,fieldType(id)&$top=500",
            200,
            "[]",
        );
        server.on(
            "GET",
            "/api/admin/projects/0-1/customFields?fields=id,field(name),defaultValues(name)&$top=200",
            200,
            &serde_json::json!([
                {"id":"189-8","field":{"name":"Type"},"defaultValues":[{"name":"Bug"}]},
                {"id":"189-9","field":{"name":"State"},"defaultValues":[{"name":"Open"}]},
                {"id":"189-10","field":{"name":"Priority"},"defaultValues":[{"name":"Major"}]},
            ])
            .to_string(),
        );
        bundle_fields_route(&server, &complete_bundles());

        let report = run(
            &config(),
            &admin(&server),
            &vocab(&["task"], &["open"]),
            InitOptions {
                dry_run: true,
                ..InitOptions::default()
            },
        )
        .expect("init");

        assert!(
            server.write_requests().is_empty(),
            "{:?}",
            server.write_requests()
        );
        assert_eq!(report.prototypes_created.len(), 5);
        assert_eq!(report.fields_attached.len(), 5);
        assert_eq!(
            report.defaults_changed.len(),
            1,
            "only Type differs from the beads default"
        );
    }
}
