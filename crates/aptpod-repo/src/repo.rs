// SPDX-License-Identifier: GPL-3.0-only

//! The repository: database + store + published trees.
//!
//! The database is the truth. `apply` commits a plan, then brings every
//! touched tree in line with its new head; each dist records the revision it
//! was last written from, so a failed publish is repaired by the next
//! [`Repo::repair`] (or the next apply on that publish).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use aptpod_core::index::ReleaseMeta;
use aptpod_core::pool::{PlaceMode, cleanup_pool};
use aptpod_core::publish::{
    ComponentSpec, DistSpec, PublishPackage, clean_leftovers, publish_dist, remove_dist,
};
use aptpod_core::sign::Signer;

use crate::Error;
use crate::db::{self, Cells, Package};
use crate::filter::Filter;
use crate::layout::Layout;
use crate::plan::{Applied, PackageRef, Part, PartKind, Plan};
use crate::store::Store;

/// The repository engine.
pub struct Repo {
    pub(crate) conn: rusqlite::Connection,
    pub(crate) store: Store,
    signer: Option<Box<dyn Signer + Send>>,
    clock: Box<dyn Fn() -> i64 + Send>,
}

/// A package in a cell, as listed by `ls` / `find`.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub alias: String,
    pub dist: String,
    pub component: String,
    #[serde(flatten)]
    pub package: PackageRef,
    #[serde(skip)]
    pub id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CellStatus {
    pub dist: String,
    pub component: String,
    pub packages: usize,
}

impl Repo {
    /// Opens (or creates) a repository in `data_dir`.
    pub fn open(data_dir: &Path) -> Result<Self, Error> {
        fs::create_dir_all(data_dir)?;
        let conn = db::open(&data_dir.join("aptpod.db"))?;
        let repo = Self {
            conn,
            store: Store::new(data_dir.join("store")),
            signer: None,
            clock: Box::new(|| time::OffsetDateTime::now_utc().unix_timestamp()),
        };
        for p in db::publishes(&repo.conn)? {
            clean_leftovers(&p.layout.path)?;
        }
        Ok(repo)
    }

    /// Signer used for publishes with `sign = true`.
    pub fn set_signer(&mut self, signer: Option<Box<dyn Signer + Send>>) {
        self.signer = signer;
    }

    /// Overrides the clock (unix seconds); for tests.
    pub fn set_clock(&mut self, clock: Box<dyn Fn() -> i64 + Send>) {
        self.clock = clock;
    }

    pub(crate) fn now(&self) -> i64 {
        (self.clock)()
    }

    pub fn has_signer(&self) -> bool {
        self.signer.is_some()
    }

    /// (alias, layout) of every publish.
    pub fn publishes(&self) -> Result<Vec<(String, Layout)>, Error> {
        Ok(db::publishes(&self.conn)?
            .into_iter()
            .map(|p| (p.alias, p.layout))
            .collect())
    }

    pub fn layout(&self, alias: &str) -> Result<Layout, Error> {
        Ok(db::publish(&self.conn, alias)?.layout)
    }

    pub(crate) fn head(&self, alias: &str) -> Result<(db::Publish, Cells), Error> {
        let p = db::publish(&self.conn, alias)?;
        let cells = db::revision_cells(&self.conn, p.head)?;
        Ok((p, cells))
    }

    /// Commits `plan` and publishes the touched trees.
    pub fn apply(&mut self, plan: &Plan, author: &str) -> Result<Applied, Error> {
        let mut applied = Applied::default();
        if plan.is_empty() {
            return Ok(applied);
        }
        let now = self.now();
        let mut deleted: Vec<(Layout, Vec<String>)> = Vec::new();
        let mut moved: Vec<(PathBuf, Vec<String>)> = Vec::new();
        let mut touched: Vec<String> = Vec::new();
        {
            let tx = self.conn.transaction()?;
            for part in &plan.parts {
                match part.kind {
                    PartKind::Create => {
                        if tx
                            .query_row(
                                "SELECT 1 FROM publishes WHERE alias = ?1",
                                [&part.alias],
                                |_| Ok(()),
                            )
                            .is_ok()
                        {
                            return Err(Error::Stale(format!(
                                "publish {:?} was created meanwhile",
                                part.alias
                            )));
                        }
                        tx.execute(
                            "INSERT INTO publishes (alias, layout, created) VALUES (?1, ?2, ?3)",
                            rusqlite::params![
                                part.alias,
                                serde_json::to_string(&part.layout)?,
                                now
                            ],
                        )?;
                        let id = tx.last_insert_rowid();
                        let rev = db::insert_revision(
                            &tx,
                            id,
                            &part.layout,
                            &part.cells,
                            author,
                            &part.message,
                            now,
                        )?;
                        applied.revisions.push((part.alias.clone(), rev));
                        touched.push(part.alias.clone());
                    }
                    PartKind::Update | PartKind::Delete => {
                        let id = part.publish_id.expect("existing publish");
                        let (head, old_layout): (i64, String) = tx.query_row(
                            "SELECT head, layout FROM publishes WHERE id = ?1",
                            [id],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )?;
                        if Some(head) != part.base {
                            return Err(Error::Stale(format!(
                                "publish {:?} changed since this plan was made; plan again",
                                part.alias
                            )));
                        }
                        let old_layout: Layout = serde_json::from_str(&old_layout)?;
                        if matches!(part.kind, PartKind::Delete) {
                            let dists = db::published(&tx, id)?.into_keys().collect();
                            tx.execute("DELETE FROM publishes WHERE id = ?1", [id])?;
                            deleted.push((old_layout, dists));
                        } else {
                            if old_layout.path != part.layout.path {
                                // the new path starts empty; the old tree is removed below
                                let dists = db::published(&tx, id)?.into_keys().collect();
                                tx.execute("DELETE FROM published WHERE publish = ?1", [id])?;
                                moved.push((old_layout.path.clone(), dists));
                            }
                            let rev = db::insert_revision(
                                &tx,
                                id,
                                &part.layout,
                                &part.cells,
                                author,
                                &part.message,
                                now,
                            )?;
                            applied.revisions.push((part.alias.clone(), rev));
                            touched.push(part.alias.clone());
                        }
                    }
                }
            }
            tx.commit()?;
        }

        // The revisions are committed. A failure from here on leaves trees
        // behind their heads; `repair` finishes the job.
        let unsynced = |e: Error| {
            Error::Unsynced(format!(
                "changes were saved, but updating the published tree failed: {e}; run repair"
            ))
        };
        for alias in &touched {
            let p = db::publish(&self.conn, alias).map_err(unsynced)?;
            let (dists, removed) = self.sync_tree(&p).map_err(unsynced)?;
            applied
                .published
                .extend(dists.into_iter().map(|d| (alias.clone(), d)));
            applied.pool_removed += removed;
        }
        for (path, dists) in moved {
            applied.pool_removed += remove_tree(&path, &dists).map_err(unsynced)?;
        }
        for (layout, dists) in deleted {
            applied.pool_removed += remove_tree(&layout.path, &dists).map_err(unsynced)?;
        }
        for alias in &touched {
            self.expire_history(alias).map_err(unsynced)?;
        }
        Ok(applied)
    }

    /// Re-publishes every publish whose tree is behind its head.
    pub fn repair(&mut self) -> Result<Applied, Error> {
        let mut applied = Applied::default();
        for p in db::publishes(&self.conn)? {
            let published = db::published(&self.conn, p.id)?;
            let behind = p.layout.dists.iter().any(|d| {
                !p.layout.components_of(d).is_empty() && published.get(d) != Some(&p.head)
            }) || published.keys().any(|d| !p.layout.dists.contains(d));
            if behind {
                let (dists, removed) = self.sync_tree(&p)?;
                applied
                    .published
                    .extend(dists.into_iter().map(|d| (p.alias.clone(), d)));
                applied.pool_removed += removed;
            }
        }
        Ok(applied)
    }

    /// Writes the dists of `p` that differ from what the tree shows, removes
    /// dists the layout no longer has, then cleans the pool.
    fn sync_tree(&self, p: &db::Publish) -> Result<(Vec<String>, usize), Error> {
        let layout = &p.layout;
        if layout.sign && self.signer.is_none() {
            return Err(Error::Invalid(format!(
                "publish {:?} is signed but no signing key is configured",
                p.alias
            )));
        }
        let root = &layout.path;
        fs::create_dir_all(root)?;
        clean_leftovers(root)?;
        let cells = db::revision_cells(&self.conn, p.head)?;
        let published = db::published(&self.conn, p.id)?;

        // Packages already in the tree (from published revisions) are
        // trusted: no re-hash of large pools on every publish.
        let mut previous: BTreeMap<i64, (Layout, Cells)> = BTreeMap::new();
        for rev in published.values().copied().collect::<BTreeSet<_>>() {
            previous.insert(
                rev,
                (
                    db::revision_layout(&self.conn, rev)?,
                    db::revision_cells(&self.conn, rev)?,
                ),
            );
        }
        let mut ids: BTreeSet<i64> = cells.values().flatten().copied().collect();
        for (_, c) in previous.values() {
            ids.extend(c.values().flatten().copied());
        }
        let pkgs = db::packages(&self.conn, ids)?;
        // What the published revisions put at each pool path. Same content
        // as head -> trust (no re-hash); other content -> replace (rollback
        // over a replace, or an interrupted one); unknown -> verify.
        let mut in_tree: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
        for (_, c) in previous.values() {
            for ((_, comp), set) in c {
                for id in set {
                    in_tree
                        .entry(pkgs[id].pool_path(comp)?)
                        .or_default()
                        .insert(pkgs[id].sha256.as_str());
                }
            }
        }
        let mode_for = |path: &str, sha: &str| match in_tree.get(path) {
            Some(shas) if shas.len() == 1 && shas.contains(sha) => PlaceMode::Trust,
            Some(_) => PlaceMode::Replace,
            None => PlaceMode::Verify,
        };

        let mut written = Vec::new();
        for dist in &layout.dists {
            let comps = layout.components_of(dist);
            if comps.is_empty() {
                if published.contains_key(dist) || root.join("dists").join(dist).exists() {
                    remove_dist(root, dist)?;
                    db::set_published(&self.conn, p.id, dist, None)?;
                }
                continue;
            }
            if let Some(rev) = published.get(dist)
                && (*rev == p.head || {
                    let (old_layout, old_cells) = &previous[rev];
                    same_release(old_layout, layout)
                        && dist_cells(old_cells, dist) == dist_cells(&cells, dist)
                        && old_layout.components_of(dist) == comps
                })
            {
                if *rev != p.head {
                    db::set_published(&self.conn, p.id, dist, Some(p.head))?;
                }
                continue;
            }
            let mut components = Vec::new();
            for comp in &comps {
                let mut packages = Vec::new();
                for id in cells
                    .get(&(dist.clone(), comp.clone()))
                    .into_iter()
                    .flatten()
                {
                    let pkg = &pkgs[id];
                    let mode = mode_for(&pkg.pool_path(comp)?, &pkg.sha256);
                    packages.push(PublishPackage {
                        info: pkg.deb_info()?,
                        file: self.store.path(&pkg.sha256),
                        mode,
                    });
                }
                components.push(ComponentSpec {
                    name: comp.clone(),
                    packages,
                });
            }
            let spec = DistSpec {
                name: dist.clone(),
                meta: release_meta(layout, dist),
                architectures: layout.architectures.clone(),
                components,
            };
            let signer: Option<&dyn Signer> = if layout.sign {
                self.signer.as_deref().map(|s| s as &dyn Signer)
            } else {
                None
            };
            publish_dist(root, &spec, signer)?;
            db::set_published(&self.conn, p.id, dist, Some(p.head))?;
            written.push(dist.clone());
        }
        for dist in published.keys() {
            if !layout.dists.contains(dist) {
                remove_dist(root, dist)?;
                db::set_published(&self.conn, p.id, dist, None)?;
            }
        }
        let mut keep = HashSet::new();
        for ((_, comp), set) in &cells {
            for id in set {
                keep.insert(pkgs[id].pool_path(comp)?);
            }
        }
        let removed = cleanup_pool(root, &keep)?.len();
        Ok((written, removed))
    }

    /// Packages of the given cells of a publish, filtered; sorted by dist
    /// (layout order), component, name, newest version first, arch.
    pub fn list(
        &self,
        alias: &str,
        components: Option<&[String]>,
        dists: Option<&[String]>,
        filter: &Filter,
    ) -> Result<Vec<Row>, Error> {
        let (p, cells) = self.head(alias)?;
        let pkgs = db::packages(&self.conn, cells.values().flatten().copied())?;
        let mut rows = Vec::new();
        for ((d, c), set) in &cells {
            if components.is_some_and(|cs| !cs.contains(c))
                || dists.is_some_and(|ds| !ds.contains(d))
            {
                continue;
            }
            for id in set {
                let pkg = &pkgs[id];
                if filter.matches(&pkg.name, &pkg.version, &pkg.arch) {
                    rows.push(Row {
                        alias: alias.into(),
                        dist: d.clone(),
                        component: c.clone(),
                        package: package_ref(pkg),
                        id: *id,
                    });
                }
            }
        }
        sort_rows(&mut rows, &p.layout.dists);
        Ok(rows)
    }

    /// Searches every publish (or the named ones).
    pub fn find(
        &self,
        aliases: Option<&[String]>,
        filter: &Filter,
        newest_only: bool,
    ) -> Result<Vec<Row>, Error> {
        let mut out = Vec::new();
        for (alias, _) in self.publishes()? {
            if aliases.is_some_and(|a| !a.contains(&alias)) {
                continue;
            }
            let mut rows = self.list(&alias, None, None, filter)?;
            if newest_only {
                rows = newest(rows);
            }
            out.extend(rows);
        }
        Ok(out)
    }

    /// Package count per cell.
    pub fn status(&self, alias: &str) -> Result<Vec<CellStatus>, Error> {
        let (p, cells) = self.head(alias)?;
        Ok(p.layout
            .cells()
            .into_iter()
            .map(|(d, c)| {
                let n = cells.get(&(d.clone(), c.clone())).map_or(0, |s| s.len());
                CellStatus {
                    dist: d,
                    component: c,
                    packages: n,
                }
            })
            .collect())
    }
}

pub(crate) fn package_ref(p: &Package) -> PackageRef {
    PackageRef {
        name: p.name.clone(),
        version: p.version.clone(),
        arch: p.arch.clone(),
        sha256: p.sha256.clone(),
    }
}

pub(crate) fn sort_rows(rows: &mut [Row], dist_order: &[String]) {
    let pos = |d: &str| dist_order.iter().position(|x| x == d).unwrap_or(usize::MAX);
    rows.sort_by(|a, b| {
        a.alias
            .cmp(&b.alias)
            .then_with(|| pos(&a.dist).cmp(&pos(&b.dist)))
            .then_with(|| a.component.cmp(&b.component))
            .then_with(|| a.package.name.cmp(&b.package.name))
            .then_with(|| aptpod_core::version::compare(&b.package.version, &a.package.version))
            .then_with(|| a.package.arch.cmp(&b.package.arch))
    });
}

/// Keeps the newest version per (alias, dist, component, name, arch).
pub(crate) fn newest(rows: Vec<Row>) -> Vec<Row> {
    let mut seen = HashSet::new();
    let mut rows = rows;
    rows.sort_by(|a, b| aptpod_core::version::compare(&b.package.version, &a.package.version));
    let mut out: Vec<Row> = rows
        .into_iter()
        .filter(|r| {
            seen.insert((
                r.alias.clone(),
                r.dist.clone(),
                r.component.clone(),
                r.package.name.clone(),
                r.package.arch.clone(),
            ))
        })
        .collect();
    let order: Vec<String> = out.iter().map(|r| r.dist.clone()).collect();
    sort_rows(&mut out, &order);
    out
}

fn dist_cells(cells: &Cells, dist: &str) -> Vec<(String, BTreeSet<i64>)> {
    cells
        .iter()
        .filter(|((d, _), _)| d == dist)
        .map(|((_, c), s)| (c.clone(), s.clone()))
        .collect()
}

/// Release-relevant settings are equal (so an unchanged dist need not be
/// rewritten).
fn same_release(a: &Layout, b: &Layout) -> bool {
    a.path == b.path
        && a.architectures == b.architectures
        && a.origin == b.origin
        && a.label == b.label
        && a.description == b.description
        && a.not_automatic == b.not_automatic
        && a.but_automatic_upgrades == b.but_automatic_upgrades
        && a.sign == b.sign
}

fn release_meta(layout: &Layout, dist: &str) -> ReleaseMeta {
    let mut meta = ReleaseMeta::new(dist, &layout.origin, &layout.label);
    meta.description = layout.description.clone();
    meta.not_automatic = layout.not_automatic;
    meta.but_automatic_upgrades = layout.but_automatic_upgrades;
    meta
}

/// Removes what aptpod wrote under `root`: the dists it recorded and the
/// pool. Other files under `root` stay.
fn remove_tree(root: &Path, dists: &[String]) -> Result<usize, Error> {
    for d in dists {
        remove_dist(root, d)?;
    }
    let _ = fs::remove_dir(root.join("dists"));
    let removed = cleanup_pool(root, &HashSet::new())?.len();
    let _ = fs::remove_dir(root.join("pool"));
    Ok(removed)
}

impl Part {
    pub(crate) fn new(
        alias: &str,
        kind: PartKind,
        publish_id: Option<i64>,
        base: Option<i64>,
        layout: Layout,
        cells: Cells,
        message: String,
    ) -> Self {
        Self {
            alias: alias.into(),
            kind,
            publish_id,
            base,
            layout,
            cells,
            message,
            changes: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn repo_is_send() {
        fn send<T: Send>() {}
        send::<super::Repo>();
    }
}
