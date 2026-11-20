// SPDX-License-Identifier: GPL-3.0-only

//! The optional config file (`config_file` in server.toml): publishes and
//! mirrors declared in a file, applied when the server starts and on
//! SIGHUP (`systemctl reload aptpod-server`) with the rules of `aptpod
//! config import` (destructive changes refused unless `force`; items
//! missing from the file are left alone). What the file declares is
//! *managed*: the API refuses to edit it, edits go through the file.
//! Everything else stays editable through the CLI, API and web UI.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard};

use aptpod_repo::Layout;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, AppState, Commit, Outcome};
use crate::mirrors::MirrorDef;
use crate::ops::ConfigView;

/// The file: the `aptpod config export` format plus apply options.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// Take over non-empty paths of new publishes and mirrors.
    #[serde(default)]
    pub adopt: bool,
    /// Allow changes that drop cells or architectures holding packages.
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub publishes: BTreeMap<String, Layout>,
    #[serde(default)]
    pub mirrors: BTreeMap<String, MirrorDef>,
}

impl FileConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Managed {
    pub publishes: BTreeSet<String>,
    pub mirrors: BTreeSet<String>,
}

/// The last application of the file.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub path: Option<PathBuf>,
    /// Unix time of the last attempt.
    pub applied: Option<i64>,
    pub ok: Option<bool>,
    pub error: Option<String>,
    /// What the last application changed.
    pub changes: Vec<String>,
    pub warnings: Vec<String>,
    pub publishes: Vec<String>,
    pub mirrors: Vec<String>,
}

#[derive(Default)]
pub struct FileState {
    managed: RwLock<Managed>,
    status: RwLock<Status>,
    /// One application at a time (startup, SIGHUP, API).
    lock: tokio::sync::Mutex<()>,
}

impl FileState {
    pub fn managed(&self) -> RwLockReadGuard<'_, Managed> {
        self.managed.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn status(&self) -> Status {
        self.status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_publish(&self, alias: &str) -> bool {
        self.managed().publishes.contains(alias)
    }

    pub fn is_mirror(&self, name: &str) -> bool {
        self.managed().mirrors.contains(name)
    }
}

/// Refuses an API edit of something the config file manages.
pub fn refuse(st: &AppState, what: &str, name: &str) -> Result<(), ApiError> {
    let managed = match what {
        "publish" => st.config_file.is_publish(name),
        _ => st.config_file.is_mirror(name),
    };
    if managed {
        let path = st
            .cfg
            .config_file
            .as_deref()
            .map_or("the config file".to_string(), |p| p.display().to_string());
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "managed",
            format!("{what} {name:?} is managed by {path}; edit it there and reload the server"),
        ));
    }
    Ok(())
}

/// Reads the config file and applies it (`apply`), or only computes what
/// applying would change. Applying updates the managed set even when the
/// changes are refused; a file that cannot be read leaves it as it was.
pub async fn run(st: &Arc<AppState>, apply: bool) -> Result<Outcome, ApiError> {
    let Some(path) = st.cfg.config_file.clone() else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no config_file is set in server.toml",
        ));
    };
    let _one = st.config_file.lock.lock().await;
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let record =
        |ok: bool, error: Option<String>, out: Option<&Outcome>, names: Option<&Managed>| {
            if !apply {
                return;
            }
            let mut s = st
                .config_file
                .status
                .write()
                .unwrap_or_else(|e| e.into_inner());
            s.path = Some(path.clone());
            s.applied = Some(now);
            s.ok = Some(ok);
            s.error = error;
            s.changes = out.map(|o| o.plan.lines.clone()).unwrap_or_default();
            s.warnings = out.map(|o| o.plan.warnings.clone()).unwrap_or_default();
            if let Some(n) = names {
                s.publishes = n.publishes.iter().cloned().collect();
                s.mirrors = n.mirrors.iter().cloned().collect();
            }
        };
    let file = match tokio::task::spawn_blocking({
        let p = path.clone();
        move || FileConfig::load(&p)
    })
    .await
    .map_err(crate::api::join_err)?
    {
        Ok(f) => f,
        Err(e) => {
            record(false, Some(e.clone()), None, None);
            return Err(ApiError::invalid(e));
        }
    };
    let names = Managed {
        publishes: file.publishes.keys().cloned().collect(),
        mirrors: file.mirrors.keys().cloned().collect(),
    };
    if apply {
        let old = std::mem::replace(
            &mut *st
                .config_file
                .managed
                .write()
                .unwrap_or_else(|e| e.into_inner()),
            names.clone(),
        );
        for a in old.publishes.difference(&names.publishes) {
            tracing::warn!("publish {a:?} left the config file; it is now edited through the API");
        }
        for m in old.mirrors.difference(&names.mirrors) {
            tracing::warn!("mirror {m:?} left the config file; it is now edited through the API");
        }
    }
    let res = crate::ops::import(
        st,
        "config-file".into(),
        ConfigView {
            publishes: file.publishes,
            mirrors: file.mirrors,
        },
        file.force,
        file.adopt,
        Commit {
            apply,
            expect: None,
        },
    )
    .await;
    match &res {
        Ok(out) => record(true, None, Some(out), Some(&names)),
        Err(e) => record(false, Some(e.message().to_string()), None, Some(&names)),
    }
    res
}
