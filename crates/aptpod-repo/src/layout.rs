// SPDX-License-Identifier: GPL-3.0-only

//! A publish's layout: where it is written and which dist x component cells
//! it has, plus its Release metadata and retention settings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;

/// Layout of one publish (one tree on disk).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// Root of the published tree (`dists/`, `pool/`). aptpod only writes
    /// `pool/` and `dists/<dist>` there; other files are left alone.
    pub path: PathBuf,
    pub dists: Vec<String>,
    pub components: Vec<String>,
    /// Restricts a component to some dists.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub only: BTreeMap<String, Vec<String>>,
    pub architectures: Vec<String>,
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub not_automatic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub but_automatic_upgrades: bool,
    /// Sign Release files (`InRelease`, `Release.gpg`).
    #[serde(default = "yes")]
    pub sign: bool,
    /// Keep only the newest N versions of a package per cell and
    /// architecture after add/promote (0 = keep all).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub keep: u32,
    /// Revisions older than this (seconds) are forgotten unless labelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_max_age: Option<u64>,
}

fn yes() -> bool {
    true
}
fn is_false(b: &bool) -> bool {
    !b
}
fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// A (dist, component) pair.
pub type Cell = (String, String);

/// A dist, component or architecture name aptpod can publish.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-._~".contains(c))
}

/// Validates an alias (publish name).
pub fn check_alias(alias: &str) -> Result<(), Error> {
    if alias.is_empty()
        || !alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        return Err(Error::Invalid(format!(
            "invalid publish name {alias:?} (letters, digits, - and _)"
        )));
    }
    Ok(())
}

impl Layout {
    /// A layout with the given cells and defaults for the rest.
    pub fn new(
        path: impl Into<PathBuf>,
        dists: &[&str],
        components: &[&str],
        architectures: &[&str],
    ) -> Self {
        let v = |l: &[&str]| l.iter().map(|s| s.to_string()).collect();
        Self {
            path: path.into(),
            dists: v(dists),
            components: v(components),
            only: BTreeMap::new(),
            architectures: v(architectures),
            origin: String::new(),
            label: String::new(),
            description: None,
            not_automatic: false,
            but_automatic_upgrades: false,
            sign: true,
            keep: 0,
            history_max_age: None,
        }
    }

    pub fn validate(&self) -> Result<(), Error> {
        let mut errs = Vec::new();
        if !self.path.is_absolute() {
            errs.push(format!("path {} must be absolute", self.path.display()));
        }
        for (what, list) in [
            ("dists", &self.dists),
            ("components", &self.components),
            ("architectures", &self.architectures),
        ] {
            if list.is_empty() {
                errs.push(format!("{what} is empty"));
            }
            let mut seen = std::collections::BTreeSet::new();
            for v in list {
                if !valid_name(v) {
                    errs.push(format!("invalid name {v:?} in {what}"));
                }
                if !seen.insert(v) {
                    errs.push(format!("duplicate {v:?} in {what}"));
                }
            }
        }
        if self.architectures.iter().any(|a| a == "all") {
            errs.push(
                "'all' is not an architecture to publish; all-arch packages go into every one"
                    .into(),
            );
        }
        for (comp, dists) in &self.only {
            if !self.components.contains(comp) {
                errs.push(format!("only: {comp:?} is not a component"));
            }
            for d in dists {
                if !self.dists.contains(d) {
                    errs.push(format!("only.{comp}: {d:?} is not a dist"));
                }
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(Error::Invalid(errs.join("; ")))
        }
    }

    /// Whether (dist, component) is a cell of this layout.
    pub fn has_cell(&self, dist: &str, component: &str) -> bool {
        self.dists.iter().any(|d| d == dist)
            && self.components.iter().any(|c| c == component)
            && self
                .only
                .get(component)
                .is_none_or(|ds| ds.iter().any(|d| d == dist))
    }

    /// Every cell, dist-major in layout order.
    pub fn cells(&self) -> Vec<Cell> {
        let mut out = Vec::new();
        for d in &self.dists {
            for c in &self.components {
                if self.has_cell(d, c) {
                    out.push((d.clone(), c.clone()));
                }
            }
        }
        out
    }

    /// Components of a dist, in layout order (empty: the dist is not published).
    pub fn components_of(&self, dist: &str) -> Vec<String> {
        self.components
            .iter()
            .filter(|c| self.has_cell(dist, c))
            .cloned()
            .collect()
    }

    pub fn publishes_arch(&self, arch: &str) -> bool {
        arch == "all" || self.architectures.iter().any(|a| a == arch)
    }
}

/// Two publish trees must not be the same or nested in each other.
pub fn paths_overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_respect_only() {
        let mut l = Layout::new(
            "/srv/r",
            &["jammy", "noble"],
            &["main", "vendor"],
            &["arm64"],
        );
        l.only.insert("vendor".into(), vec!["noble".into()]);
        l.validate().unwrap();
        assert_eq!(
            l.cells(),
            vec![
                ("jammy".into(), "main".into()),
                ("noble".into(), "main".into()),
                ("noble".into(), "vendor".into())
            ]
        );
        assert!(!l.has_cell("jammy", "vendor"));
        assert_eq!(l.components_of("noble"), ["main", "vendor"]);
    }

    #[test]
    fn validation_errors() {
        let bad = |f: &dyn Fn(&mut Layout)| {
            let mut l = Layout::new("/srv/r", &["noble"], &["main"], &["arm64"]);
            f(&mut l);
            l.validate().unwrap_err().to_string()
        };
        assert!(bad(&|l| l.path = "rel".into()).contains("absolute"));
        assert!(bad(&|l| l.dists.clear()).contains("dists is empty"));
        assert!(bad(&|l| l.dists.push("noble".into())).contains("duplicate"));
        assert!(bad(&|l| l.architectures.push("all".into())).contains("'all'"));
        assert!(bad(&|l| l.components.push("../x".into())).contains("invalid name"));
        assert!(
            bad(&|l| {
                l.only.insert("nope".into(), vec![]);
            })
            .contains("not a component")
        );
        assert!(check_alias("bad name").is_err());
        assert!(check_alias("vendor-boards_2").is_ok());
    }

    #[test]
    fn overlap() {
        assert!(paths_overlap(Path::new("/srv/a"), Path::new("/srv/a/b")));
        assert!(paths_overlap(Path::new("/srv/a"), Path::new("/srv/a")));
        assert!(!paths_overlap(Path::new("/srv/a"), Path::new("/srv/ab")));
    }

    #[test]
    fn json_round_trip_and_defaults() {
        let l: Layout = serde_json::from_str(
            r#"{"path":"/r","dists":["noble"],"components":["main"],"architectures":["arm64"]}"#,
        )
        .unwrap();
        assert!(l.sign && l.keep == 0 && l.only.is_empty());
        let back: Layout = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!(back, l);
        assert!(
            serde_json::from_str::<Layout>(
                r#"{"path":"/r","dists":[],"components":[],"architectures":[],"typo":1}"#
            )
            .is_err()
        );
    }
}
