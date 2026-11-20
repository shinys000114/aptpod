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
fn default_upload_limit() -> u64 {
    2 << 30
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        for root in &cfg.publish_roots {
            if !is_clean_absolute(root) {
                return Err(format!(
                    "{}: publish_roots entry {} must be an absolute path without `..`",
                    path.display(),
                    root.display()
                ));
            }
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

    /// Whether `path` may hold a publish.
    pub fn allows_publish_path(&self, path: &Path) -> Result<(), String> {
        if !is_clean_absolute(path) {
            return Err(format!(
                "publish path {} must be absolute, without `.` or `..`",
                path.display()
            ));
        }
        if self.publish_roots.is_empty() {
            return Err("no publish_roots are configured in server.toml".into());
        }
        if self
            .publish_roots
            .iter()
            .any(|r| path.starts_with(r) && path != r.as_path())
        {
            Ok(())
        } else {
            Err(format!(
                "publish path {} is not under a configured publish root ({})",
                path.display(),
                self.publish_roots
                    .iter()
                    .map(|r| r.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    }
}

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
