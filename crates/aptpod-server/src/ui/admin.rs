// SPDX-License-Identifier: GPL-3.0-only

//! Mirrors, job logs, the own account (tokens, password) and users.

use std::sync::Arc;

use askama::Template;
use axum::Json;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use super::{Shell, UiUser, base, error_part, fmt_time, render, state_class};
use crate::api::{ApiError, AppState};
use crate::authdb::Role;

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/mirrors", get(mirrors_page))
        .route("/mirrors/{name}", get(mirror_page))
        .route("/mirrors/{name}/sync", post(mirror_sync))
        .route("/mirrors/{name}/stop", post(mirror_stop))
        .route("/mirrors/{name}/check", post(mirror_check))
        .route("/mirrors/{name}/edit", post(mirror_edit))
        .route("/jobs/{id}/log", get(job_log))
        .route("/account", get(account_page))
        .route("/account/tokens", post(token_create))
        .route("/account/tokens/{id}/revoke", post(token_revoke))
        .route("/account/password", post(password))
        .route("/admin/users", get(users_page).post(user_add))
        .route("/admin/users/{name}", post(user_set))
        .route("/admin/users/{name}/delete", post(user_del))
}

#[derive(Template)]
#[template(path = "message.html")]
struct Message {
    base: &'static str,
    ok: bool,
    lines: Vec<String>,
    job: Option<i64>,
}

fn message(ok: bool, lines: Vec<String>, job: Option<i64>) -> Response {
    render(&Message {
        base: base(),
        ok,
        lines,
        job,
    })
}

// ---- mirrors ---------------------------------------------------------------------

struct MirrorRow {
    name: String,
    kind: String,
    path: String,
    state: String,
    state_class: &'static str,
    finished: String,
    summary: String,
    next_run: String,
    managed: bool,
}

#[derive(Template)]
#[template(path = "mirrors.html")]
struct MirrorsPage {
    s: Shell,
    mirrors: Vec<MirrorRow>,
}

fn kind_name(k: crate::mirrors::Kind) -> String {
    serde_json::to_value(k)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn shown_state(v: &crate::mirror_api::MirrorView) -> String {
    if v.running.is_some() {
        "syncing".into()
    } else {
        v.status
            .state
            .clone()
            .unwrap_or_else(|| "never synced".into())
    }
}

async fn mirrors_page(State(st): State<Arc<AppState>>, u: UiUser) -> Response {
    match crate::mirror_api::list(State(st.clone()), u.caller()).await {
        Ok(Json(list)) => render(&MirrorsPage {
            s: u.shell(),
            mirrors: list
                .into_iter()
                .map(|v| {
                    let state = shown_state(&v);
                    MirrorRow {
                        state_class: state_class(&state),
                        state,
                        kind: kind_name(v.def.kind),
                        path: v.def.path.display().to_string(),
                        finished: fmt_time(&st, v.status.finished),
                        summary: v.status.summary.clone().unwrap_or_default(),
                        next_run: v.next_run.clone().unwrap_or_default(),
                        managed: v.managed,
                        name: v.name,
                    }
                })
                .collect(),
        }),
        Err(e) => error_part(&e),
    }
}

#[derive(Template)]
#[template(path = "mirror.html")]
struct MirrorPage {
    s: Shell,
    name: String,
    managed: bool,
    state: String,
    state_class: &'static str,
    job: Option<i64>,
    finished: String,
    next_run: String,
    summary: String,
    def_toml: String,
    prune: bool,
    can_publish: bool,
    can_admin: bool,
}

async fn mirror_page(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(name): Path<String>,
) -> Response {
    match crate::mirror_api::show(State(st.clone()), u.caller(), Path(name)).await {
        Ok(Json(v)) => {
            let state = shown_state(&v);
            render(&MirrorPage {
                s: u.shell(),
                managed: v.managed,
                state_class: state_class(&state),
                state,
                job: v.running.or(v.status.job),
                finished: fmt_time(&st, v.status.finished),
                next_run: v.next_run.clone().unwrap_or_default(),
                summary: v.status.summary.clone().unwrap_or_default(),
                def_toml: toml::to_string_pretty(&v.def).unwrap_or_default(),
                prune: v.def.prune,
                can_publish: u.can(Role::Publisher),
                can_admin: u.can(Role::Admin),
                name: v.name,
            })
        }
        Err(e) => error_part(&e),
    }
}

#[derive(Deserialize)]
struct SyncForm {
    #[serde(default)]
    prune: String,
}

async fn mirror_sync(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(name): Path<String>,
    Form(f): Form<SyncForm>,
) -> Response {
    let req = crate::mirror_api::SyncReq {
        prune: f.prune == "1",
        verify_all: false,
    };
    match crate::mirror_api::sync(State(st.clone()), u.caller(), Path(name), Some(Json(req))).await
    {
        Ok(Json(v)) => {
            let job = v["job"].as_i64();
            message(true, vec!["Sync started.".into()], job)
        }
        Err(e) => error_part(&e),
    }
}

async fn mirror_stop(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(name): Path<String>,
) -> Response {
    match crate::mirror_api::stop(State(st.clone()), u.caller(), Path(name)).await {
        Ok(Json(v)) => message(
            true,
            vec!["Asked the sync to stop.".into()],
            v["job"].as_i64(),
        ),
        Err(e) => error_part(&e),
    }
}

async fn mirror_check(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(name): Path<String>,
) -> Response {
    match crate::mirror_api::check(State(st.clone()), u.caller(), Path(name)).await {
        Ok(Json(v)) => {
            let lines = v["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|it| {
                    format!(
                        "{} {}: {}",
                        if it["ok"].as_bool() == Some(true) {
                            "ok"
                        } else {
                            "FAILED"
                        },
                        it["what"].as_str().unwrap_or(""),
                        it["detail"].as_str().unwrap_or("")
                    )
                })
                .collect();
            message(v["ok"].as_bool() == Some(true), lines, None)
        }
        Err(e) => error_part(&e),
    }
}

#[derive(Deserialize)]
struct EditForm {
    def: String,
    #[serde(default)]
    adopt: String,
}

async fn mirror_edit(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(name): Path<String>,
    Form(f): Form<EditForm>,
) -> Response {
    let def: crate::mirrors::MirrorDef = match toml::from_str(&f.def) {
        Ok(d) => d,
        Err(e) => return error_part(&ApiError::invalid(format!("definition: {e}"))),
    };
    let req = crate::mirror_api::UpdateReq {
        def,
        adopt: f.adopt == "1",
    };
    match crate::mirror_api::update(State(st.clone()), u.caller(), Path(name), Json(req)).await {
        Ok(Json(v)) => {
            let mut lines = vec!["Saved.".to_string()];
            if let Some(n) = v["note"].as_str() {
                lines.push(n.to_string());
            }
            message(true, lines, None)
        }
        Err(e) => error_part(&e),
    }
}

// ---- job log ---------------------------------------------------------------------

#[derive(Template)]
#[template(path = "joblog.html")]
struct JobLog {
    s: Shell,
    id: i64,
    running: bool,
    log: String,
}

/// The log of a job; keeps polling itself while the job runs.
async fn job_log(State(st): State<Arc<AppState>>, u: UiUser, Path(id): Path<i64>) -> Response {
    let Ok(Some(j)) = st.auth.job(id) else {
        return (axum::http::StatusCode::NOT_FOUND, "no such job").into_response();
    };
    render(&JobLog {
        s: u.shell(),
        id,
        running: j.state == "running",
        log: super::read_log(&st, id),
    })
}

// ---- account ---------------------------------------------------------------------

struct TokenRow {
    id: i64,
    name: String,
    created: String,
    last_used: String,
}

#[derive(Template)]
#[template(path = "tokens.html")]
struct TokensPart {
    base: &'static str,
    tokens: Vec<TokenRow>,
    new_token: Option<String>,
    errors: Vec<String>,
}

#[derive(Template)]
#[template(path = "account.html")]
struct AccountPage {
    s: Shell,
    base: &'static str,
    tokens: Vec<TokenRow>,
    new_token: Option<String>,
    errors: Vec<String>,
}

async fn token_rows(st: &Arc<AppState>, u: &UiUser) -> Result<Vec<TokenRow>, ApiError> {
    let Json(list) = crate::users::tokens(State(st.clone()), u.caller()).await?;
    Ok(list
        .into_iter()
        .map(|t| TokenRow {
            id: t.id,
            name: t.name,
            created: fmt_time(st, Some(t.created)),
            last_used: t
                .last_used
                .map_or("never".into(), |x| fmt_time(st, Some(x))),
        })
        .collect())
}

async fn account_page(State(st): State<Arc<AppState>>, u: UiUser) -> Response {
    match token_rows(&st, &u).await {
        Ok(tokens) => render(&AccountPage {
            s: u.shell(),
            base: base(),
            tokens,
            new_token: None,
            errors: vec![],
        }),
        Err(e) => error_part(&e),
    }
}

async fn tokens_part(
    st: &Arc<AppState>,
    u: &UiUser,
    new_token: Option<String>,
    errors: Vec<String>,
) -> Response {
    match token_rows(st, u).await {
        Ok(tokens) => render(&TokensPart {
            base: base(),
            tokens,
            new_token,
            errors,
        }),
        Err(e) => error_part(&e),
    }
}

#[derive(Deserialize)]
struct TokenForm {
    name: String,
}

async fn token_create(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Form(f): Form<TokenForm>,
) -> Response {
    let req = crate::users::TokenReq { name: f.name };
    match crate::users::token_create(State(st.clone()), u.caller(), Json(req)).await {
        Ok(Json(v)) => tokens_part(&st, &u, v["token"].as_str().map(String::from), vec![]).await,
        Err(e) => tokens_part(&st, &u, None, vec![e.message().to_string()]).await,
    }
}

async fn token_revoke(State(st): State<Arc<AppState>>, u: UiUser, Path(id): Path<i64>) -> Response {
    let errors = match crate::api::revoke_token(State(st.clone()), u.caller(), Path(id)).await {
        Ok(_) => vec![],
        Err(e) => vec![e.message().to_string()],
    };
    tokens_part(&st, &u, None, errors).await
}

#[derive(Deserialize)]
struct PasswordForm {
    current: String,
    new: String,
}

async fn password(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Form(f): Form<PasswordForm>,
) -> Response {
    let req = crate::users::OwnPasswordReq {
        current: f.current,
        new: f.new,
    };
    match crate::users::own_password(State(st.clone()), u.caller(), Json(req)).await {
        Ok(_) => message(true, vec!["Password changed.".into()], None),
        Err(e) => error_part(&e),
    }
}

// ---- users (admin) ----------------------------------------------------------------

struct UserRow {
    name: String,
    role: &'static str,
}

#[derive(Template)]
#[template(path = "users_table.html")]
struct UsersPart {
    base: &'static str,
    users: Vec<UserRow>,
    errors: Vec<String>,
}

#[derive(Template)]
#[template(path = "users.html")]
struct UsersPage {
    s: Shell,
    base: &'static str,
    users: Vec<UserRow>,
    errors: Vec<String>,
}

async fn user_rows(st: &Arc<AppState>, u: &UiUser) -> Result<Vec<UserRow>, ApiError> {
    let Json(list) = crate::users::users(State(st.clone()), u.caller()).await?;
    Ok(list
        .into_iter()
        .map(|x| UserRow {
            name: x.name,
            role: x.role.as_str(),
        })
        .collect())
}

async fn users_page(State(st): State<Arc<AppState>>, u: UiUser) -> Response {
    match user_rows(&st, &u).await {
        Ok(users) => render(&UsersPage {
            s: u.shell(),
            base: base(),
            users,
            errors: vec![],
        }),
        Err(e) => error_part(&e),
    }
}

async fn users_part(st: &Arc<AppState>, u: &UiUser, errors: Vec<String>) -> Response {
    match user_rows(st, u).await {
        Ok(users) => render(&UsersPart {
            base: base(),
            users,
            errors,
        }),
        Err(e) => error_part(&e),
    }
}

fn role(s: &str) -> Result<Role, ApiError> {
    s.parse::<Role>().map_err(ApiError::invalid)
}

#[derive(Deserialize)]
struct UserAddForm {
    name: String,
    role: String,
    password: String,
}

async fn user_add(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Form(f): Form<UserAddForm>,
) -> Response {
    let res = async {
        let req = crate::users::UserReq {
            name: f.name,
            password: f.password,
            role: role(&f.role)?,
        };
        crate::users::user_add(State(st.clone()), u.caller(), Json(req)).await
    }
    .await;
    let errors = res
        .err()
        .map(|e| vec![e.message().to_string()])
        .unwrap_or_default();
    users_part(&st, &u, errors).await
}

#[derive(Deserialize)]
struct UserSetForm {
    role: String,
    #[serde(default)]
    password: String,
}

async fn user_set(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(name): Path<String>,
    Form(f): Form<UserSetForm>,
) -> Response {
    let res = async {
        let req = crate::users::UserSetReq {
            role: Some(role(&f.role)?),
            password: (!f.password.is_empty()).then_some(f.password),
        };
        crate::users::user_set(State(st.clone()), u.caller(), Path(name), Json(req)).await
    }
    .await;
    let errors = res
        .err()
        .map(|e| vec![e.message().to_string()])
        .unwrap_or_default();
    users_part(&st, &u, errors).await
}

async fn user_del(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(name): Path<String>,
) -> Response {
    let errors = match crate::users::user_del(State(st.clone()), u.caller(), Path(name)).await {
        Ok(_) => vec![],
        Err(e) => vec![e.message().to_string()],
    };
    users_part(&st, &u, errors).await
}
