// SPDX-License-Identifier: GPL-3.0-only

//! Web UI: server-rendered pages (askama, escaped by default) with htmx.
//!
//! Sessions are cookies stored hashed in server.db, re-read with the
//! user's current role on every request; every non-GET request needs the
//! session's CSRF token in `X-CSRF-Token` (htmx sends it from the body's
//! `hx-headers`). The session cookie authenticates the UI only, never
//! `/api/v1`. Handlers call the API handlers directly, so permissions,
//! previews, stale checks and config-file rules are the API's.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Form, FromRequestParts, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header, request::Parts};
use axum::middleware::map_response;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::api::{ApiError, AppState, Caller, Outcome};
use crate::authdb::{Role, User, random_hex};

const SESSION: &str = "aptpod_session";
const LOGIN_CSRF: &str = "aptpod_login";
const HTMX: &[u8] = include_bytes!("../../assets/htmx.min.js");
const CSS: &[u8] = include_bytes!("../../assets/style.css");

/// Every link goes through here, so a sub-path deployment changes one place.
pub fn base() -> &'static str {
    "/"
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(index))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/jobs", get(jobs_page))
        .route("/jobs/{id}", get(job_page))
        .route("/p/{alias}", get(publish_page))
        .route("/p/{alias}/packages", get(packages_part))
        .route("/p/{alias}/rm", post(rm))
        .route(
            "/ui/htmx-2.0.11.min.js",
            get(|| asset(HTMX, "text/javascript")),
        )
        .route("/ui/style.css", get(|| asset(CSS, "text/css")))
        .layer(map_response(security_headers))
}

async fn security_headers(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    r
}

async fn asset(body: &'static [u8], ty: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, ty),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        body,
    )
        .into_response()
}

// ---- sessions ------------------------------------------------------------

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn set_cookie(
    st: &AppState,
    name: &str,
    value: &str,
    path: &str,
    max_age: i64,
    same_site: &str,
) -> HeaderValue {
    let secure = if st.cfg.cookie_secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite={same_site}{secure}"
    ))
    .expect("cookie header")
}

/// Common page data: who is logged in, the CSRF token, the base path.
pub struct Shell {
    pub user: String,
    pub role: &'static str,
    pub csrf: String,
    pub base: &'static str,
}

/// A logged-in UI user. Non-GET requests are rejected without the
/// session's CSRF token.
pub struct UiUser {
    pub user: User,
    pub csrf: String,
}

impl UiUser {
    fn caller(&self) -> Caller {
        Caller::from_session(self.user.clone())
    }
    fn shell(&self) -> Shell {
        Shell {
            user: self.user.name.clone(),
            role: self.user.role.as_str(),
            csrf: self.csrf.clone(),
            base: base(),
        }
    }
    fn can(&self, role: Role) -> bool {
        self.user.role >= role
    }
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers.get("hx-request").is_some()
}

/// To the login page: a redirect, or an HX-Redirect for htmx requests.
fn to_login(headers: &HeaderMap) -> Response {
    let login = format!("{}login", base());
    if is_htmx(headers) {
        ([("hx-redirect", login)], "").into_response()
    } else {
        (StatusCode::SEE_OTHER, [(header::LOCATION, login)]).into_response()
    }
}

impl FromRequestParts<Arc<AppState>> for UiUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, st: &Arc<AppState>) -> Result<Self, Response> {
        let Some((user, csrf)) =
            cookie(&parts.headers, SESSION).and_then(|id| st.auth.session(&id).ok().flatten())
        else {
            return Err(to_login(&parts.headers));
        };
        if parts.method != Method::GET && parts.method != Method::HEAD {
            let sent = parts
                .headers
                .get("x-csrf-token")
                .and_then(|v| v.to_str().ok());
            if sent != Some(csrf.as_str()) {
                return Err((StatusCode::FORBIDDEN, "missing or wrong CSRF token").into_response());
            }
        }
        Ok(UiUser { user, csrf })
    }
}

fn render<T: Template>(t: &T) -> Response {
    match t.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("rendering: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "rendering failed").into_response()
        }
    }
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPart<'a> {
    message: &'a str,
}

/// API errors as an inline box (200, so htmx swaps it in).
fn error_part(e: &ApiError) -> Response {
    render(&ErrorPart {
        message: e.message(),
    })
}

fn fmt_time(st: &AppState, t: Option<i64>) -> String {
    t.and_then(|t| jiff::Timestamp::from_second(t).ok())
        .map(|ts| {
            ts.to_zoned_datetime(st.cfg.tz())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn state_class(state: &str) -> &'static str {
    match state {
        "done" | "synced" => "ok",
        "failed" => "bad",
        "interrupted" | "running" | "syncing" => "warn",
        _ => "",
    }
}

// ---- login ---------------------------------------------------------------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    s: Shell,
    login_csrf: String,
    error: Option<String>,
}

fn login_form(st: &AppState, error: Option<String>, status: StatusCode) -> Response {
    // Double-submit token for the login form itself (no session yet).
    let token = random_hex(16);
    let page = LoginPage {
        s: Shell {
            user: String::new(),
            role: "",
            csrf: String::new(),
            base: base(),
        },
        login_csrf: token.clone(),
        error,
    };
    let mut r = render(&page);
    *r.status_mut() = status;
    r.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(st, LOGIN_CSRF, &token, "/login", 600, "Strict"),
    );
    r
}

async fn login_page(State(st): State<Arc<AppState>>) -> Response {
    login_form(&st, None, StatusCode::OK)
}

#[derive(Deserialize)]
struct LoginForm {
    user: String,
    password: String,
    csrf: String,
}

async fn login(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    if cookie(&headers, LOGIN_CSRF).as_deref() != Some(f.csrf.as_str()) || f.csrf.is_empty() {
        return login_form(
            &st,
            Some("The form expired; try again.".into()),
            StatusCode::FORBIDDEN,
        );
    }
    let checked = {
        let Ok(_permit) = st.logins.acquire().await else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let st2 = st.clone();
        tokio::task::spawn_blocking(move || st2.auth.check_password(&f.user, &f.password)).await
    };
    let user = match checked {
        Ok(Ok(Some(u))) => u,
        Ok(Ok(None)) => {
            return login_form(
                &st,
                Some("Wrong user name or password.".into()),
                StatusCode::UNAUTHORIZED,
            );
        }
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let ttl = i64::from(st.cfg.session_hours.max(1)) * 3600;
    let Ok((id, _csrf)) = st.auth.session_create(user.id, ttl) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    tracing::info!(user = %user.name, "web login");
    let mut r = (StatusCode::SEE_OTHER, [(header::LOCATION, base())]).into_response();
    r.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(&st, SESSION, &id, "/", ttl, "Lax"),
    );
    r.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(&st, LOGIN_CSRF, "", "/login", 0, "Strict"),
    );
    r
}

async fn logout(State(st): State<Arc<AppState>>, headers: HeaderMap, _u: UiUser) -> Response {
    if let Some(id) = cookie(&headers, SESSION) {
        let _ = st.auth.session_delete(&id);
    }
    let mut r = to_login(&headers);
    r.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(&st, SESSION, "", "/", 0, "Lax"),
    );
    r
}

// ---- overview, jobs -------------------------------------------------------

struct PublishRow {
    alias: String,
    path: String,
    dists: String,
    components: String,
    packages: usize,
    managed: bool,
}

struct MirrorRow {
    name: String,
    kind: String,
    state: String,
    state_class: &'static str,
    finished: String,
    next_run: String,
}

struct JobRow {
    id: i64,
    created: String,
    state: String,
    state_class: &'static str,
    kind: String,
    resource: String,
    user: String,
    text: String,
}

fn job_rows(st: &AppState, limit: usize) -> Result<Vec<JobRow>, ApiError> {
    Ok(st
        .auth
        .jobs(limit)?
        .into_iter()
        .map(|j| JobRow {
            id: j.id,
            created: fmt_time(st, Some(j.created)),
            state_class: state_class(&j.state),
            state: j.state,
            kind: j.kind,
            resource: j.resource,
            user: j.user,
            text: j.error.unwrap_or(j.summary),
        })
        .collect())
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    s: Shell,
    publishes: Vec<PublishRow>,
    mirrors: Vec<MirrorRow>,
    jobs: Vec<JobRow>,
}

async fn index(State(st): State<Arc<AppState>>, u: UiUser) -> Response {
    let res: Result<IndexPage, ApiError> = async {
        let list = crate::api::read(&st, |r| {
            r.publishes()?
                .into_iter()
                .map(|(alias, l)| {
                    let n: usize = r.status(&alias)?.iter().map(|c| c.packages).sum();
                    Ok((alias, l, n))
                })
                .collect::<Result<Vec<_>, aptpod_repo::Error>>()
        })
        .await?;
        let publishes = list
            .into_iter()
            .map(|(alias, l, packages)| PublishRow {
                managed: st.config_file.is_publish(&alias),
                path: l.path.display().to_string(),
                dists: l.dists.join(", "),
                components: l.components.join(", "),
                packages,
                alias,
            })
            .collect();
        let mut mirrors = Vec::new();
        for (name, def) in st.auth.mirrors()? {
            let s = st.auth.mirror_status(&name)?;
            let state = if st.mirror_runner.running(&name).is_some() {
                "syncing".to_string()
            } else {
                s.state.clone().unwrap_or_else(|| "never synced".into())
            };
            mirrors.push(MirrorRow {
                kind: serde_json::to_value(def.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default(),
                state_class: state_class(&state),
                state,
                finished: fmt_time(&st, s.finished),
                next_run: crate::schedule::next_run(&st, &def).unwrap_or_default(),
                name,
            });
        }
        Ok(IndexPage {
            s: u.shell(),
            publishes,
            mirrors,
            jobs: job_rows(&st, 10)?,
        })
    }
    .await;
    match res {
        Ok(p) => render(&p),
        Err(e) => error_part(&e),
    }
}

#[derive(Template)]
#[template(path = "jobs.html")]
struct JobsPage {
    s: Shell,
    jobs: Vec<JobRow>,
}

async fn jobs_page(State(st): State<Arc<AppState>>, u: UiUser) -> Response {
    match job_rows(&st, 200) {
        Ok(jobs) => render(&JobsPage { s: u.shell(), jobs }),
        Err(e) => error_part(&e),
    }
}

#[derive(Template)]
#[template(path = "job.html")]
struct JobPage {
    s: Shell,
    id: i64,
    state: String,
    state_class: &'static str,
    kind: String,
    resource: String,
    user: String,
    created: String,
    summary: String,
    error: Option<String>,
    log: String,
}

async fn job_page(State(st): State<Arc<AppState>>, u: UiUser, Path(id): Path<i64>) -> Response {
    let Ok(Some(j)) = st.auth.job(id) else {
        return (StatusCode::NOT_FOUND, "no such job").into_response();
    };
    let log = std::fs::read(crate::runner::log_path(&st, id))
        .map(|b| String::from_utf8_lossy(&b[b.len().saturating_sub(1 << 20)..]).into_owned())
        .unwrap_or_default();
    render(&JobPage {
        s: u.shell(),
        id,
        state_class: state_class(&j.state),
        state: j.state,
        kind: j.kind,
        resource: j.resource,
        user: j.user,
        created: fmt_time(&st, Some(j.created)),
        summary: j.summary,
        error: j.error,
        log,
    })
}

// ---- a publish -----------------------------------------------------------

struct MatrixRow {
    dist: String,
    counts: Vec<String>,
}

#[derive(Template)]
#[template(path = "publish.html")]
struct PublishPage {
    s: Shell,
    alias: String,
    path: String,
    managed: bool,
    dists: Vec<String>,
    components: Vec<String>,
    matrix: Vec<MatrixRow>,
    can_publish: bool,
}

async fn publish_page(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(alias): Path<String>,
) -> Response {
    let a = alias.clone();
    let got = crate::api::read(&st, move |r| Ok((r.layout(&a)?, r.status(&a)?))).await;
    let (layout, cells) = match got {
        Ok(x) => x,
        Err(e) => return error_part(&e),
    };
    let matrix = layout
        .dists
        .iter()
        .map(|d| MatrixRow {
            dist: d.clone(),
            counts: layout
                .components
                .iter()
                .map(|c| {
                    cells
                        .iter()
                        .find(|x| &x.dist == d && &x.component == c)
                        .map_or("–".to_string(), |x| x.packages.to_string())
                })
                .collect(),
        })
        .collect();
    render(&PublishPage {
        s: u.shell(),
        managed: st.config_file.is_publish(&alias),
        path: layout.path.display().to_string(),
        dists: layout.dists.clone(),
        components: layout.components.clone(),
        matrix,
        can_publish: u.can(Role::Publisher),
        alias,
    })
}

#[derive(Deserialize)]
struct PackagesQ {
    #[serde(default)]
    q: String,
    #[serde(default)]
    dist: String,
    #[serde(default)]
    component: String,
}

struct PkgRow {
    dist: String,
    component: String,
    name: String,
    version: String,
    arch: String,
}

#[derive(Template)]
#[template(path = "packages.html")]
struct PackagesPart {
    rows: Vec<PkgRow>,
    total: usize,
    truncated: bool,
}

const MAX_ROWS: usize = 500;

async fn packages_part(
    State(st): State<Arc<AppState>>,
    _u: UiUser,
    Path(alias): Path<String>,
    Query(q): Query<PackagesQ>,
) -> Response {
    let res = crate::api::read(&st, move |r| {
        let specs: Vec<&str> = q.q.split_whitespace().collect();
        let filter = aptpod_repo::Filter::new(&specs, None, None)?;
        let dists = (!q.dist.is_empty()).then(|| vec![q.dist.clone()]);
        let comps = (!q.component.is_empty()).then(|| vec![q.component.clone()]);
        r.list(&alias, comps.as_deref(), dists.as_deref(), &filter)
    })
    .await;
    match res {
        Ok(rows) => {
            let total = rows.len();
            render(&PackagesPart {
                truncated: total > MAX_ROWS,
                total,
                rows: rows
                    .into_iter()
                    .take(MAX_ROWS)
                    .map(|r| PkgRow {
                        dist: r.dist,
                        component: r.component,
                        name: r.package.name,
                        version: r.package.version,
                        arch: r.package.arch,
                    })
                    .collect(),
            })
        }
        Err(e) => error_part(&e),
    }
}

// ---- preview and apply ------------------------------------------------------

#[derive(Template)]
#[template(path = "plan.html")]
struct PlanPart {
    lines: Vec<String>,
    warnings: Vec<String>,
    empty: bool,
    destructive: bool,
    action: String,
    /// The request's own fields, sent back with `apply`.
    fields: Vec<(String, String)>,
    /// The previewed heads as JSON, sent back as `expect`.
    expect: String,
}

#[derive(Template)]
#[template(path = "applied.html")]
struct AppliedPart {
    s_base: &'static str,
    job: Option<i64>,
    lines: Vec<String>,
}

/// The preview of `out`, or what applying did; applied changes tell the
/// page to refresh its lists (`packages-changed`).
fn outcome(
    out: &Outcome,
    action: String,
    fields: Vec<(String, String)>,
    applied: bool,
) -> Response {
    if applied {
        let mut r = render(&AppliedPart {
            s_base: base(),
            job: out.job,
            lines: out.plan.lines.clone(),
        });
        r.headers_mut()
            .insert("hx-trigger", HeaderValue::from_static("packages-changed"));
        return r;
    }
    render(&PlanPart {
        lines: out.plan.lines.clone(),
        warnings: out.plan.warnings.clone(),
        empty: out.plan.empty,
        destructive: out.plan.destructive,
        action,
        fields,
        expect: serde_json::to_string(&out.plan.bases).unwrap_or_default(),
    })
}

fn parse_expect(s: &str) -> Result<crate::api::Bases, ApiError> {
    if s.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(s)
        .map(Some)
        .map_err(|e| ApiError::invalid(format!("bad preview data: {e}")))
}

#[derive(Deserialize)]
struct RmForm {
    component: String,
    #[serde(default)]
    packages: String,
    #[serde(default)]
    dists: String,
    #[serde(default)]
    keep: String,
    #[serde(default)]
    apply: String,
    #[serde(default)]
    expect: String,
}

async fn rm(
    State(st): State<Arc<AppState>>,
    u: UiUser,
    Path(alias): Path<String>,
    Form(f): Form<RmForm>,
) -> Response {
    let apply = f.apply == "1";
    let res: Result<Outcome, ApiError> = async {
        let keep = match f.keep.trim() {
            "" => None,
            k => Some(
                k.parse()
                    .map_err(|_| ApiError::invalid("keep must be a number"))?,
            ),
        };
        let dists: Vec<String> = f
            .dists
            .split([',', ' '])
            .filter(|d| !d.is_empty())
            .map(String::from)
            .collect();
        let req = crate::ops::RmReq {
            component: f.component.clone(),
            packages: f.packages.split_whitespace().map(String::from).collect(),
            all: false,
            versions: None,
            arch: None,
            dists: (!dists.is_empty()).then_some(dists),
            keep,
            apply,
            expect: if apply {
                parse_expect(&f.expect)?
            } else {
                None
            },
        };
        let Json(out) = crate::ops::rm(
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
            format!("{}p/{alias}/rm", base()),
            vec![
                ("component".into(), f.component),
                ("packages".into(), f.packages),
                ("dists".into(), f.dists),
                ("keep".into(), f.keep),
            ],
            apply,
        ),
        Err(e) => error_part(&e),
    }
}
