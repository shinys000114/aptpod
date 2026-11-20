// SPDX-License-Identifier: GPL-3.0-only

//! Repository actions on the publish page: add (upload), promote,
//! snapshots, history, diff and rollback. Changes go through the API
//! handlers with preview/apply.

use std::sync::Arc;

use askama::Template;
use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Form, Multipart, Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;

use super::{UiUser, base, error_part, fmt_time, outcome, parse_expect, render};
use crate::api::{ApiError, AppState, DistsReq, Outcome};
use crate::authdb::Role;

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/p/{alias}/add",
            post(add_upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/p/{alias}/add/apply", post(add_apply))
        .route("/p/{alias}/promote", post(promote))
        .route("/p/{alias}/snapshots", get(snapshots).post(snapshot_create))
        .route("/p/{alias}/snapshots/{label}/drop", post(snapshot_drop))
        .route("/p/{alias}/history", get(history))
        .route("/p/{alias}/diff", get(diff))
        .route("/p/{alias}/rollback", post(rollback))
        .route("/p/{alias}/layout", post(layout))
        .route("/publishes/new", post(create))
}

fn list(s: &str) -> Vec<String> {
    s.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

fn dists_req(s: &str) -> Option<DistsReq> {
    match s.trim() {
        "" | "detect" => None,
        "all" => Some(DistsReq::Word("all".into())),
        other => Some(DistsReq::List(list(other))),
    }
}

// ---- add -------------------------------------------------------------------

async fn add_call(
    st: &Arc<AppState>,
    u: &UiUser,
    alias: &str,
    f: &AddFields,
    apply: bool,
    expect: crate::api::Bases,
) -> Result<Outcome, ApiError> {
    let req = crate::api::AddReq {
        component: f.component.clone(),
        uploads: list(&f.uploads),
        dists: dists_req(&f.dists),
        replace: f.replace == "1",
        names: f
            .names
            .split('\n')
            .filter(|n| !n.is_empty())
            .map(String::from)
            .collect(),
        apply,
        expect,
    };
    let Json(out) = crate::api::add(
        State(st.clone()),
        u.caller(),
        Path(alias.to_string()),
        Json(req),
    )
    .await?;
    Ok(out)
}

#[derive(Deserialize, Default)]
struct AddFields {
    component: String,
    #[serde(default)]
    uploads: String,
    #[serde(default)]
    names: String,
    #[serde(default)]
    dists: String,
    #[serde(default)]
    replace: String,
    #[serde(default)]
    apply: String,
    #[serde(default)]
    expect: String,
}

fn add_fields(f: &AddFields) -> Vec<(String, String)> {
    vec![
        ("component".into(), f.component.clone()),
        ("uploads".into(), f.uploads.clone()),
        ("names".into(), f.names.clone()),
        ("dists".into(), f.dists.clone()),
        ("replace".into(), f.replace.clone()),
    ]
}

/// Multipart: files are streamed to uploads (with the upload limit), then
/// the add is previewed.
async fn add_upload(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(alias): Path<String>,
    mut mp: Multipart,
) -> Response {
    let res: Result<(AddFields, Outcome), ApiError> = async {
        if !u.can(Role::Publisher) {
            return Err(ApiError::new(
                axum::http::StatusCode::FORBIDDEN,
                "forbidden",
                "this needs the publisher role",
            ));
        }
        let mut f = AddFields::default();
        let (mut ids, mut names) = (Vec::new(), Vec::new());
        while let Some(field) = mp
            .next_field()
            .await
            .map_err(|e| ApiError::invalid(format!("reading the form: {e}")))?
        {
            let key = field.name().unwrap_or("").to_string();
            match key.as_str() {
                "file" => {
                    let name = field.file_name().unwrap_or("upload.deb").replace(['\n', '\r'], "");
                    if name.is_empty() {
                        continue;
                    }
                    let info = crate::api::store_upload(&st, field).await?;
                    tracing::info!(user = %u.user.name, id = %info.id, size = info.size, "web upload");
                    ids.push(info.id);
                    names.push(name);
                }
                key => {
                    let key = key.to_string();
                    let v = field
                        .text()
                        .await
                        .map_err(|e| ApiError::invalid(format!("reading the form: {e}")))?;
                    match key.as_str() {
                        "component" => f.component = v,
                        "dists" => f.dists = v,
                        "replace" => f.replace = v,
                        _ => {}
                    }
                }
            }
        }
        if ids.is_empty() {
            return Err(ApiError::invalid("choose at least one .deb file"));
        }
        f.uploads = ids.join(",");
        f.names = names.join("\n");
        let out = add_call(&st, &u, &alias, &f, false, None).await?;
        Ok((f, out))
    }
    .await;
    match res {
        Ok((f, out)) => outcome(
            &out,
            format!("{}p/{alias}/add/apply", base()),
            add_fields(&f),
            false,
        ),
        Err(e) => error_part(&e),
    }
}

async fn add_apply(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(alias): Path<String>,
    Form(f): Form<AddFields>,
) -> Response {
    let res = async {
        let expect = parse_expect(&f.expect)?;
        add_call(&st, &u, &alias, &f, f.apply == "1", expect).await
    }
    .await;
    match res {
        Ok(out) => outcome(
            &out,
            format!("{}p/{alias}/add/apply", base()),
            add_fields(&f),
            f.apply == "1",
        ),
        Err(e) => error_part(&e),
    }
}

// ---- promote -----------------------------------------------------------------

#[derive(Deserialize)]
struct PromoteForm {
    component: String,
    #[serde(default)]
    packages: String,
    #[serde(default)]
    dists: String,
    #[serde(default)]
    to_publish: String,
    #[serde(default)]
    to_component: String,
    #[serde(default)]
    to_dist: String,
    #[serde(default)]
    all_versions: String,
    #[serde(default, rename = "move")]
    move_: String,
    #[serde(default)]
    apply: String,
    #[serde(default)]
    expect: String,
}

async fn promote(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(alias): Path<String>,
    Form(f): Form<PromoteForm>,
) -> Response {
    let apply = f.apply == "1";
    let opt = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    let res: Result<Outcome, ApiError> = async {
        let dists = list(&f.dists);
        let req = crate::ops::PromoteReq {
            component: f.component.clone(),
            packages: list(&f.packages),
            all: false,
            versions: None,
            arch: None,
            dists: (!dists.is_empty()).then_some(dists),
            to_publish: opt(&f.to_publish),
            to_component: opt(&f.to_component),
            to_dist: opt(&f.to_dist),
            all_versions: f.all_versions == "1",
            move_: f.move_ == "1",
            apply,
            expect: if apply {
                parse_expect(&f.expect)?
            } else {
                None
            },
        };
        let Json(out) = crate::ops::promote(
            State(st.clone()),
            u.caller(),
            Path(alias.clone()),
            Json(req),
        )
        .await?;
        Ok(out)
    }
    .await;
    match res {
        Ok(out) => outcome(
            &out,
            format!("{}p/{alias}/promote", base()),
            vec![
                ("component".into(), f.component),
                ("packages".into(), f.packages),
                ("dists".into(), f.dists),
                ("to_publish".into(), f.to_publish),
                ("to_component".into(), f.to_component),
                ("to_dist".into(), f.to_dist),
                ("all_versions".into(), f.all_versions),
                ("move".into(), f.move_),
            ],
            apply,
        ),
        Err(e) => error_part(&e),
    }
}

// ---- snapshots ------------------------------------------------------------------

struct SnapRow {
    label: String,
    revision: i64,
    created: String,
    description: String,
}

#[derive(Template)]
#[template(path = "snapshots.html")]
struct SnapshotsPart {
    base: &'static str,
    alias: String,
    can_publish: bool,
    snapshots: Vec<SnapRow>,
    errors: Vec<String>,
}

async fn snapshots_part(
    st: &Arc<AppState>,
    u: &UiUser,
    alias: String,
    errors: Vec<String>,
) -> Response {
    match crate::ops::snapshots(State(st.clone()), u.caller(), Path(alias.clone())).await {
        Ok(Json(list)) => render(&SnapshotsPart {
            base: base(),
            can_publish: u.can(Role::Publisher),
            snapshots: list
                .into_iter()
                .map(|x| SnapRow {
                    created: fmt_time(st, Some(x.created)),
                    label: x.label,
                    revision: x.revision,
                    description: x.description,
                })
                .collect(),
            errors,
            alias,
        }),
        Err(e) => error_part(&e),
    }
}

async fn snapshots(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(alias): Path<String>,
) -> Response {
    snapshots_part(&st, &u, alias, vec![]).await
}

#[derive(Deserialize)]
struct SnapForm {
    label: String,
    #[serde(default)]
    description: String,
}

async fn snapshot_create(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(alias): Path<String>,
    Form(f): Form<SnapForm>,
) -> Response {
    let req = crate::ops::SnapshotReq {
        label: f.label.trim().to_string(),
        description: f.description,
        at: None,
    };
    let errors = match crate::ops::snapshot_create(
        State(st.clone()),
        u.caller(),
        Path(alias.clone()),
        Json(req),
    )
    .await
    {
        Ok(_) => vec![],
        Err(e) => vec![e.message().to_string()],
    };
    snapshots_part(&st, &u, alias, errors).await
}

async fn snapshot_drop(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path((alias, label)): Path<(String, String)>,
) -> Response {
    let errors = match crate::ops::snapshot_drop(
        State(st.clone()),
        u.caller(),
        Path((alias.clone(), label)),
    )
    .await
    {
        Ok(_) => vec![],
        Err(e) => vec![e.message().to_string()],
    };
    snapshots_part(&st, &u, alias, errors).await
}

// ---- history, diff, rollback -------------------------------------------------------

struct RevRow {
    id: i64,
    created: String,
    author: String,
    message: String,
    labels: String,
    head: bool,
}

#[derive(Template)]
#[template(path = "history.html")]
struct HistoryPart {
    base: &'static str,
    alias: String,
    can_publish: bool,
    revisions: Vec<RevRow>,
}

async fn history(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(alias): Path<String>,
) -> Response {
    let q = crate::ops::HistoryQuery { limit: Some(50) };
    match crate::ops::history(State(st.clone()), u.caller(), Path(alias.clone()), Query(q)).await {
        Ok(Json(list)) => render(&HistoryPart {
            base: base(),
            can_publish: u.can(Role::Publisher),
            revisions: list
                .into_iter()
                .map(|r| RevRow {
                    id: r.id,
                    created: fmt_time(&st, Some(r.created)),
                    author: r.author,
                    message: r.message,
                    labels: r.labels.join(", "),
                    head: r.head,
                })
                .collect(),
            alias,
        }),
        Err(e) => error_part(&e),
    }
}

#[derive(Template)]
#[template(path = "diff.html")]
struct DiffPart {
    from: String,
    to: String,
    entries: Vec<String>,
}

#[derive(Deserialize)]
struct DiffForm {
    from: String,
    #[serde(default)]
    to: Option<String>,
}

async fn diff(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(alias): Path<String>,
    Query(f): Query<DiffForm>,
) -> Response {
    let q = crate::ops::DiffQuery {
        from: f.from.clone(),
        to: f.to.clone(),
    };
    match crate::ops::diff(State(st.clone()), u.caller(), Path(alias), Query(q)).await {
        Ok(Json(list)) => render(&DiffPart {
            from: f.from,
            to: f.to.unwrap_or_else(|| "HEAD".into()),
            entries: list
                .into_iter()
                .map(|e| {
                    format!(
                        "{} {}/{} {} {} {}",
                        e.change,
                        e.dist,
                        e.component,
                        e.package.name,
                        e.package.version,
                        e.package.arch
                    )
                })
                .collect(),
        }),
        Err(e) => error_part(&e),
    }
}

#[derive(Deserialize)]
struct RollbackForm {
    to: String,
    #[serde(default)]
    apply: String,
    #[serde(default)]
    expect: String,
}

async fn rollback(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(alias): Path<String>,
    Form(f): Form<RollbackForm>,
) -> Response {
    let apply = f.apply == "1";
    let res: Result<Outcome, ApiError> = async {
        let req = crate::ops::RollbackReq {
            to: f.to.clone(),
            apply,
            expect: if apply {
                parse_expect(&f.expect)?
            } else {
                None
            },
        };
        let Json(out) = crate::ops::rollback(
            State(st.clone()),
            u.caller(),
            Path(alias.clone()),
            Json(req),
        )
        .await?;
        Ok(out)
    }
    .await;
    match res {
        Ok(out) => outcome(
            &out,
            format!("{}p/{alias}/rollback", base()),
            vec![("to".into(), f.to)],
            apply,
        ),
        Err(e) => error_part(&e),
    }
}

// ---- layout ------------------------------------------------------------------------

#[derive(Deserialize)]
struct LayoutForm {
    layout: String,
    #[serde(default)]
    force: String,
    #[serde(default)]
    adopt: String,
    #[serde(default)]
    apply: String,
    #[serde(default)]
    expect: String,
}

async fn layout(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(alias): Path<String>,
    Form(f): Form<LayoutForm>,
) -> Response {
    let apply = f.apply == "1";
    let res: Result<Outcome, ApiError> = async {
        let layout: aptpod_repo::Layout =
            toml::from_str(&f.layout).map_err(|e| ApiError::invalid(format!("layout: {e}")))?;
        let req = crate::ops::LayoutReq {
            layout,
            force: f.force == "1",
            adopt: f.adopt == "1",
            apply,
            expect: if apply {
                parse_expect(&f.expect)?
            } else {
                None
            },
        };
        let Json(out) = crate::ops::set_layout(
            State(st.clone()),
            u.caller(),
            Path(alias.clone()),
            Json(req),
        )
        .await?;
        Ok(out)
    }
    .await;
    match res {
        Ok(out) => outcome(
            &out,
            format!("{}p/{alias}/layout", base()),
            vec![
                ("layout".into(), f.layout),
                ("force".into(), f.force),
                ("adopt".into(), f.adopt),
            ],
            apply,
        ),
        Err(e) => error_part(&e),
    }
}

#[derive(Deserialize)]
struct CreateForm {
    alias: String,
    layout: String,
    #[serde(default)]
    adopt: String,
    #[serde(default)]
    apply: String,
    #[serde(default)]
    expect: String,
}

async fn create(State(st): State<Arc<AppState>>, u: UiUser, Form(f): Form<CreateForm>) -> Response {
    let apply = f.apply == "1";
    let res: Result<Outcome, ApiError> = async {
        let layout: aptpod_repo::Layout =
            toml::from_str(&f.layout).map_err(|e| ApiError::invalid(format!("layout: {e}")))?;
        let req = crate::api::CreateReq {
            alias: f.alias.trim().to_string(),
            layout,
            adopt: f.adopt == "1",
            apply,
            expect: if apply {
                parse_expect(&f.expect)?
            } else {
                None
            },
        };
        let Json(out) =
            crate::api::create_publish(State(st.clone()), u.caller(), Json(req)).await?;
        Ok(out)
    }
    .await;
    match res {
        Ok(out) => outcome(
            &out,
            format!("{}publishes/new", base()),
            vec![
                ("alias".into(), f.alias),
                ("layout".into(), f.layout),
                ("adopt".into(), f.adopt),
            ],
            apply,
        ),
        Err(e) => error_part(&e),
    }
}
