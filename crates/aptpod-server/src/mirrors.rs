// SPDX-License-Identifier: GPL-3.0-only

//! Mirror definitions (server configuration in `server.db`) and their last
//! sync status.

use std::path::{Path, PathBuf};

use aptpod_mirror::http::HttpMirror;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::authdb::AuthDb;
use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Verbatim mirror over HTTP(S), upstream signature kept.
    Http,
}

fn http() -> Kind {
    Kind::Http
}
fn yes() -> bool {
    true
}
fn is_false(b: &bool) -> bool {
    !b
}
fn four() -> usize {
    4
}
fn three() -> u32 {
    3
}
fn max_delete() -> usize {
    10_000
}

/// What a mirror copies from where to where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorDef {
    #[serde(default = "http")]
    pub kind: Kind,
    /// Archive root (the directory holding `dists/` and `pool/`).
    pub url: String,
    /// Where the mirrored tree is written; under `mirror_roots`.
    pub path: PathBuf,
    pub dists: Vec<String>,
    /// Empty: all the dist's Release lists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub architectures: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub udebs: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub sources: bool,
    /// More index files to keep (globs relative to `dists/<dist>/`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extras: Vec<String>,
    /// Upstream public key(s), armored. Required unless `insecure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyring: Option<String>,
    /// Mirror without checking upstream signatures.
    #[serde(default, skip_serializing_if = "is_false")]
    pub insecure: bool,
    /// Pool paths upstream lists but does not serve (404/410), accepted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_missing: Vec<String>,
    #[serde(default = "four")]
    pub parallel: usize,
    #[serde(default = "three")]
    pub retries: u32,
    /// Delete pool files upstream dropped (off for an adopted tree until
    /// reviewed).
    #[serde(default = "yes")]
    pub prune: bool,
    /// Delete nothing when more files than this would go.
    #[serde(default = "max_delete")]
    pub max_delete: usize,
    /// Scheduled syncs run only when enabled.
    #[serde(default = "yes")]
    pub enabled: bool,
}

impl MirrorDef {
    /// The engine configuration; checks everything that does not need the
    /// network.
    pub fn engine(&self) -> Result<HttpMirror, String> {
        // Definitions are readable by every reader (config export): no
        // secrets in them.
        let authority = self
            .url
            .split_once("://")
            .map_or("", |(_, rest)| rest.split('/').next().unwrap_or(""));
        if authority.contains('@') {
            return Err("credentials in the URL are not supported".into());
        }
        let mut m = HttpMirror::new(&self.url, &[]);
        m.dists = self.dists.clone();
        m.components = self.components.clone();
        m.architectures = self.architectures.clone();
        m.udebs = self.udebs;
        m.sources = self.sources;
        m.extras = self.extras.clone();
        m.ignore_missing = self.ignore_missing.iter().cloned().collect();
        m.parallel = self.parallel.clamp(1, 64);
        m.retries = self.retries.min(20);
        m.prune = self.prune;
        m.max_delete = self.max_delete;
        m.keyring = match (&self.keyring, self.insecure) {
            (Some(_), true) => return Err("give either keyring or insecure, not both".into()),
            (None, false) => {
                return Err(
                    "a keyring (the upstream's public key) is required, or insecure = true".into(),
                );
            }
            (None, true) => None,
            (Some(k), false) => Some(
                aptpod_core::verify::Keyring::from_bytes(k.as_bytes())
                    .map_err(|e| format!("keyring: {e}"))?,
            ),
        };
        m.validate().map_err(|e| e.to_string())?;
        Ok(m)
    }

    pub fn check(&self, cfg: &Config) -> Result<(), String> {
        cfg.allows_mirror_path(&self.path)?;
        self.engine().map(|_| ())
    }
}

/// Last sync of a mirror.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub job: Option<i64>,
    pub started: Option<i64>,
    pub finished: Option<i64>,
    /// running, done, failed, interrupted
    pub state: Option<String>,
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<serde_json::Value>,
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

type R<T> = Result<T, rusqlite::Error>;

fn parse_def(name: &str, s: &str) -> R<MirrorDef> {
    serde_json::from_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            format!("mirror {name}: {e}").into(),
        )
    })
}

impl AuthDb {
    pub fn mirrors(&self) -> R<Vec<(String, MirrorDef)>> {
        let c = self.conn();
        let mut stmt = c.prepare("SELECT name, def FROM mirrors ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|r| r.and_then(|(n, d)| parse_def(&n, &d).map(|d| (n, d))))
            .collect()
    }

    pub fn mirror(&self, name: &str) -> R<Option<MirrorDef>> {
        let d: Option<String> = self
            .conn()
            .query_row("SELECT def FROM mirrors WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .optional()?;
        d.map(|d| parse_def(name, &d)).transpose()
    }

    pub fn mirror_put(&self, name: &str, def: &MirrorDef) -> R<()> {
        let json = serde_json::to_string(def).expect("serializable");
        let t = now();
        self.conn().execute(
            "INSERT INTO mirrors (name, def, created, updated) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(name) DO UPDATE SET def = excluded.def, updated = excluded.updated",
            params![name, json, t],
        )?;
        Ok(())
    }

    pub fn mirror_del(&self, name: &str) -> R<bool> {
        let c = self.conn();
        c.execute("DELETE FROM mirror_status WHERE name = ?1", [name])?;
        Ok(c.execute("DELETE FROM mirrors WHERE name = ?1", [name])? > 0)
    }

    pub fn mirror_status(&self, name: &str) -> R<Status> {
        Ok(self
            .conn()
            .query_row(
                "SELECT job, started, finished, state, summary, report FROM mirror_status WHERE name = ?1",
                [name],
                |r| {
                    Ok(Status {
                        job: r.get(0)?,
                        started: r.get(1)?,
                        finished: r.get(2)?,
                        state: r.get(3)?,
                        summary: r.get(4)?,
                        report: r
                            .get::<_, Option<String>>(5)?
                            .and_then(|s| serde_json::from_str(&s).ok()),
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    pub fn mirror_started(&self, name: &str, job: i64) -> R<()> {
        self.conn().execute(
            "INSERT INTO mirror_status (name, job, started, finished, state, summary, report)
             VALUES (?1, ?2, ?3, NULL, 'running', NULL, NULL)
             ON CONFLICT(name) DO UPDATE SET job = ?2, started = ?3, finished = NULL,
               state = 'running', summary = NULL, report = NULL",
            params![name, job, now()],
        )?;
        Ok(())
    }

    pub fn mirror_finished(
        &self,
        name: &str,
        state: &str,
        summary: &str,
        report: Option<&serde_json::Value>,
    ) -> R<()> {
        self.conn().execute(
            "UPDATE mirror_status SET finished = ?2, state = ?3, summary = ?4, report = ?5 WHERE name = ?1",
            params![name, now(), state, summary, report.map(|r| r.to_string())],
        )?;
        Ok(())
    }

    /// Syncs left running by a previous process.
    pub fn mirrors_interrupted(&self) -> R<usize> {
        self.conn().execute(
            "UPDATE mirror_status SET state = 'interrupted', finished = ?1 WHERE state = 'running'",
            [now()],
        )
    }
}

/// Whether `path` is missing or an empty directory.
pub fn is_empty_dir(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Ok(mut rd) => rd.next().is_none(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defs_round_trip_and_validate() {
        let toml_def = r#"
            url = "http://deb.example.org/debian"
            path = "/srv/mirror/debian"
            dists = ["stable"]
            insecure = true
        "#;
        let d: MirrorDef = toml::from_str(toml_def).unwrap();
        assert_eq!(
            (d.kind, d.parallel, d.prune, d.enabled),
            (Kind::Http, 4, true, true)
        );
        let m = d.engine().unwrap();
        assert!(m.url.ends_with('/'));
        let mut no_key = d.clone();
        no_key.insecure = false;
        assert!(no_key.engine().unwrap_err().contains("keyring"));
        let mut bad = d.clone();
        bad.url = "ftp://x/".into();
        assert!(bad.engine().is_err());
        bad.url = "https://user:secret@deb.example.org/debian/".into();
        assert!(bad.engine().unwrap_err().contains("credentials"));
        assert!(toml::from_str::<MirrorDef>(&format!("{toml_def}\nbogus = 1")).is_err());

        let dir = tempfile::tempdir().unwrap();
        let db = AuthDb::open(dir.path()).unwrap();
        db.mirror_put("debian", &d).unwrap();
        assert_eq!(db.mirror("debian").unwrap().unwrap(), d);
        db.mirror_started("debian", 7).unwrap();
        assert_eq!(
            db.mirror_status("debian").unwrap().state.as_deref(),
            Some("running")
        );
        assert_eq!(db.mirrors_interrupted().unwrap(), 1);
        db.mirror_finished("debian", "done", "ok", None).unwrap();
        assert_eq!(
            db.mirror_status("debian").unwrap().summary.as_deref(),
            Some("ok")
        );
        assert!(db.mirror_del("debian").unwrap());
        assert!(db.mirrors().unwrap().is_empty());
    }
}
