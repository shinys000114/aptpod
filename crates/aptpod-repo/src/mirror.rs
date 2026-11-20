// SPDX-License-Identifier: GPL-3.0-only

//! Re-signed mirrors: a publish whose layout and contents follow an
//! upstream archive. Every sync brings the whole publish to the upstream
//! state in one revision (labelled `sync-<UTC time>`), published with our
//! key. Dists that could not be read this time keep their contents.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use aptpod_core::deb::DebInfo;
use serde::Serialize;

use crate::Error;
use crate::db::{self, Cells};
use crate::layout::{Layout, check_alias};
use crate::ops::{RevRef, cell_changes, check_target, layout_diff};
use crate::plan::{Change, Part, PartKind, Plan};
use crate::repo::Repo;

/// Prefix of the snapshot labels a sync creates (and prunes).
pub const SYNC_LABEL: &str = "sync-";

/// One upstream dist as read this time.
#[derive(Debug, Clone, Default)]
pub struct FetchedDist {
    pub dist: String,
    /// component -> sha256 of every package the upstream lists there (after
    /// filtering), in upstream order; a component without packages is
    /// still a cell.
    pub components: Vec<(String, Vec<String>)>,
}

/// The desired state of a mirrored publish.
#[derive(Debug, Clone, Default)]
pub struct MirrorSync {
    pub path: PathBuf,
    /// The configured dists, in order. A dist missing from `fetched` keeps
    /// its current contents (or stays unpublished).
    pub dists: Vec<String>,
    pub fetched: Vec<FetchedDist>,
    pub architectures: Vec<String>,
    /// Release Origin and Label; `None` keeps the current ones.
    pub origin: Option<String>,
    pub label: Option<String>,
    /// Packages taken into the store for this sync (their rows are added).
    pub ingested: Vec<DebInfo>,
    /// Refuse when more packages than this would leave cells...
    pub max_delete: usize,
    /// ...unless forced.
    pub force: bool,
    /// Sync snapshots to keep (0: none are made).
    pub keep_snapshots: usize,
    /// The path may already hold a tree (first sync only).
    pub adopt: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MirrorReport {
    pub created: bool,
    /// New head, when something changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    pub added: usize,
    pub removed: usize,
    /// Packages of kept dists whose pool file now has the upstream's new
    /// content.
    pub replaced: usize,
    /// Fetched dists refused here (dist, why); they keep their contents.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dropped_snapshots: Vec<String>,
    /// Dists written.
    pub published: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl Repo {
    /// The subset of `sha256` that packages in the repository have.
    pub fn known_packages(&self, sha256: &[String]) -> Result<BTreeSet<String>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT 1 FROM packages WHERE sha256 = ?1")?;
        let mut out = BTreeSet::new();
        for s in sha256 {
            if stmt.exists([s])? {
                out.insert(s.clone());
            }
        }
        Ok(out)
    }

    /// Brings publish `alias` (created when missing) to the state `s`
    /// describes, publishes it and labels the new revision.
    pub fn sync_mirror(
        &mut self,
        alias: &str,
        s: &MirrorSync,
        author: &str,
    ) -> Result<MirrorReport, Error> {
        check_alias(alias)?;
        let now = self.now();
        for info in &s.ingested {
            db::insert_package(&self.conn, info, now)?;
        }
        let mut report = MirrorReport::default();
        let existing = db::publish(&self.conn, alias).ok();
        let old: Cells = match &existing {
            Some(p) => db::revision_cells(&self.conn, p.head)?,
            None => Cells::new(),
        };
        if let Some(p) = &existing
            && s.keep_snapshots > 0
        {
            let last = self
                .snapshots(alias)?
                .into_iter()
                .find(|x| x.label.starts_with(SYNC_LABEL));
            if last.is_some_and(|l| l.revision != p.head) {
                report.warnings.push(format!(
                    "{alias} was changed since its last sync (e.g. rolled back); \
                     this sync replaces it with the upstream state"
                ));
            }
        }

        // sha256 -> package id for everything fetched.
        let mut ids: BTreeMap<String, i64> = BTreeMap::new();
        let mut failed: BTreeMap<String, String> = BTreeMap::new();
        for d in &s.fetched {
            for (_, shas) in &d.components {
                for sha in shas {
                    if ids.contains_key(sha) {
                        continue;
                    }
                    match db::package_by_sha(&self.conn, sha)? {
                        Some(p) => {
                            ids.insert(sha.clone(), p.id);
                        }
                        None => {
                            failed.entry(d.dist.clone()).or_insert_with(|| {
                                format!("package {} is not in the store", &sha[..12.min(sha.len())])
                            });
                        }
                    }
                }
            }
        }
        let mut all: BTreeSet<i64> = ids.values().copied().collect();
        all.extend(old.values().flatten().copied());
        let pkgs = db::packages(&self.conn, all)?;

        // Pool paths are shared by every dist of a component: the first
        // dist that lists a path decides its content, a later dist with
        // other content is refused.
        let mut by_path: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
        let mut accepted: BTreeMap<String, Vec<(String, BTreeSet<i64>)>> = BTreeMap::new();
        for d in &s.fetched {
            if failed.contains_key(&d.dist) {
                continue;
            }
            let mut mine: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
            let mut cells = Vec::new();
            let mut clash = None;
            'comps: for (comp, shas) in &d.components {
                let mut set = BTreeSet::new();
                for sha in shas {
                    let id = ids[sha];
                    let path = pkgs[&id].pool_path(comp)?;
                    let seen = by_path
                        .get(comp)
                        .and_then(|m| m.get(&path))
                        .or_else(|| mine.get(comp).and_then(|m| m.get(&path)));
                    if seen.is_some_and(|o| *o != id) {
                        clash = Some(format!(
                            "{comp}: {path} has other content than in an earlier dist"
                        ));
                        break 'comps;
                    }
                    mine.entry(comp.clone()).or_default().insert(path, id);
                    set.insert(id);
                }
                cells.push((comp.clone(), set));
            }
            if let Some(why) = clash {
                failed.insert(d.dist.clone(), why);
                continue;
            }
            for (comp, m) in mine {
                by_path.entry(comp).or_default().extend(m);
            }
            accepted.insert(d.dist.clone(), cells);
        }

        // The layout: configured dists that were read, plus the ones that
        // keep what they had.
        let old_layout = existing.as_ref().map(|p| p.layout.clone());
        let mut dist_comps: Vec<(String, Vec<String>)> = Vec::new();
        let mut kept = Vec::new();
        for d in &s.dists {
            if let Some(cells) = accepted.get(d) {
                dist_comps.push((d.clone(), cells.iter().map(|(c, _)| c.clone()).collect()));
            } else if let Some(l) = &old_layout
                && !l.components_of(d).is_empty()
            {
                dist_comps.push((d.clone(), l.components_of(d)));
                kept.push(d.clone());
            }
        }
        if dist_comps.is_empty() {
            return Err(Error::Invalid(format!(
                "no dist of {alias} could be read; nothing to publish"
            )));
        }
        let mut components: Vec<String> = Vec::new();
        for (_, cs) in &dist_comps {
            for c in cs {
                if !components.contains(c) {
                    components.push(c.clone());
                }
            }
        }
        let mut only = BTreeMap::new();
        for c in &components {
            let ds: Vec<String> = dist_comps
                .iter()
                .filter(|(_, cs)| cs.contains(c))
                .map(|(d, _)| d.clone())
                .collect();
            if ds.len() < dist_comps.len() {
                only.insert(c.clone(), ds);
            }
        }
        let mut architectures = s.architectures.clone();
        if let Some(l) = old_layout.as_ref().filter(|_| !kept.is_empty()) {
            for a in &l.architectures {
                if !architectures.contains(a) {
                    architectures.push(a.clone());
                }
            }
        }
        let mut layout = old_layout
            .clone()
            .unwrap_or_else(|| Layout::new(&s.path, &[], &[], &[]));
        layout.path = s.path.clone();
        layout.dists = dist_comps.iter().map(|(d, _)| d.clone()).collect();
        layout.components = components;
        layout.only = only;
        layout.architectures = architectures;
        if let Some(o) = &s.origin {
            layout.origin = o.clone();
        }
        if let Some(l) = &s.label {
            layout.label = l.clone();
        }
        layout.sign = true;
        layout.keep = 0;
        // History is the sync snapshots (and labels set by hand).
        layout.history_max_age = Some(0);
        layout.validate()?;
        if !self.has_signer() {
            return Err(Error::Invalid(format!(
                "mirror {alias:?} is re-signed but no signing key is configured"
            )));
        }

        let mut cells = Cells::new();
        for (d, comps) in &accepted {
            for (c, set) in comps {
                cells.insert((d.clone(), c.clone()), set.clone());
            }
        }
        for d in &kept {
            for c in layout.components_of(d) {
                let mut set = BTreeSet::new();
                for id in old.get(&(d.clone(), c.clone())).into_iter().flatten() {
                    let p = &pkgs[id];
                    if !layout.publishes_arch(&p.arch) {
                        continue;
                    }
                    // The shared pool file now has the upstream's content.
                    let path = p.pool_path(&c)?;
                    match by_path.get(&c).and_then(|m| m.get(&path)) {
                        Some(new) if new != id => {
                            report.replaced += 1;
                            set.insert(*new);
                        }
                        _ => {
                            set.insert(*id);
                        }
                    }
                }
                cells.insert((d.clone(), c), set);
            }
        }

        let msg = format!("mirror sync {alias}");
        let part = match &existing {
            Some(p) => {
                self.check_path(Some(alias), &layout)?;
                if p.layout.path != layout.path {
                    check_target(&layout, s.adopt)?;
                }
                let mut part = self.part(p, layout.clone(), &old, cells, msg)?;
                let mut changes = layout_diff(&p.layout, &layout);
                changes.append(&mut part.changes);
                part.changes = changes;
                part
            }
            None => {
                self.check_path(None, &layout)?;
                check_target(&layout, s.adopt)?;
                let mut part = Part::new(
                    alias,
                    PartKind::Create,
                    None,
                    None,
                    layout.clone(),
                    cells,
                    msg,
                );
                let mut changes = vec![Change::CreatePublish];
                changes.extend(cell_changes(&Cells::new(), &part.cells, &pkgs));
                part.changes = changes;
                report.created = true;
                part
            }
        };
        for c in &part.changes {
            match c {
                Change::Add { .. } => report.added += 1,
                Change::Remove { .. } => report.removed += 1,
                Change::RemoveCell { packages, .. } => report.removed += packages,
                _ => {}
            }
        }
        report.failed = failed.into_iter().collect();
        if report.removed > s.max_delete && !s.force {
            return Err(Error::Blocked(format!(
                "the sync would remove {} package(s), more than max_delete ({}); \
                 nothing was changed (sync with --prune to allow it)",
                report.removed, s.max_delete
            )));
        }
        let plan = Plan {
            parts: vec![part],
            warnings: vec![],
        };
        if plan.is_empty() {
            return Ok(report);
        }
        let applied = self.apply(&plan, author)?;
        if applied.pool_copied > 0 {
            report
                .warnings
                .push(copied_warning(applied.pool_copied, &s.path));
        }
        report.revision = applied.revisions.first().map(|(_, r)| *r);
        report.published = applied.published.into_iter().map(|(_, d)| d).collect();

        if s.keep_snapshots > 0 {
            let t = time::OffsetDateTime::from_unix_timestamp(now)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
            let base = format!(
                "{SYNC_LABEL}{:04}{:02}{:02}T{:02}{:02}{:02}Z",
                t.year(),
                u8::from(t.month()),
                t.day(),
                t.hour(),
                t.minute(),
                t.second()
            );
            let taken: BTreeSet<String> = self
                .snapshots(alias)?
                .into_iter()
                .map(|x| x.label)
                .collect();
            let mut label = base.clone();
            let mut n = 1;
            while taken.contains(&label) {
                n += 1;
                label = format!("{base}-{n}");
            }
            self.snapshot_create(alias, &label, "mirror sync", &RevRef::Head)?;
            report.snapshot = Some(label);
        }
        let old_syncs: Vec<String> = self
            .snapshots(alias)?
            .into_iter()
            .filter(|x| x.label.starts_with(SYNC_LABEL))
            .skip(s.keep_snapshots)
            .map(|x| x.label)
            .collect();
        for l in &old_syncs {
            self.snapshot_drop(alias, l)?;
        }
        if !old_syncs.is_empty() {
            self.expire_history(alias)?;
        }
        report.dropped_snapshots = old_syncs;
        Ok(report)
    }
}

/// Pool files were copied from the store instead of hard-linked.
pub fn copied_warning(n: usize, tree: &std::path::Path) -> String {
    format!(
        "{n} pool file(s) were copied into {} instead of hard-linked from the store \
         (different filesystem or mount; under systemd, a StateDirectory bind mount): \
         each is stored twice",
        tree.display()
    )
}
