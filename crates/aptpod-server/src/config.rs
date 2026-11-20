// SPDX-License-Identifier: GPL-3.0-only

//! `server.toml`: the few settings the server needs before it can open its
//! database. Everything else lives in the database.

use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Directories publishes may be written under. Publishes outside every
    /// root cannot be created or moved through the API.
    #[serde(default)]
    pub publish_roots: Vec<PathBuf>,
    /// Directories mirrors may be written under (usually another disk).
    #[serde(default)]
    pub mirror_roots: Vec<PathBuf>,
    /// Mirror syncs that may run at the same time.
    #[serde(default = "one")]
    pub mirror_workers: usize,
    /// Time zone of mirror schedules and status times (IANA name, e.g.
    /// `Asia/Seoul`, read from /usr/share/zoneinfo); default UTC.
    #[serde(default)]
    pub timezone: Option<String>,
    /// Public directory for mirror status files (`<name>.json`,
    /// `<name>.svg`, `index.json`), e.g. served next to the mirrors.
    #[serde(default)]
    pub status_dir: Option<PathBuf>,
    /// Largest accepted upload, in bytes.
    #[serde(default = "default_upload_limit")]
    pub upload_limit: u64,
    #[serde(default)]
    pub signing: Option<Signing>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signing {
    /// Key id or fingerprint.
    pub key: String,
    #[serde(default)]
    pub gpg: Option<PathBuf>,
    #[serde(default)]
    pub passphrase_file: Option<PathBuf>,
    #[serde(default)]
    pub homedir: Option<PathBuf>,
}

fn default_listen() -> String {
    "127.0.0.1:8090".into()
}
fn default_data_dir() -> PathBuf {
    "/var/lib/aptpod".into()
}
fn one() -> usize {
    1
}
fn default_upload_limit() -> u64 {
    2 << 30
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        for (what, roots) in [
            ("publish_roots", &cfg.publish_roots),
            ("mirror_roots", &cfg.mirror_roots),
        ] {
            for root in roots {
                if !is_clean_absolute(root) {
                    return Err(format!(
                        "{}: {what} entry {} must be an absolute path without `..`",
                        path.display(),
                        root.display()
                    ));
                }
            }
        }
        if let Some(tz) = &cfg.timezone {
            jiff::tz::TimeZone::get(tz)
                .map_err(|e| format!("{}: timezone {tz:?}: {e}", path.display()))?;
        }
        if let Some(d) = &cfg.status_dir
            && !is_clean_absolute(d)
        {
            return Err(format!(
                "{}: status_dir must be an absolute path",
                path.display()
            ));
        }
        if cfg.mirror_workers == 0 {
            return Err(format!(
                "{}: mirror_workers must be at least 1",
                path.display()
            ));
        }
        Ok(cfg)
    }

    pub fn signer(&self) -> Option<aptpod_core::sign::GpgSigner> {
        self.signing.as_ref().map(|s| {
            let mut g = aptpod_core::sign::GpgSigner::new(s.key.clone());
            if let Some(gpg) = &s.gpg {
                g.gpg = gpg.clone();
            }
            g.passphrase_file = s.passphrase_file.clone();
            g.homedir = s.homedir.clone();
            g
        })
    }

    pub fn tz(&self) -> jiff::tz::TimeZone {
        self.timezone
            .as_deref()
            .and_then(|t| jiff::tz::TimeZone::get(t).ok())
            .unwrap_or(jiff::tz::TimeZone::UTC)
    }

    /// Whether `path` may hold a publish.
    pub fn allows_publish_path(&self, path: &Path) -> Result<(), String> {
        allows(path, &self.publish_roots, "publish", "publish_roots")
    }

    /// Whether `path` may hold a mirror.
    pub fn allows_mirror_path(&self, path: &Path) -> Result<(), String> {
        allows(path, &self.mirror_roots, "mirror", "mirror_roots")
    }
}

fn allows(path: &Path, roots: &[PathBuf], what: &str, key: &str) -> Result<(), String> {
    if !is_clean_absolute(path) {
        return Err(format!(
            "{what} path {} must be absolute, without `.` or `..`",
            path.display()
        ));
    }
    if roots.is_empty() {
        return Err(format!("no {key} are configured in server.toml"));
    }
    if roots
        .iter()
        .any(|r| path.starts_with(r) && path != r.as_path())
    {
        Ok(())
    } else {
        Err(format!(
            "{what} path {} is not under a configured {what} root ({})",
            path.display(),
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

impl Config {}

fn is_clean_absolute(p: &Path) -> bool {
    p.is_absolute()
        && p.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_config_parses() {
        let cfg: Config =
            toml::from_str(include_str!("../../../packaging/server/server.toml")).unwrap();
        assert_eq!(cfg.data_dir, Path::new("/var/lib/aptpod"));
        assert_eq!(cfg.upload_limit, 2 << 30);
        assert!(cfg.signing.is_none());
    }

    #[test]
    fn publish_roots() {
        let mut cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:8090");
        assert!(cfg.allows_publish_path(Path::new("/srv/repo/x")).is_err());
        cfg.publish_roots = vec!["/srv/repo".into()];
        assert!(cfg.allows_publish_path(Path::new("/srv/repo/x")).is_ok());
        assert!(cfg.allows_publish_path(Path::new("/srv/repo")).is_err());
        assert!(
            cfg.allows_publish_path(Path::new("/srv/repo/../etc"))
                .is_err()
        );
        assert!(
            cfg.allows_publish_path(Path::new("/srv/repository"))
                .is_err()
        );
        assert!(cfg.allows_publish_path(Path::new("srv/repo/x")).is_err());
    }
}
