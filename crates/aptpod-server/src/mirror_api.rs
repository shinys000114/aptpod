// SPDX-License-Identifier: GPL-3.0-only

//! Mirror API: definitions (admin), syncs (publisher), job logs.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path as FsPath;
use std::sync::Arc;

use aptpod_repo::layout::{check_alias, paths_overlap};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{ApiError, AppState, Caller, Res, St, read};
use crate::authdb::Role;
use crate::mirrors::{MirrorDef, Status, is_empty_dir};
use crate::runner::{SyncOpts, log_path};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/mirrors", get(list).post(create))
        .route(
            "/api/v1/mirrors/{name}",
            get(show).post(update).delete(remove),
        )
        .route("/api/v1/mirrors/{name}/sync", post(sync))
        .route("/api/v1/mirrors/{name}/stop", post(stop))
        .route("/api/v1/mirrors/{name}/check", post(check))
        .route("/api/v1/jobs/{id}/log", get(job_log))
}

fn not_found(name: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("no mirror {name:?}"),
    )
}

/// Refuses a publish (or other) path that overlaps a mirror's tree.
pub fn refuse_mirror_overlap(
    st: &AppState,
    path: &FsPath,
    except: Option<&str>,
) -> Result<(), ApiError> {
    for (name, def) in st.auth.mirrors()? {
        if Some(name.as_str()) != except && paths_overlap(path, &def.path) {
            return Err(ApiError::invalid(format!(
                "{} overlaps the tree of mirror {name:?} ({})",
                path.display(),
                def.path.display()
            )));
        }
    }
    Ok(())
}

/// Checks a definition and its destination; returns it as it will be
/// stored (prune off for an adopted, non-empty tree) and a note.
async fn checked(
    st: &Arc<AppState>,
    name: &str,
    mut def: MirrorDef,
    adopt: bool,
    previous: Option<&MirrorDef>,
) -> Result<(MirrorDef, Option<String>), ApiError> {
    check_alias(name).map_err(|e| ApiError::invalid(e.to_string()))?;
    if name == "index" {
        // status_dir/index.json lists all mirrors.
        return Err(ApiError::invalid(
            "\"index\" is reserved; pick another name",
        ));
    }
    def.check(&st.cfg)
        .map_err(|e| ApiError::invalid(format!("mirror {name:?}: {e}")))?;
    refuse_mirror_overlap(st, &def.path, Some(name))?;
    let path = def.path.clone();
    let publishes = read(st, |r| r.publishes()).await?;
    if let Some((alias, l)) = publishes
        .iter()
        .find(|(_, l)| paths_overlap(&l.path, &path))
    {
        return Err(ApiError::invalid(format!(
            "{} overlaps publish {alias:?} ({})",
            path.display(),
            l.path.display()
        )));
    }
    let same_path = previous.is_some_and(|p| p.path == def.path);
    let mut note = None;
    if !same_path && !is_empty_dir(&def.path) {
        if !adopt {
            return Err(ApiError::invalid(format!(
                "{} already holds files; pass adopt to mirror into it",
                def.path.display()
            )));
        }
        if def.prune {
            def.prune = false;
            note = Some(format!(
                "{} is adopted with prune off: syncs report unreferenced files without deleting \
                 them; review, then `aptpod mirror sync {name} --prune` once or set prune = true",
                def.path.display()
            ));
        }
    }
    Ok((def, note))
}

#[derive(Serialize)]
struct MirrorView {
    name: String,
    #[serde(flatten)]
    def: MirrorDef,
    status: Status,
    /// Job of the sync queued or running now.
    running: Option<i64>,
    /// Next scheduled sync (server time zone).
    next_run: Option<String>,
}

fn view(st: &AppState, name: String, def: MirrorDef) -> Result<MirrorView, ApiError> {
    Ok(MirrorView {
        status: st.auth.mirror_status(&name)?,
        running: st.mirror_runner.running(&name),
        next_run: crate::schedule::next_run(st, &def),
        name,
        def,
    })
}

async fn list(State(st): St, caller: Caller) -> Res<Vec<MirrorView>> {
    caller.need(Role::Reader)?;
    let mut out = Vec::new();
    for (name, def) in st.auth.mirrors()? {
        out.push(view(&st, name, def)?);
    }
    Ok(Json(out))
}

async fn show(State(st): St, caller: Caller, Path(name): Path<String>) -> Res<MirrorView> {
    caller.need(Role::Reader)?;
    let def = st.auth.mirror(&name)?.ok_or_else(|| not_found(&name))?;
    Ok(Json(view(&st, name, def)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateReq {
    name: String,
    def: MirrorDef,
    #[serde(default)]
    adopt: bool,
}

async fn create(State(st): St, caller: Caller, Json(req): Json<CreateReq>) -> Res<Value> {
    caller.need(Role::Admin)?;
    if st.auth.mirror(&req.name)?.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            format!("mirror {:?} already exists", req.name),
        ));
    }
    let (def, note) = checked(&st, &req.name, req.def, req.adopt, None).await?;
    st.auth.mirror_put(&req.name, &def)?;
    crate::schedule::write_status(&st);
    tracing::info!(admin = %caller.user.name, mirror = %req.name, "mirror created");
    Ok(Json(json!({ "name": req.name, "note": note })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateReq {
    def: MirrorDef,
    #[serde(default)]
    adopt: bool,
}

async fn update(
    State(st): St,
    caller: Caller,
    Path(name): Path<String>,
    Json(req): Json<UpdateReq>,
) -> Res<Value> {
    caller.need(Role::Admin)?;
    let old = st.auth.mirror(&name)?.ok_or_else(|| not_found(&name))?;
    if let Some(job) = st.mirror_runner.running(&name) {
        return Err(busy(&name, job));
    }
    let (def, note) = checked(&st, &name, req.def, req.adopt, Some(&old)).await?;
    let moved = (old.path != def.path).then(|| old.path.display().to_string());
    st.auth.mirror_put(&name, &def)?;
    crate::schedule::write_status(&st);
    Ok(Json(
        json!({ "name": name, "note": note, "old_path_left": moved }),
    ))
}

fn busy(name: &str, job: i64) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "conflict",
        format!("mirror {name:?} is syncing (job {job}); stop it first"),
    )
}

/// Forgets a mirror; its tree is left on disk.
async fn remove(State(st): St, caller: Caller, Path(name): Path<String>) -> Res<Value> {
    caller.need(Role::Admin)?;
    let def = st.auth.mirror(&name)?.ok_or_else(|| not_found(&name))?;
    if let Some(job) = st.mirror_runner.running(&name) {
        return Err(busy(&name, job));
    }
    st.auth.mirror_del(&name)?;
    crate::schedule::write_status(&st);
    let _ = std::fs::remove_dir_all(st.cfg.data_dir.join("mirrors").join(&name));
    Ok(Json(json!({ "path_left": def.path })))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SyncReq {
    #[serde(default)]
    prune: bool,
    #[serde(default)]
    verify_all: bool,
}

async fn sync(
    State(st): St,
    caller: Caller,
    Path(name): Path<String>,
    body: Option<Json<SyncReq>>,
) -> Res<Value> {
    caller.need(Role::Publisher)?;
    let def = st.auth.mirror(&name)?.ok_or_else(|| not_found(&name))?;
    let req = body.map(|b| b.0).unwrap_or_default();
    // Deleting what an admin left in place (prune off) is an admin call.
    if req.prune && !def.prune {
        caller.need(Role::Admin)?;
    }
    let opts = SyncOpts {
        prune: req.prune,
        verify_all: req.verify_all,
    };
    let job = st
        .mirror_runner
        .submit(&st, &name, def, &caller.user.name, opts)?;
    Ok(Json(json!({ "job": job })))
}

async fn stop(State(st): St, caller: Caller, Path(name): Path<String>) -> Res<Value> {
    caller.need(Role::Publisher)?;
    match st.mirror_runner.stop(&name) {
        Some(job) => Ok(Json(json!({ "job": job }))),
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("mirror {name:?} is not syncing"),
        )),
    }
}

/// Probes the upstream (Release and signature, or an rsync listing).
async fn check(State(st): St, caller: Caller, Path(name): Path<String>) -> Res<Value> {
    caller.need(Role::Reader)?;
    let def = st.auth.mirror(&name)?.ok_or_else(|| not_found(&name))?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    // The HTTP engine must not run on a tokio thread.
    std::thread::spawn(move || {
        let lines: Vec<(String, Result<String, String>)> = match def.engine() {
            Err(e) => vec![("config".into(), Err(e))],
            Ok(crate::mirrors::Engine::Rsync(r)) => {
                vec![(def.url.clone(), aptpod_mirror::rsync::probe(&r))]
            }
            Ok(crate::mirrors::Engine::Http(h)) => {
                match aptpod_mirror::fetch::Http::new(std::time::Duration::from_secs(30)) {
                    Ok(http) => aptpod_mirror::http::probe(&h, &http),
                    Err(e) => vec![("http".into(), Err(e.to_string()))],
                }
            }
        };
        let _ = tx.send(lines);
    });
    let lines = rx
        .await
        .map_err(|_| ApiError::internal("the check did not finish"))?;
    let ok = lines.iter().all(|(_, r)| r.is_ok());
    let items: Vec<Value> = lines
        .into_iter()
        .map(|(what, r)| match r {
            Ok(m) => json!({ "what": what, "ok": true, "detail": m }),
            Err(e) => json!({ "what": what, "ok": false, "detail": e }),
        })
        .collect();
    Ok(Json(json!({ "ok": ok, "items": items })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogQuery {
    #[serde(default)]
    offset: u64,
}

/// A slice of a job's log from `offset` (at most 1 MiB), the next offset,
/// and whether the job is still running.
async fn job_log(
    State(st): St,
    caller: Caller,
    Path(id): Path<i64>,
    Query(q): Query<LogQuery>,
) -> Res<Value> {
    caller.need(Role::Reader)?;
    let job = st
        .auth
        .job(id)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", format!("no job {id}")))?;
    let path = log_path(&st, id);
    let mut text = Vec::new();
    let mut next = q.offset;
    if let Ok(mut f) = std::fs::File::open(&path) {
        f.seek(SeekFrom::Start(q.offset))?;
        f.take(1 << 20).read_to_end(&mut text)?;
        next += text.len() as u64;
    }
    Ok(Json(json!({
        "text": String::from_utf8_lossy(&text),
        "offset": next,
        "running": job.state == "running",
        "state": job.state,
    })))
}

/// Mirror changes of a config import: (name, definition, plan line).
pub async fn import_changes(
    st: &Arc<AppState>,
    mirrors: BTreeMap<String, MirrorDef>,
    adopt: bool,
) -> Result<(Vec<(String, MirrorDef)>, Vec<String>, Vec<String>), ApiError> {
    let mut changes = Vec::new();
    let mut lines = Vec::new();
    let mut warnings = Vec::new();
    let existing: BTreeMap<String, MirrorDef> = st.auth.mirrors()?.into_iter().collect();
    for name in existing.keys().filter(|n| !mirrors.contains_key(*n)) {
        warnings.push(format!(
            "mirror {name:?} is not in the file; left unchanged"
        ));
    }
    let list: Vec<(&String, &MirrorDef)> = mirrors.iter().collect();
    for (i, (a, da)) in list.iter().enumerate() {
        for (b, db) in &list[i + 1..] {
            if paths_overlap(&da.path, &db.path) {
                return Err(ApiError::invalid(format!(
                    "mirrors {a:?} and {b:?} have overlapping paths"
                )));
            }
        }
    }
    for (name, def) in mirrors {
        let old = existing.get(&name);
        if old == Some(&def) {
            continue;
        }
        if let Some(job) = st.mirror_runner.running(&name) {
            return Err(busy(&name, job));
        }
        let (def, note) = checked(st, &name, def, adopt, old).await?;
        lines.push(format!(
            "mirror {name}: {}",
            if old.is_some() { "update" } else { "create" }
        ));
        if let Some(n) = note {
            warnings.push(n);
        }
        changes.push((name, def));
    }
    Ok((changes, lines, warnings))
}
