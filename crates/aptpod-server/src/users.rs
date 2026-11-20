// SPDX-License-Identifier: GPL-3.0-only

//! Users and tokens through the API: everyone manages their own tokens and
//! password; admins manage users.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{ApiError, AppState, Caller, Res, St, join_err};
use crate::authdb::{self, Role, User};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/tokens", get(tokens).post(token_create))
        .route("/api/v1/password", post(own_password))
        .route("/api/v1/users", get(users).post(user_add))
        .route("/api/v1/users/{name}", post(user_set).delete(user_del))
}

fn not_found(name: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("no user {name:?}"),
    )
}

/// argon2 off the runtime, bounded like logins.
async fn hash(st: &Arc<AppState>, password: String) -> Result<String, ApiError> {
    let _permit = st
        .logins
        .acquire()
        .await
        .map_err(|_| ApiError::internal("shutting down"))?;
    tokio::task::spawn_blocking(move || authdb::hash_password(&password))
        .await
        .map_err(join_err)?
        .map_err(ApiError::invalid)
}

async fn tokens(State(st): St, caller: Caller) -> Res<Vec<authdb::TokenInfo>> {
    Ok(Json(st.auth.tokens(caller.user.id)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenReq {
    name: String,
}

/// A further token for the caller (e.g. for CI); shown once.
async fn token_create(State(st): St, caller: Caller, Json(req): Json<TokenReq>) -> Res<Value> {
    if req.name.is_empty() || req.name.len() > 100 {
        return Err(ApiError::invalid("token names have 1 to 100 characters"));
    }
    let token = st.auth.token_create(caller.user.id, &req.name)?;
    tracing::info!(user = %caller.user.name, token = %req.name, "token created");
    Ok(Json(json!({ "token": token })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnPasswordReq {
    current: String,
    new: String,
}

async fn own_password(
    State(st): St,
    caller: Caller,
    Json(req): Json<OwnPasswordReq>,
) -> Res<Value> {
    let name = caller.user.name.clone();
    let ok = {
        let _permit = st
            .logins
            .acquire()
            .await
            .map_err(|_| ApiError::internal("shutting down"))?;
        let st = st.clone();
        let name = name.clone();
        tokio::task::spawn_blocking(move || st.auth.check_password(&name, &req.current))
            .await
            .map_err(join_err)??
            .is_some()
    };
    if !ok {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "the current password is wrong",
        ));
    }
    let h = hash(&st, req.new).await?;
    st.auth.set_password(&name, &h)?;
    tracing::info!(user = %name, "password changed");
    Ok(Json(json!({})))
}

async fn users(State(st): St, caller: Caller) -> Res<Vec<User>> {
    caller.need(Role::Admin)?;
    Ok(Json(st.auth.users()?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserReq {
    name: String,
    password: String,
    role: Role,
}

async fn user_add(State(st): St, caller: Caller, Json(req): Json<UserReq>) -> Res<User> {
    caller.need(Role::Admin)?;
    authdb::check_user_name(&req.name).map_err(ApiError::invalid)?;
    if st.auth.user(&req.name)?.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            format!("user {:?} already exists", req.name),
        ));
    }
    let h = hash(&st, req.password).await?;
    st.auth.user_add(&req.name, &h, req.role)?;
    tracing::info!(admin = %caller.user.name, user = %req.name, role = req.role.as_str(), "user added");
    Ok(Json(
        st.auth
            .user(&req.name)?
            .ok_or_else(|| not_found(&req.name))?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserSetReq {
    role: Option<Role>,
    password: Option<String>,
}

/// Refuses to leave the server without an admin.
fn keep_an_admin(st: &AppState, user: &User) -> Result<(), ApiError> {
    if user.role == Role::Admin && st.auth.admin_count()? <= 1 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            format!("{:?} is the last admin", user.name),
        ));
    }
    Ok(())
}

async fn user_set(
    State(st): St,
    caller: Caller,
    Path(name): Path<String>,
    Json(req): Json<UserSetReq>,
) -> Res<User> {
    caller.need(Role::Admin)?;
    let user = st.auth.user(&name)?.ok_or_else(|| not_found(&name))?;
    if let Some(role) = req.role {
        if role != Role::Admin {
            keep_an_admin(&st, &user)?;
        }
        st.auth.set_role(&name, role)?;
    }
    if let Some(pw) = req.password {
        let h = hash(&st, pw).await?;
        st.auth.set_password(&name, &h)?;
    }
    tracing::info!(admin = %caller.user.name, user = %name, "user changed");
    Ok(Json(st.auth.user(&name)?.ok_or_else(|| not_found(&name))?))
}

async fn user_del(State(st): St, caller: Caller, Path(name): Path<String>) -> Res<Value> {
    caller.need(Role::Admin)?;
    let user = st.auth.user(&name)?.ok_or_else(|| not_found(&name))?;
    keep_an_admin(&st, &user)?;
    st.auth.user_del(&name)?;
    tracing::info!(admin = %caller.user.name, user = %name, "user removed");
    Ok(Json(json!({})))
}
