// SPDX-License-Identifier: GPL-3.0-only

//! REST API (`/api/v1`). Changes are previewed and applied statelessly: a
//! preview returns the plan and the head revisions it was computed from
//! (`bases`); applying sends the same request with `apply` and those bases
//! as `expect`, and is refused if a head has moved since.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aptpod_repo::{Applied, Change, Error as RepoError, Filter, Layout, Plan, Repo};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State};
use axum::http::{StatusCode, header, request::Parts};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::authdb::{AuthDb, Role, User, random_hex};
use crate::config::Config;
use crate::writer::{Stopped, Writer};

pub struct AppState {
    pub cfg: Config,
    pub auth: AuthDb,
    pub writer: Writer,
    pub reader: Mutex<Repo>,
    pub uploads: PathBuf,
    /// Bounds concurrent password checks (argon2 is slow and memory hungry
    /// on purpose, and `/login` is unauthenticated).
    pub logins: tokio::sync::Semaphore,
    pub events: tokio::sync::broadcast::Sender<crate::events::JobEvent>,
    /// Set when shutting down; ends event streams so connections close.
    pub stopping: tokio::sync::watch::Sender<bool>,
    pub mirror_runner: crate::runner::Runner,
    pub config_file: crate::configfile::FileState,
}

pub(crate) type St = State<Arc<AppState>>;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/login", post(login))
        .route("/api/v1/token", delete(logout))
        .route("/api/v1/whoami", get(whoami))
        .route("/api/v1/tokens/{id}", delete(revoke_token))
        .route(
            "/api/v1/uploads",
            post(upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/api/v1/publishes", get(publishes).post(create_publish))
        .route("/api/v1/publishes/{alias}", get(publish))
        .route("/api/v1/publishes/{alias}/packages", get(packages))
        .route("/api/v1/publishes/{alias}/add", post(add))
        .route("/api/v1/jobs", get(jobs))
        .merge(crate::mirror_api::routes())
        .route("/api/v1/events", get(crate::events::events))
        .merge(crate::ops::routes())
        .merge(crate::users::routes())
        .merge(crate::ui::routes())
        .fallback(|| async {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such endpoint")
        })
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

// ---- errors -------------------------------------------------------------

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    kind: &'static str,
    message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
        }
    }
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid", message)
    }
    pub(crate) fn internal(message: impl std::fmt::Display) -> Self {
        tracing::error!("{message}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            message.to_string(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": self.message, "kind": self.kind })),
        )
            .into_response()
    }
}

impl From<RepoError> for ApiError {
    fn from(e: RepoError) -> Self {
        use StatusCode as S;
        let (status, kind) = match &e {
            RepoError::Invalid(_) => (S::BAD_REQUEST, "invalid"),
            RepoError::Core(aptpod_core::Error::Format(_) | aptpod_core::Error::At { .. }) => {
                (S::BAD_REQUEST, "invalid")
            }
            RepoError::NotFound(_) => (S::NOT_FOUND, "not_found"),
            RepoError::Conflict(_) | RepoError::Core(aptpod_core::Error::Conflict(_)) => {
                (S::CONFLICT, "conflict")
            }
            RepoError::Blocked(_) => (S::CONFLICT, "blocked"),
            RepoError::Stale(_) => (S::CONFLICT, "stale"),
            RepoError::Unsynced(_) => (S::INTERNAL_SERVER_ERROR, "unsynced"),
            _ => return Self::internal(e),
        };
        Self::new(status, kind, e.to_string())
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        Self::internal(format!("server database: {e}"))
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        Self::internal(e)
    }
}

impl From<Stopped> for ApiError {
    fn from(_: Stopped) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "the server is shutting down",
        )
    }
}

pub(crate) fn join_err(e: tokio::task::JoinError) -> ApiError {
    ApiError::internal(format!("task failed: {e}"))
}

pub(crate) type Res<T> = Result<Json<T>, ApiError>;

// ---- authentication -----------------------------------------------------

/// The authenticated user of a request (`Authorization: Bearer <token>`).
pub struct Caller {
    pub user: User,
    token: String,
}

impl Caller {
    /// A caller from a web UI session (no API token).
    pub(crate) fn from_session(user: User) -> Self {
        Self {
            user,
            token: String::new(),
        }
    }
}

impl Caller {
    pub(crate) fn need(&self, role: Role) -> Result<(), ApiError> {
        if self.user.role >= role {
            Ok(())
        } else {
            Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                format!("this needs the {} role", role.as_str()),
            ))
        }
    }
}

impl FromRequestParts<Arc<AppState>> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &Arc<AppState>) -> Result<Self, ApiError> {
        let unauthorized = || {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or invalid token; run `aptpod login`",
            )
        };
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(unauthorized)?
            .trim()
            .to_string();
        let user = st.auth.authenticate(&token)?.ok_or_else(unauthorized)?;
        Ok(Caller { user, token })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginReq {
    user: String,
    password: String,
    #[serde(default)]
    token_name: Option<String>,
}

async fn login(State(st): St, Json(req): Json<LoginReq>) -> Res<serde_json::Value> {
    let _permit = st.logins.acquire().await.map_err(|_| Stopped)?;
    let st = st.clone();
    let (user, token) = tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        let user = st
            .auth
            .check_password(&req.user, &req.password)?
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "wrong user name or password",
                )
            })?;
        let name = req.token_name.as_deref().unwrap_or("cli");
        let token = st.auth.token_create(user.id, name)?;
        tracing::info!(user = %user.name, token = name, "login");
        Ok((user, token))
    })
    .await
    .map_err(join_err)??;
    Ok(Json(json!({ "token": token, "user": user })))
}

async fn logout(State(st): St, caller: Caller) -> Res<serde_json::Value> {
    st.auth.token_revoke_plain(&caller.token)?;
    Ok(Json(json!({})))
}

async fn whoami(caller: Caller) -> Res<User> {
    Ok(Json(caller.user))
}

pub(crate) async fn revoke_token(
    State(st): St,
    caller: Caller,
    Path(id): Path<i64>,
) -> Res<serde_json::Value> {
    if !st.auth.token_revoke(caller.user.id, id)? {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no token {id}"),
        ));
    }
    Ok(Json(json!({})))
}

// ---- uploads ------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct UploadInfo {
    pub(crate) id: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
}

/// Streams the request body into `uploads/<id>.deb`. Uploads are referred
/// to by id in later requests and removed once applied (or after an hour).
async fn upload(State(st): St, caller: Caller, body: Body) -> Res<UploadInfo> {
    caller.need(Role::Publisher)?;
    let info = store_upload(&st, body.into_data_stream()).await?;
    tracing::info!(user = %caller.user.name, id = %info.id, size = info.size, "upload");
    Ok(Json(info))
}

/// Streams `stream` into `uploads/<id>.deb` (at most `upload_limit`
/// bytes); the API body and the UI's multipart fields both go through
/// here.
pub(crate) async fn store_upload<S, E>(st: &AppState, mut stream: S) -> Result<UploadInfo, ApiError>
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let id = random_hex(16);
    let tmp = st.uploads.join(format!(".{id}.part"));
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let received: Result<(), ApiError> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ApiError::invalid(format!("reading the upload: {e}")))?;
            size += chunk.len() as u64;
            if size > st.cfg.upload_limit {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "too_large",
                    format!("uploads are limited to {} bytes", st.cfg.upload_limit),
                ));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(())
    }
    .await;
    drop(file);
    if let Err(e) = received {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    tokio::fs::rename(&tmp, st.uploads.join(format!("{id}.deb"))).await?;
    let sha256 = crate::authdb::hex(&hasher.finalize());
    Ok(UploadInfo { id, sha256, size })
}

fn upload_path(st: &AppState, id: &str) -> Result<PathBuf, ApiError> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(ApiError::invalid(format!("invalid upload id {id:?}")));
    }
    let p = st.uploads.join(format!("{id}.deb"));
    if !p.is_file() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no upload {id} (uploads expire after an hour)"),
        ));
    }
    Ok(p)
}

// ---- queries ------------------------------------------------------------

/// Runs a query on the read handle, off the async runtime.
pub(crate) async fn read<T, F>(st: &Arc<AppState>, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Repo) -> Result<T, RepoError> + Send + 'static,
{
    let st = st.clone();
    tokio::task::spawn_blocking(move || {
        let repo = st.reader.lock().unwrap_or_else(|e| e.into_inner());
        f(&repo).map_err(ApiError::from)
    })
    .await
    .map_err(join_err)?
}

#[derive(Serialize)]
struct PublishView {
    alias: String,
    layout: Layout,
    /// Declared in the config file (edit it there).
    managed: bool,
}

async fn publishes(State(st): St, caller: Caller) -> Res<Vec<PublishView>> {
    caller.need(Role::Reader)?;
    let list = read(&st, |r| r.publishes()).await?;
    Ok(Json(
        list.into_iter()
            .map(|(alias, layout)| PublishView {
                managed: st.config_file.is_publish(&alias),
                alias,
                layout,
            })
            .collect(),
    ))
}

async fn publish(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
) -> Res<serde_json::Value> {
    caller.need(Role::Reader)?;
    let alias2 = alias.clone();
    let (layout, cells) = read(&st, move |r| Ok((r.layout(&alias)?, r.status(&alias)?))).await?;
    Ok(Json(
        json!({ "layout": layout, "cells": cells, "managed": st.config_file.is_publish(&alias2) }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackagesQuery {
    /// Comma-separated lists.
    component: Option<String>,
    dist: Option<String>,
    /// Comma-separated package patterns (`name`, `name_version`, globs).
    q: Option<String>,
    versions: Option<String>,
    arch: Option<String>,
}

pub(crate) fn csv(s: &Option<String>) -> Option<Vec<String>> {
    s.as_ref().map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect()
    })
}

async fn packages(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
    Query(q): Query<PackagesQuery>,
) -> Res<Vec<aptpod_repo::Row>> {
    caller.need(Role::Reader)?;
    let rows = read(&st, move |r| {
        let specs = csv(&q.q).unwrap_or_default();
        let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
        let filter = Filter::new(&specs, q.versions.as_deref(), q.arch.as_deref())?;
        r.list(
            &alias,
            csv(&q.component).as_deref(),
            csv(&q.dist).as_deref(),
            &filter,
        )
    })
    .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct JobsQuery {
    limit: Option<usize>,
}

async fn jobs(
    State(st): St,
    caller: Caller,
    Query(q): Query<JobsQuery>,
) -> Res<Vec<crate::authdb::Job>> {
    caller.need(Role::Reader)?;
    Ok(Json(st.auth.jobs(q.limit.unwrap_or(50).min(1000))?))
}

// ---- changes ------------------------------------------------------------

/// Fields every change request has (as `apply` / `expect` in each request
/// type: serde's `flatten` would accept unknown fields, and a typo such as
/// `aply` must not quietly turn an apply into a preview).
#[derive(Debug, Default)]
pub(crate) struct Commit {
    /// Apply instead of only previewing.
    pub(crate) apply: bool,
    /// `bases` of the preview the user accepted.
    pub(crate) expect: Bases,
}

pub(crate) type Bases = Option<Vec<(String, Option<i64>)>>;

#[derive(Serialize)]
struct ChangeView {
    alias: String,
    #[serde(flatten)]
    change: Change,
}

#[derive(Serialize)]
pub(crate) struct PlanView {
    pub(crate) lines: Vec<String>,
    changes: Vec<ChangeView>,
    pub(crate) warnings: Vec<String>,
    pub(crate) bases: Vec<(String, Option<i64>)>,
    pub(crate) destructive: bool,
    pub(crate) empty: bool,
}

impl PlanView {
    fn of(plan: &Plan) -> Self {
        Self {
            lines: plan.lines(),
            changes: plan
                .changes()
                .map(|(alias, c)| ChangeView {
                    alias: alias.to_string(),
                    change: c.clone(),
                })
                .collect(),
            warnings: plan.warnings.clone(),
            bases: plan.bases(),
            destructive: plan.is_destructive(),
            empty: plan.is_empty(),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct Outcome {
    pub(crate) plan: PlanView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) applied: Option<Applied>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) job: Option<i64>,
}

/// Computes a plan on the writer and, if asked, applies it there in the
/// same job, so nothing can move the heads in between.
pub(crate) async fn plan_job<F>(
    st: &Arc<AppState>,
    caller: &Caller,
    kind: &'static str,
    resource: String,
    commit: Commit,
    uploads: Vec<PathBuf>,
    make: F,
) -> Res<Outcome>
where
    F: FnOnce(&mut Repo) -> Result<Plan, RepoError> + Send + 'static,
{
    plan_job_as(
        st,
        caller.user.name.clone(),
        kind,
        resource,
        commit,
        uploads,
        make,
    )
    .await
}

/// [`plan_job`] on behalf of `user` (e.g. the config file).
pub(crate) async fn plan_job_as<F>(
    st: &Arc<AppState>,
    user: String,
    kind: &'static str,
    resource: String,
    commit: Commit,
    uploads: Vec<PathBuf>,
    make: F,
) -> Res<Outcome>
where
    F: FnOnce(&mut Repo) -> Result<Plan, RepoError> + Send + 'static,
{
    let st2 = st.clone();
    let out = st
        .writer
        .run(move |repo| -> Result<Outcome, ApiError> {
            let plan = make(repo)?;
            let view = PlanView::of(&plan);
            if !commit.apply {
                return Ok(Outcome {
                    plan: view,
                    applied: None,
                    job: None,
                });
            }
            if let Some(expect) = &commit.expect
                && *expect != view.bases
            {
                return Err(RepoError::Stale(format!(
                    "the repository changed since the preview (heads {expect:?}, now {:?}); preview again",
                    view.bases
                ))
                .into());
            }
            if plan.is_empty() {
                return Ok(Outcome {
                    plan: view,
                    applied: Some(Applied::default()),
                    job: None,
                });
            }
            let job = st2.job_start(kind, &resource, &user)?;
            let job_id = job.id();
            let res = repo.apply(&plan, &user);
            let summary = match &res {
                Ok(a) => Ok(format!(
                    "{} change(s); {}",
                    view.changes.len(),
                    a.revisions
                        .iter()
                        .map(|(alias, rev)| format!("{alias} #{rev}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                Err(e) => Err(e.to_string()),
            };
            job.finish(summary.as_deref().map_err(String::as_str))?;
            if matches!(res, Ok(_) | Err(RepoError::Unsynced(_))) {
                // Committed: the store has the files now.
                for p in &uploads {
                    let _ = std::fs::remove_file(p);
                }
            }
            tracing::info!(%user, kind, %resource, job = job_id, ok = res.is_ok(), "apply");
            Ok(Outcome {
                plan: view,
                applied: Some(res?),
                job: Some(job_id),
            })
        })
        .await??;
    Ok(Json(out))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateReq {
    pub(crate) alias: String,
    pub(crate) layout: Layout,
    #[serde(default)]
    pub(crate) adopt: bool,
    #[serde(default)]
    pub(crate) apply: bool,
    #[serde(default)]
    pub(crate) expect: Bases,
}

pub(crate) async fn create_publish(
    State(st): St,
    caller: Caller,
    Json(req): Json<CreateReq>,
) -> Res<Outcome> {
    caller.need(Role::Admin)?;
    crate::configfile::refuse(&st, "publish", &req.alias)?;
    st.cfg
        .allows_publish_path(&req.layout.path)
        .map_err(ApiError::invalid)?;
    crate::mirror_api::refuse_mirror_overlap(&st, &req.layout.path, None)?;
    let resource = format!("publish:{}", req.alias);
    plan_job(
        &st,
        &caller,
        "create",
        resource,
        Commit {
            apply: req.apply,
            expect: req.expect,
        },
        vec![],
        move |r| r.plan_create(&req.alias, req.layout, req.adopt),
    )
    .await
}

/// `"detect"` (from `~<dist>` version suffixes), `"all"`, or a list.
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum DistsReq {
    Word(String),
    List(Vec<String>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddReq {
    pub(crate) component: String,
    pub(crate) uploads: Vec<String>,
    #[serde(default)]
    pub(crate) dists: Option<DistsReq>,
    #[serde(default)]
    pub(crate) replace: bool,
    /// What to call each upload in messages (the client's file names).
    #[serde(default)]
    pub(crate) names: Vec<String>,
    #[serde(default)]
    pub(crate) apply: bool,
    #[serde(default)]
    pub(crate) expect: Bases,
}

pub(crate) async fn add(
    State(st): St,
    caller: Caller,
    Path(alias): Path<String>,
    Json(req): Json<AddReq>,
) -> Res<Outcome> {
    caller.need(Role::Publisher)?;
    if req.uploads.is_empty() {
        return Err(ApiError::invalid("no uploads given"));
    }
    let dists = match req.dists {
        None => aptpod_repo::ops::Dists::Detect,
        Some(DistsReq::Word(w)) if w == "detect" => aptpod_repo::ops::Dists::Detect,
        Some(DistsReq::Word(w)) if w == "all" => aptpod_repo::ops::Dists::All,
        Some(DistsReq::Word(w)) => {
            return Err(ApiError::invalid(format!(
                "dists must be \"detect\", \"all\" or a list, not {w:?}"
            )));
        }
        Some(DistsReq::List(l)) if l.is_empty() => {
            return Err(ApiError::invalid("empty dist list"));
        }
        Some(DistsReq::List(l)) => aptpod_repo::ops::Dists::List(l),
    };
    let paths = req
        .uploads
        .iter()
        .map(|id| upload_path(&st, id))
        .collect::<Result<Vec<_>, _>>()?;
    // Uploads belong to the server: the store may link them, not copy.
    let files: Vec<aptpod_repo::ops::AddFile> = paths
        .iter()
        .map(|p| aptpod_repo::ops::AddFile {
            path: p.clone(),
            sha256: None,
            owned: true,
        })
        .collect();
    let resource = format!("publish:{alias}");
    let component = req.component;
    let replace = req.replace;
    let labels: Vec<(String, String)> = paths
        .iter()
        .zip(&req.uploads)
        .enumerate()
        .map(|(i, (p, id))| {
            let label = req
                .names
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("upload {id}"));
            (p.display().to_string(), label)
        })
        .collect();
    let labels2 = labels.clone();
    let commit = Commit {
        apply: req.apply,
        expect: req.expect,
    };
    plan_job(&st, &caller, "add", resource, commit, paths, move |r| {
        let mut plan = r.plan_add(&alias, &component, &files, &dists, replace)?;
        for w in &mut plan.warnings {
            for (path, label) in &labels {
                *w = w.replace(path, label);
            }
        }
        Ok(plan)
    })
    .await
    .map_err(|mut e| {
        for (path, label) in &labels2 {
            e.message = e.message.replace(path, label);
        }
        e
    })
}
