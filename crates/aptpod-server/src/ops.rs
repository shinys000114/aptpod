// SPDX-License-Identifier: GPL-3.0-only

//! API for the repository operations beyond add: rm, promote,
//! prune-versions, rollback, layout edits and delete (planned, previewed and
//! applied like add), and history, snapshots, diff, find, gc and repair.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use aptpod_repo::history::{DiffEntry, GcReport, RevisionInfo, SnapshotInfo};
use aptpod_repo::ops::{Promote, RevRef};
use aptpod_repo::{Applied, Error as RepoError, Filter, Layout, Plan, Repo, Row};
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::{
    ApiError, AppState, Bases, Caller, Commit, Outcome, Res, St, csv, plan_job, read,
};
use crate::authdb::Role;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/packages", get(find))
        .route("/api/v1/publishes/{alias}/rm", post(rm))
        .route("/api/v1/publishes/{alias}/promote", post(promote))
        .route("/api/v1/publishes/{alias}/prune", post(prune))
        .route("/api/v1/publishes/{alias}/rollback", post(rollback))
        .route("/api/v1/publishes/{alias}/layout", post(set_layout))
        .route("/api/v1/publishes/{alias}/delete", post(delete_publish))
        .route("/api/v1/publishes/{alias}/history", get(history))
        .route("/api/v1/publishes/{alias}/diff", get(diff))
        .route(
            "/api/v1/publishes/{alias}/snapshots",
            get(snapshots).post(snapshot_create),
        )
        .route(
            "/api/v1/publishes/{alias}/snapshots/prune",
            post(snapshot_prune),
        )
        .route(
            "/api/v1/publishes/{alias}/snapshots/{label}",
            delete(snapshot_drop),
        )
        .route("/api/v1/config", get(config_export).post(config_import))
        .route("/api/v1/config/file", get(file_status))
        .route("/api/v1/config/file/plan", post(file_plan))
        .route("/api/v1/config/file/apply", post(file_apply))
        .route("/api/v1/gc", post(gc))
        .route("/api/v1/repair", post(repair))
}

/// Runs a change that is not a plan (labels, gc, repair) on the writer and
/// records it in the job log.
async fn write_job<T, F>(
    st: &Arc<AppState>,
    caller: &Caller,
    kind: &'static str,
    resource: String,
    f: F,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&mut Repo) -> Result<(T, String), RepoError> + Send + 'static,
{
    let st2 = st.clone();
    let user = caller.user.name.clone();
    st.writer
        .run(move |repo| -> Result<T, ApiError> {
            let job = st2.job_start(kind, &resource, &user)?;
            let job_id = job.id();
            let res = f(repo);
            match &res {
                Ok((_, summary)) => job.finish(Ok(summary))?,
                Err(e) => job.finish(Err(&e.to_string()))?,
            }
            tracing::info!(%user, kind, %resource, job = job_id, ok = res.is_ok(), "job");
            Ok(res?.0)
        })
        .await?
}

fn filter(
    packages: &[String],
    versions: Option<&str>,
    arch: Option<&str>,
) -> Result<Filter, ApiError> {
    let specs: Vec<&str> = packages.iter().map(String::as_str).collect();
    Ok(Filter::new(&specs, versions, arch)?)
}

/// Refuses an operation that would select every package by accident.
fn need_selection(packages: &[String], all: bool, what: &str) -> Result<(), ApiError> {
    if packages.is_empty() && !all {
        return Err(ApiError::invalid(format!(
            "{what}: name packages (patterns) or select all of them explicitly (--all)"
        )));
    }
    Ok(())
}

fn commit(apply: bool, expect: Bases) -> Commit {
    Commit { apply, expect }
}

// ---- planned changes -----------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RmReq {
    pub(crate) component: String,
    #[serde(default)]
    pub(crate) packages: Vec<String>,
    #[serde(default)]
    pub(crate) all: bool,
    pub(crate) versions: Option<String>,
    pub(crate) arch: Option<String>,
    pub(crate) dists: Option<Vec<String>>,
    /// Only remove versions beyond the newest `keep` per name and arch.
    pub(crate) keep: Option<u32>,
    #[serde(default)]
    pub(crate) apply: bool,
    #[serde(default)]
    pub(crate) expect: Bases,
}

pub(crate) async fn rm(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
    Json(req): Json<RmReq>,
) -> Res<Outcome> {
    caller.need(Role::Publisher)?;
    crate::mirror_api::refuse_mirrored(&st, &alias)?;
    if req.keep.is_none() && req.versions.is_none() {
        need_selection(&req.packages, req.all, "rm")?;
    }
    let f = filter(&req.packages, req.versions.as_deref(), req.arch.as_deref())?;
    let resource = format!("publish:{alias}");
    plan_job(
        &st,
        &caller,
        "rm",
        resource,
        commit(req.apply, req.expect),
        vec![],
        move |r| r.plan_rm(&alias, &req.component, &f, req.dists.as_deref(), req.keep),
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromoteReq {
    pub(crate) component: String,
    #[serde(default)]
    pub(crate) packages: Vec<String>,
    #[serde(default)]
    pub(crate) all: bool,
    pub(crate) versions: Option<String>,
    pub(crate) arch: Option<String>,
    /// Source dists (default: all of the source publish).
    pub(crate) dists: Option<Vec<String>>,
    /// Target publish (default: the same one).
    pub(crate) to_publish: Option<String>,
    /// Target component (default: the same one).
    pub(crate) to_component: Option<String>,
    /// Single target dist (with a single source dist).
    pub(crate) to_dist: Option<String>,
    /// Every matching version instead of the newest.
    #[serde(default)]
    pub(crate) all_versions: bool,
    /// Remove from the source.
    #[serde(default, rename = "move")]
    pub(crate) move_: bool,
    #[serde(default)]
    pub(crate) apply: bool,
    #[serde(default)]
    pub(crate) expect: Bases,
}

pub(crate) async fn promote(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
    Json(req): Json<PromoteReq>,
) -> Res<Outcome> {
    caller.need(Role::Publisher)?;
    need_selection(&req.packages, req.all, "promote")?;
    let f = filter(&req.packages, req.versions.as_deref(), req.arch.as_deref())?;
    let to_publish = req.to_publish.unwrap_or_else(|| alias.clone());
    crate::mirror_api::refuse_mirrored(&st, &to_publish)?;
    if req.move_ {
        crate::mirror_api::refuse_mirrored(&st, &alias)?;
    }
    let to_component = req.to_component.unwrap_or_else(|| req.component.clone());
    let resource = if to_publish == alias {
        format!("publish:{alias}")
    } else {
        format!("publish:{alias},publish:{to_publish}")
    };
    let opt = Promote {
        dists: req.dists,
        to_dist: req.to_dist,
        all_versions: req.all_versions,
        move_: req.move_,
    };
    let component = req.component;
    plan_job(
        &st,
        &caller,
        "promote",
        resource,
        commit(req.apply, req.expect),
        vec![],
        move |r| r.plan_promote((&alias, &component), (&to_publish, &to_component), &f, &opt),
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PruneReq {
    component: Option<String>,
    dists: Option<Vec<String>>,
    /// Default: the publish's `keep`.
    keep: Option<u32>,
    #[serde(default)]
    packages: Vec<String>,
    versions: Option<String>,
    arch: Option<String>,
    #[serde(default)]
    apply: bool,
    #[serde(default)]
    expect: Bases,
}

async fn prune(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
    Json(req): Json<PruneReq>,
) -> Res<Outcome> {
    caller.need(Role::Publisher)?;
    crate::mirror_api::refuse_mirrored(&st, &alias)?;
    let f = filter(&req.packages, req.versions.as_deref(), req.arch.as_deref())?;
    let resource = format!("publish:{alias}");
    plan_job(
        &st,
        &caller,
        "prune",
        resource,
        commit(req.apply, req.expect),
        vec![],
        move |r| {
            r.plan_prune_versions(
                &alias,
                req.component.as_deref(),
                req.dists.as_deref(),
                req.keep,
                &f,
            )
        },
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RollbackReq {
    /// `#<revision>` or a snapshot label.
    pub(crate) to: String,
    #[serde(default)]
    pub(crate) apply: bool,
    #[serde(default)]
    pub(crate) expect: Bases,
}

pub(crate) async fn rollback(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
    Json(req): Json<RollbackReq>,
) -> Res<Outcome> {
    caller.need(Role::Publisher)?;
    let to = RevRef::parse(&req.to);
    let resource = format!("publish:{alias}");
    plan_job(
        &st,
        &caller,
        "rollback",
        resource,
        commit(req.apply, req.expect),
        vec![],
        move |r| r.plan_rollback(&alias, &to),
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LayoutReq {
    pub(crate) layout: Layout,
    /// Allow dropping cells or architectures that hold packages.
    #[serde(default)]
    pub(crate) force: bool,
    /// Take over a new path that already holds a repository tree.
    #[serde(default)]
    pub(crate) adopt: bool,
    #[serde(default)]
    pub(crate) apply: bool,
    #[serde(default)]
    pub(crate) expect: Bases,
}

pub(crate) async fn set_layout(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
    Json(req): Json<LayoutReq>,
) -> Res<Outcome> {
    caller.need(Role::Admin)?;
    crate::configfile::refuse(&st, "publish", &alias)?;
    crate::mirror_api::refuse_mirrored(&st, &alias)?;
    st.cfg
        .allows_publish_path(&req.layout.path)
        .map_err(ApiError::invalid)?;
    crate::mirror_api::refuse_mirror_overlap(&st, &req.layout.path, None)?;
    let resource = format!("publish:{alias}");
    plan_job(
        &st,
        &caller,
        "layout",
        resource,
        commit(req.apply, req.expect),
        vec![],
        move |r| r.plan_layout(&alias, req.layout, req.force, req.adopt),
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteReq {
    /// Delete although packages are left.
    #[serde(default)]
    force: bool,
    #[serde(default)]
    apply: bool,
    #[serde(default)]
    expect: Bases,
}

async fn delete_publish(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
    Json(req): Json<DeleteReq>,
) -> Res<Outcome> {
    caller.need(Role::Admin)?;
    crate::configfile::refuse(&st, "publish", &alias)?;
    crate::mirror_api::refuse_mirrored(&st, &alias)?;
    let resource = format!("publish:{alias}");
    plan_job(
        &st,
        &caller,
        "delete",
        resource,
        commit(req.apply, req.expect),
        vec![],
        move |r| r.plan_delete(&alias, req.force),
    )
    .await
}

// ---- history and snapshots ----------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryQuery {
    pub(crate) limit: Option<usize>,
}

pub(crate) async fn history(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> Res<Vec<RevisionInfo>> {
    caller.need(Role::Reader)?;
    let limit = q.limit.unwrap_or(50).min(10_000);
    Ok(Json(read(&st, move |r| r.history(&alias, limit)).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffQuery {
    pub(crate) from: String,
    /// Default: HEAD.
    pub(crate) to: Option<String>,
}

pub(crate) async fn diff(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
    Query(q): Query<DiffQuery>,
) -> Res<Vec<DiffEntry>> {
    caller.need(Role::Reader)?;
    let from = RevRef::parse(&q.from);
    let to = q.to.as_deref().map_or(RevRef::Head, RevRef::parse);
    Ok(Json(read(&st, move |r| r.diff(&alias, &from, &to)).await?))
}

pub(crate) async fn snapshots(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
) -> Res<Vec<SnapshotInfo>> {
    caller.need(Role::Reader)?;
    Ok(Json(read(&st, move |r| r.snapshots(&alias)).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotReq {
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) description: String,
    /// `#<revision>`, a label or HEAD (default).
    pub(crate) at: Option<String>,
}

pub(crate) async fn snapshot_create(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
    Json(req): Json<SnapshotReq>,
) -> Res<serde_json::Value> {
    caller.need(Role::Publisher)?;
    let at = req.at.as_deref().map_or(RevRef::Head, RevRef::parse);
    let resource = format!("publish:{alias}");
    let label = req.label.clone();
    let rev = write_job(&st, &caller, "snapshot", resource, move |r| {
        let rev = r.snapshot_create(&alias, &req.label, &req.description, &at)?;
        Ok((rev, format!("{} -> #{rev}", req.label)))
    })
    .await?;
    Ok(Json(json!({ "label": label, "revision": rev })))
}

pub(crate) async fn snapshot_drop(
    State(st): St,
    caller: Caller,
    Path((alias, label)): Path<(String, String)>,
) -> Res<serde_json::Value> {
    caller.need(Role::Publisher)?;
    let resource = format!("publish:{alias}");
    write_job(&st, &caller, "snapshot-drop", resource, move |r| {
        r.snapshot_drop(&alias, &label)?;
        Ok(((), label))
    })
    .await?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotPruneReq {
    /// Only labels older than this many seconds.
    older_than: Option<i64>,
    /// Always keep the newest N labels.
    #[serde(default)]
    keep_last: usize,
    #[serde(default)]
    dry_run: bool,
}

async fn snapshot_prune(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
    Json(req): Json<SnapshotPruneReq>,
) -> Res<Vec<SnapshotInfo>> {
    caller.need(Role::Publisher)?;
    if req.older_than.is_none() && req.keep_last == 0 {
        return Err(ApiError::invalid(
            "snapshot prune: give an age (older_than) or a number to keep (keep_last)",
        ));
    }
    if req.dry_run {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let list = read(&st, move |r| r.snapshots(&alias)).await?;
        return Ok(Json(
            list.into_iter()
                .skip(req.keep_last)
                .filter(|s| req.older_than.is_none_or(|age| s.created <= now - age))
                .collect(),
        ));
    }
    let resource = format!("publish:{alias}");
    let dropped = write_job(&st, &caller, "snapshot-prune", resource, move |r| {
        let v = r.snapshot_prune(&alias, req.older_than, req.keep_last, false)?;
        let n = v.len();
        Ok((v, format!("{n} label(s) dropped")))
    })
    .await?;
    Ok(Json(dropped))
}

// ---- configuration -------------------------------------------------------

/// Every publish layout and mirror; the TOML of `aptpod config`.
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigView {
    #[serde(default)]
    pub(crate) publishes: BTreeMap<String, Layout>,
    #[serde(default)]
    pub(crate) mirrors: BTreeMap<String, crate::mirrors::MirrorDef>,
}

async fn config_export(State(st): St, caller: Caller) -> Res<ConfigView> {
    caller.need(Role::Reader)?;
    let mirrors: BTreeMap<String, crate::mirrors::MirrorDef> =
        st.auth.mirrors()?.into_iter().collect();
    // A re-signed mirror's publish is made by the mirror, not configured.
    let publishes = read(&st, |r| r.publishes())
        .await?
        .into_iter()
        .filter(|(a, _)| {
            mirrors
                .get(a)
                .is_none_or(|d| d.kind != crate::mirrors::Kind::Resign)
        })
        .collect();
    Ok(Json(ConfigView { publishes, mirrors }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportReq {
    config: ConfigView,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    adopt: bool,
    #[serde(default)]
    apply: bool,
    #[serde(default)]
    expect: Bases,
}

/// The config file: path, last application, managed items.
async fn file_status(State(st): St, caller: Caller) -> Res<crate::configfile::Status> {
    caller.need(Role::Reader)?;
    let mut s = st.config_file.status();
    s.path = st.cfg.config_file.clone();
    Ok(Json(s))
}

/// What applying the config file now would change (nothing is applied).
async fn file_plan(State(st): St, caller: Caller) -> Res<Outcome> {
    caller.need(Role::Admin)?;
    Ok(Json(crate::configfile::run(&st, false).await?))
}

/// Applies the config file now (as SIGHUP does).
async fn file_apply(State(st): St, caller: Caller) -> Res<Outcome> {
    caller.need(Role::Admin)?;
    Ok(Json(crate::configfile::run(&st, true).await?))
}

/// `aptpod config import`: items the config file manages are skipped (with
/// a warning), so an exported file can be imported back as it is.
async fn config_import(State(st): St, caller: Caller, Json(req): Json<ImportReq>) -> Res<Outcome> {
    caller.need(Role::Admin)?;
    let mut config = req.config;
    let mut skipped = Vec::new();
    {
        let managed = st.config_file.managed();
        config.publishes.retain(|a, _| {
            let m = managed.publishes.contains(a);
            if m {
                skipped.push(format!(
                    "publish {a:?} is managed by the config file; skipped"
                ));
            }
            !m
        });
        config.mirrors.retain(|n, _| {
            let m = managed.mirrors.contains(n);
            if m {
                skipped.push(format!(
                    "mirror {n:?} is managed by the config file; skipped"
                ));
            }
            !m
        });
    }
    let mut out = import(
        &st,
        caller.user.name.clone(),
        config,
        req.force,
        req.adopt,
        commit(req.apply, req.expect),
    )
    .await?;
    out.plan.warnings.extend(skipped);
    Ok(Json(out))
}

/// Creates the publishes of `config` that do not exist and updates the
/// layouts of those that do, as one plan; mirrors are created or updated
/// with it. Publishes and mirrors missing from `config` are left alone
/// (reported as warnings).
pub(crate) async fn import(
    st: &Arc<AppState>,
    user: String,
    config: ConfigView,
    force: bool,
    adopt: bool,
    commit: Commit,
) -> Result<Outcome, ApiError> {
    let pubs = config.publishes;
    // Re-signed mirrors (as they will be after the import) fill the
    // publish of their name; it cannot be configured as well.
    let mut resigned: BTreeSet<String> = st
        .auth
        .mirrors()?
        .into_iter()
        .filter(|(_, d)| d.kind == crate::mirrors::Kind::Resign)
        .map(|(n, _)| n)
        .collect();
    for (n, d) in &config.mirrors {
        if d.kind == crate::mirrors::Kind::Resign {
            resigned.insert(n.clone());
        } else {
            resigned.remove(n);
        }
    }
    if let Some(a) = pubs.keys().find(|a| resigned.contains(*a)) {
        return Err(ApiError::invalid(format!(
            "publish {a:?} is filled by the re-signed mirror of that name; leave it out"
        )));
    }
    // Publish paths against the mirrors as they will be after the import.
    let mut mirror_paths: BTreeMap<String, std::path::PathBuf> = st
        .auth
        .mirrors()?
        .into_iter()
        .map(|(n, d)| (n, d.path))
        .collect();
    for (n, d) in &config.mirrors {
        mirror_paths.insert(n.clone(), d.path.clone());
    }
    let skip_warn = resigned.clone();
    for (a, l) in &pubs {
        if let Some((m, _)) = mirror_paths
            .iter()
            .find(|(_, p)| aptpod_repo::layout::paths_overlap(&l.path, p))
        {
            return Err(ApiError::invalid(format!(
                "publish {a:?} and mirror {m:?} have overlapping paths"
            )));
        }
    }
    let (mirror_changes, mirror_lines, mirror_warnings) =
        crate::mirror_api::import_changes(st, config.mirrors, adopt).await?;
    let apply = commit.apply;
    let list: Vec<(&String, &Layout)> = pubs.iter().collect();
    for (i, (a, la)) in list.iter().enumerate() {
        st.cfg
            .allows_publish_path(&la.path)
            .map_err(|e| ApiError::invalid(format!("publish {a:?}: {e}")))?;
        for (b, lb) in &list[i + 1..] {
            if aptpod_repo::layout::paths_overlap(&la.path, &lb.path) {
                return Err(ApiError::invalid(format!(
                    "publishes {a:?} and {b:?} have overlapping paths"
                )));
            }
        }
    }
    let resource = if pubs.is_empty() {
        "config".to_string()
    } else {
        pubs.keys()
            .map(|a| format!("publish:{a}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let out = crate::api::plan_job_as(st, user, "import", resource, commit, vec![], move |r| {
        let existing: BTreeSet<String> = r.publishes()?.into_iter().map(|(a, _)| a).collect();
        let mut plan = Plan::default();
        for a in existing
            .iter()
            .filter(|a| !pubs.contains_key(*a) && !skip_warn.contains(*a))
        {
            plan.warnings
                .push(format!("publish {a:?} is not in the file; left unchanged"));
        }
        for (alias, layout) in pubs {
            let p = if existing.contains(&alias) {
                r.plan_layout(&alias, layout, force, adopt)?
            } else {
                r.plan_create(&alias, layout, adopt)?
            };
            // An unchanged publish must not get an empty revision.
            plan.parts
                .extend(p.parts.into_iter().filter(|part| !part.changes.is_empty()));
            plan.warnings.extend(p.warnings);
        }
        Ok(plan)
    })
    .await?;
    let mut out = out.0;
    out.plan.lines.extend(mirror_lines);
    out.plan.warnings.extend(mirror_warnings);
    out.plan.empty &= mirror_changes.is_empty();
    if apply {
        for (name, def) in &mirror_changes {
            st.auth.mirror_put(name, def)?;
        }
        crate::schedule::write_status(st);
    }
    Ok(out)
}

// ---- search, maintenance -------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindQuery {
    /// Comma-separated patterns.
    q: Option<String>,
    versions: Option<String>,
    arch: Option<String>,
    /// Comma-separated publishes (default: all).
    publish: Option<String>,
    /// Only the newest version per publish, dist, component, name, arch.
    #[serde(default)]
    newest: bool,
}

async fn find(State(st): St, caller: Caller, Query(q): Query<FindQuery>) -> Res<Vec<Row>> {
    caller.need(Role::Reader)?;
    let specs = csv(&q.q).unwrap_or_default();
    let f = filter(&specs, q.versions.as_deref(), q.arch.as_deref())?;
    let aliases = csv(&q.publish);
    Ok(Json(
        read(&st, move |r| r.find(aliases.as_deref(), &f, q.newest)).await?,
    ))
}

#[derive(Serialize)]
struct GcView {
    /// Revisions forgotten per publish (history_max_age).
    expired: BTreeMap<String, usize>,
    #[serde(flatten)]
    gc: GcReport,
}

async fn gc(State(st): St, caller: Caller) -> Res<GcView> {
    caller.need(Role::Admin)?;
    let view = write_job(&st, &caller, "gc", "repository".into(), |r| {
        let mut expired = BTreeMap::new();
        for (alias, _) in r.publishes()? {
            let n = r.expire_history(&alias)?;
            if n > 0 {
                expired.insert(alias, n);
            }
        }
        let gc = r.gc()?;
        let summary = format!(
            "{} revision(s) expired, {} cell set(s) and {} package(s) removed",
            expired.values().sum::<usize>(),
            gc.cellsets,
            gc.packages
        );
        Ok((GcView { expired, gc }, summary))
    })
    .await?;
    Ok(Json(view))
}

async fn repair(State(st): St, caller: Caller) -> Res<Applied> {
    caller.need(Role::Admin)?;
    let applied = write_job(&st, &caller, "repair", "repository".into(), |r| {
        let a = r.repair()?;
        let summary = format!("{} dist(s) rewritten", a.published.len());
        Ok((a, summary))
    })
    .await?;
    Ok(Json(applied))
}
