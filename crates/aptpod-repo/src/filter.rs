// SPDX-License-Identifier: GPL-3.0-only

//! Selecting packages: shell-style name globs (`linux-image-*`, `*-dbg`),
//! `name=version` specs, dpkg-ordered version conditions and architecture.

use std::cmp::Ordering;

use aptpod_core::version;

use crate::Error;

/// One `name` or `name=version` argument; the name is a glob.
#[derive(Debug, Clone)]
pub struct Spec {
    pattern: glob::Pattern,
    version: Option<String>,
}

impl Spec {
    pub fn parse(s: &str) -> Result<Self, Error> {
        let (name, ver) = match s.split_once('=') {
            Some((n, v)) if !v.is_empty() => (n, Some(v.to_string())),
            Some(_) => return Err(Error::Invalid(format!("empty version in {s:?}"))),
            None => (s, None),
        };
        let pattern = glob::Pattern::new(name)
            .map_err(|e| Error::Invalid(format!("bad package pattern {name:?}: {e}")))?;
        Ok(Self {
            pattern,
            version: ver,
        })
    }

    pub fn has_version(&self) -> bool {
        self.version.is_some()
    }

    pub fn matches(&self, name: &str, ver: &str) -> bool {
        self.pattern.matches(name) && self.version.as_deref().is_none_or(|v| v == ver)
    }
}

/// A version condition such as `>=1.2` (all conditions must hold).
#[derive(Debug, Clone)]
struct Cond {
    op: &'static str,
    version: String,
}

/// Package selection: any spec matches (none = all), every version condition
/// holds, and the architecture equals `arch` when given.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    specs: Vec<Spec>,
    conds: Vec<Cond>,
    arch: Option<String>,
}

impl Filter {
    pub fn new(specs: &[&str], versions: Option<&str>, arch: Option<&str>) -> Result<Self, Error> {
        let specs = specs
            .iter()
            .map(|s| Spec::parse(s))
            .collect::<Result<Vec<_>, _>>()?;
        let mut conds = Vec::new();
        if let Some(v) = versions.filter(|v| !v.trim().is_empty()) {
            for part in v.split(',') {
                conds.push(parse_cond(part)?);
            }
        }
        Ok(Self {
            specs,
            conds,
            arch: arch.map(String::from),
        })
    }

    pub fn all() -> Self {
        Self::default()
    }

    /// Whether any spec pins an exact version.
    pub fn has_exact_version(&self) -> bool {
        self.specs.iter().any(Spec::has_version)
    }

    pub fn has_version_conditions(&self) -> bool {
        !self.conds.is_empty()
    }

    pub fn matches(&self, name: &str, ver: &str, arch: &str) -> bool {
        if self.arch.as_deref().is_some_and(|a| a != arch) {
            return false;
        }
        if !self.specs.is_empty() && !self.specs.iter().any(|s| s.matches(name, ver)) {
            return false;
        }
        self.conds.iter().all(|c| {
            let o = version::compare(ver, &c.version);
            match c.op {
                ">=" => o != Ordering::Less,
                "<=" => o != Ordering::Greater,
                ">" => o == Ordering::Greater,
                "<" => o == Ordering::Less,
                "!=" => o != Ordering::Equal,
                _ => o == Ordering::Equal,
            }
        })
    }
}

fn parse_cond(s: &str) -> Result<Cond, Error> {
    let s = s.trim();
    for (prefix, op) in [
        (">=", ">="),
        ("<=", "<="),
        ("!=", "!="),
        (">>", ">"),
        ("<<", "<"),
        (">", ">"),
        ("<", "<"),
        ("=", "="),
    ] {
        if let Some(v) = s.strip_prefix(prefix) {
            let v = v.trim();
            if v.is_empty() {
                return Err(Error::Invalid(format!(
                    "version condition {s:?} has no version"
                )));
            }
            return Ok(Cond {
                op,
                version: v.to_string(),
            });
        }
    }
    if s.is_empty() {
        return Err(Error::Invalid("empty version condition".into()));
    }
    Ok(Cond {
        op: "=",
        version: s.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_versions_and_arch() {
        let f = Filter::new(
            &["linux-image-*vendor*", "*-dbg"],
            Some(">=9:6.6.144-202608000000,<9:6.6.144-202610000000"),
            Some("arm64"),
        )
        .unwrap();
        assert!(f.matches(
            "linux-image-6.6.0-vendor-board-arm64",
            "9:6.6.144-202609031112~resolute",
            "arm64"
        ));
        assert!(f.matches("foo-dbg", "9:6.6.144-202609031112~resolute", "arm64"));
        assert!(!f.matches(
            "linux-headers-6.6.0-vendor",
            "9:6.6.144-202609031112~resolute",
            "arm64"
        ));
        assert!(
            !f.matches("foo-dbg", "9:6.6.144-202610151627~resolute", "arm64"),
            "too new"
        );
        assert!(
            !f.matches("foo-dbg", "9:6.6.144-202609031112~resolute", "amd64"),
            "arch"
        );
    }

    #[test]
    fn exact_versions_and_errors() {
        let f = Filter::new(&["hello=1.0", "tool"], None, None).unwrap();
        assert!(f.has_exact_version());
        assert!(
            f.matches("hello", "1.0", "all")
                && !f.matches("hello", "1.1", "all")
                && f.matches("tool", "9", "all")
        );
        assert!(Filter::all().matches("x", "1", "amd64"));
        assert!(Filter::new(&["["], None, None).is_err());
        assert!(Filter::new(&["x="], None, None).is_err());
        for bad in [">=", ",", "<<"] {
            assert!(Filter::new(&[], Some(bad), None).is_err(), "{bad}");
        }
        for (cond, v) in [
            ("1.0", "1.0"),
            ("=1.0", "1.0"),
            ("!=1.0", "1.1"),
            (">>1.0", "1.1"),
            ("<<1.0", "1.0~rc1"),
        ] {
            assert!(
                Filter::new(&[], Some(cond), None)
                    .unwrap()
                    .matches("x", v, "all"),
                "{cond} {v}"
            );
        }
    }
}
