// SPDX-License-Identifier: GPL-3.0-only

//! Plans: every change is computed first (a pure value bound to the head
//! revisions it was computed from), shown or returned as a dry run, and only
//! then applied. Applying refuses if a head moved in between.

use serde::Serialize;

use crate::db::Cells;
use crate::layout::Layout;

/// A package as shown in plans and listings.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct PackageRef {
    pub name: String,
    pub version: String,
    pub arch: String,
    pub sha256: String,
}

/// One visible effect of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    CreatePublish,
    DeletePublish {
        packages: usize,
    },
    AddCell {
        dist: String,
        component: String,
    },
    RemoveCell {
        dist: String,
        component: String,
        packages: usize,
    },
    Add {
        dist: String,
        component: String,
        package: PackageRef,
    },
    Remove {
        dist: String,
        component: String,
        package: PackageRef,
    },
    /// The pool file of `package` replaces different content under the same name.
    Replace {
        component: String,
        package: PackageRef,
    },
    Layout {
        field: String,
        from: String,
        to: String,
    },
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pkg = |p: &PackageRef| format!("{} {} {}", p.name, p.version, p.arch);
        match self {
            Change::CreatePublish => write!(f, "+ publish"),
            Change::DeletePublish { packages } => write!(f, "- publish ({packages} packages)"),
            Change::AddCell { dist, component } => write!(f, "+ cell     {dist}/{component}"),
            Change::RemoveCell {
                dist,
                component,
                packages,
            } => write!(f, "- cell     {dist}/{component} ({packages} packages)"),
            Change::Add {
                dist,
                component,
                package,
            } => write!(f, "+ {dist}/{component} {}", pkg(package)),
            Change::Remove {
                dist,
                component,
                package,
            } => write!(f, "- {dist}/{component} {}", pkg(package)),
            Change::Replace { component, package } => {
                write!(f, "~ {component} {} (pool file replaced)", pkg(package))
            }
            Change::Layout { field, from, to } => write!(f, "~ {field}: {from} -> {to}"),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PartKind {
    /// New layout and/or cell contents for an existing publish.
    Update,
    Create,
    Delete,
}

/// The part of a plan that concerns one publish.
#[derive(Debug, Clone)]
pub struct Part {
    pub alias: String,
    pub(crate) kind: PartKind,
    pub(crate) publish_id: Option<i64>,
    /// Head revision the part was computed from.
    pub(crate) base: Option<i64>,
    pub(crate) layout: Layout,
    pub(crate) cells: Cells,
    pub(crate) message: String,
    pub changes: Vec<Change>,
}

/// A computed, not yet applied change.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub parts: Vec<Part>,
    pub warnings: Vec<String>,
}

impl Plan {
    /// Nothing would change.
    pub fn is_empty(&self) -> bool {
        self.parts.iter().all(|p| p.changes.is_empty())
    }

    pub fn changes(&self) -> impl Iterator<Item = (&str, &Change)> {
        self.parts
            .iter()
            .flat_map(|p| p.changes.iter().map(move |c| (p.alias.as_str(), c)))
    }

    /// Whether the plan removes anything.
    pub fn is_destructive(&self) -> bool {
        self.changes().any(|(_, c)| {
            matches!(
                c,
                Change::DeletePublish { .. }
                    | Change::RemoveCell { .. }
                    | Change::Remove { .. }
                    | Change::Replace { .. }
            )
        })
    }

    /// Human readable lines, one per change.
    pub fn lines(&self) -> Vec<String> {
        let multi = self.parts.len() > 1;
        self.changes()
            .map(|(a, c)| {
                if multi {
                    format!("[{a}] {c}")
                } else {
                    c.to_string()
                }
            })
            .collect()
    }
}

/// What applying did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Applied {
    /// (alias, new head revision)
    pub revisions: Vec<(String, i64)>,
    /// Dists written per publish.
    pub published: Vec<(String, String)>,
    pub pool_removed: usize,
}
