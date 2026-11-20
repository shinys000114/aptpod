// SPDX-License-Identifier: GPL-3.0-only

//! Building plans. Nothing here changes the published trees or the
//! revision history; only `add` ingests files into the content store (which
//! is idempotent and garbage-collected).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::Error;
use crate::db::{self, Cells, Package};
use crate::filter::Filter;
use crate::layout::{Cell, Layout, check_alias, paths_overlap};
use crate::plan::{Change, Part, PartKind, Plan};
use crate::repo::{Repo, package_ref};

/// Which dists `add` targets.
#[derive(Debug, Clone)]
pub enum Dists {
    /// From a `~<dist>` suffix of each package version.
    Detect,
    /// Every dist that has the component.
    All,
    List(Vec<String>),
}

/// A file to add, optionally with the sha256 it must have.
#[derive(Debug, Clone)]
pub struct AddFile {
    pub path: PathBuf,
    pub sha256: Option<String>,
    /// The caller owns the file and will not change it (e.g. a server
    /// upload): the store may hard-link it instead of copying.
    pub owned: bool,
}

impl From<PathBuf> for AddFile {
    fn from(path: PathBuf) -> Self {
        Self {
            path,
            sha256: None,
            owned: false,
        }
    }
}

/// What to promote.
#[derive(Debug, Clone, Default)]
pub struct Promote {
    pub dists: Option<Vec<String>>,
    pub to_dist: Option<String>,
    pub all_versions: bool,
    pub move_: bool,
}

/// A revision to roll back to.
#[derive(Debug, Clone)]
pub enum RevRef {
    Head,
    Revision(i64),
    Label(String),
}

impl RevRef {
    /// `#123` is a revision, anything else a snapshot label.
    pub fn parse(s: &str) -> Self {
        match s.strip_prefix('#').and_then(|n| n.parse().ok()) {
            Some(n) => RevRef::Revision(n),
            None if s == "HEAD" => RevRef::Head,
            None => RevRef::Label(s.to_string()),
        }
    }
}

impl Repo {
    /// A new publish. Its path must not already hold a repository tree
    /// (`pool/` or one of its `dists/<dist>`) unless `adopt`: adopting
    /// rewrites those dists and deletes pool files the publish does not list.
    pub fn plan_create(&self, alias: &str, layout: Layout, adopt: bool) -> Result<Plan, Error> {
        check_alias(alias)?;
        layout.validate()?;
        if db::publish(&self.conn, alias).is_ok() {
            return Err(Error::Invalid(format!("publish {alias:?} already exists")));
        }
        self.check_path(None, &layout)?;
        check_target(&layout, adopt)?;
        if layout.sign && !self.has_signer() {
            return Err(Error::Invalid(format!(
                "publish {alias:?} is signed but no signing key is configured"
            )));
        }
        let cells: Cells = layout
            .cells()
            .into_iter()
            .map(|c| (c, BTreeSet::new()))
            .collect();
        let mut part = Part::new(
            alias,
            PartKind::Create,
            None,
            None,
            layout,
            cells,
            format!("create {alias}"),
        );
        part.changes.push(Change::CreatePublish);
        for (d, c) in part.cells.keys() {
            part.changes.push(Change::AddCell {
                dist: d.clone(),
                component: c.clone(),
            });
        }
        Ok(Plan {
            parts: vec![part],
            warnings: vec![],
        })
    }

    pub(crate) fn check_path(&self, except: Option<&str>, layout: &Layout) -> Result<(), Error> {
        for (alias, other) in self.publishes()? {
            if Some(alias.as_str()) != except && paths_overlap(&layout.path, &other.path) {
                return Err(Error::Invalid(format!(
                    "path {} overlaps publish {alias:?} ({})",
                    layout.path.display(),
                    other.path.display()
                )));
            }
        }
        Ok(())
    }

    /// New layout for an existing publish. Cells (or architectures) that go
    /// away while holding packages need `force`.
    pub fn plan_layout(
        &self,
        alias: &str,
        layout: Layout,
        force: bool,
        adopt: bool,
    ) -> Result<Plan, Error> {
        layout.validate()?;
        self.check_path(Some(alias), &layout)?;
        let (p, old) = self.head(alias)?;
        if p.layout.path != layout.path {
            check_target(&layout, adopt)?;
        }
        if layout.sign && !self.has_signer() {
            return Err(Error::Invalid(format!(
                "publish {alias:?} is signed but no signing key is configured"
            )));
        }
        let pkgs = db::packages(&self.conn, old.values().flatten().copied())?;
        let mut cells = Cells::new();
        let mut blocked = Vec::new();
        for cell in layout.cells() {
            let mut set = old.get(&cell).cloned().unwrap_or_default();
            let gone: Vec<i64> = set
                .iter()
                .copied()
                .filter(|id| !layout.publishes_arch(&pkgs[id].arch))
                .collect();
            if !gone.is_empty() && !force {
                blocked.push(format!(
                    "{}/{} has {} package(s) of architectures no longer published",
                    cell.0,
                    cell.1,
                    gone.len()
                ));
            }
            for id in gone {
                set.remove(&id);
            }
            cells.insert(cell, set);
        }
        for (cell, set) in &old {
            if !cells.contains_key(cell) && !set.is_empty() && !force {
                blocked.push(format!(
                    "{}/{} still has {} package(s)",
                    cell.0,
                    cell.1,
                    set.len()
                ));
            }
        }
        if !blocked.is_empty() {
            return Err(Error::Blocked(format!(
                "{} (use --force to remove them)",
                blocked.join("; ")
            )));
        }
        let mut part = self.part(&p, layout.clone(), &old, cells, format!("layout {alias}"))?;
        let mut layout_changes = layout_diff(&p.layout, &layout);
        layout_changes.append(&mut part.changes);
        part.changes = layout_changes;
        Ok(Plan {
            parts: vec![part],
            warnings: vec![],
        })
    }

    pub fn plan_delete(&self, alias: &str, force: bool) -> Result<Plan, Error> {
        let (p, cells) = self.head(alias)?;
        let n: usize = cells.values().map(|s| s.len()).sum();
        if n > 0 && !force {
            return Err(Error::Blocked(format!(
                "publish {alias:?} still has {n} package(s) (use --force)"
            )));
        }
        let mut part = Part::new(
            alias,
            PartKind::Delete,
            Some(p.id),
            Some(p.head),
            p.layout.clone(),
            Cells::new(),
            format!("delete {alias}"),
        );
        part.changes.push(Change::DeletePublish { packages: n });
        Ok(Plan {
            parts: vec![part],
            warnings: vec![],
        })
    }

    /// Adds files to `component` of `alias`.
    pub fn plan_add(
        &self,
        alias: &str,
        component: &str,
        files: &[AddFile],
        dists: &Dists,
        replace: bool,
    ) -> Result<Plan, Error> {
        let (p, old) = self.head(alias)?;
        let layout = &p.layout;
        if !layout.components.iter().any(|c| c == component) {
            return Err(Error::Invalid(format!(
                "component {component:?} is not in publish {alias:?} ({})",
                layout.components.join(", ")
            )));
        }
        let mut warnings = Vec::new();
        let mut incoming: Vec<(i64, Vec<String>)> = Vec::new();
        let now = self.now();
        // Reading and hashing the files is the expensive part: in parallel.
        let infos = {
            let n = std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .min(8);
            let next = std::sync::atomic::AtomicUsize::new(0);
            let store = &self.store;
            let out: Vec<std::sync::Mutex<Option<Result<aptpod_core::deb::DebInfo, Error>>>> =
                files.iter().map(|_| std::sync::Mutex::new(None)).collect();
            std::thread::scope(|s| {
                for _ in 0..n.min(files.len()).max(1) {
                    s.spawn(|| {
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(f) = files.get(i) else { return };
                            *out[i].lock().unwrap() =
                                Some(store.ingest(&f.path, f.sha256.as_deref(), f.owned));
                        }
                    });
                }
            });
            out.into_iter()
                .map(|m| m.into_inner().unwrap().expect("every file ran"))
                .collect::<Result<Vec<_>, _>>()?
        };
        for (f, info) in files.iter().zip(infos) {
            let shown = f.path.display();
            if !layout.publishes_arch(info.architecture()) {
                return Err(Error::Invalid(format!(
                    "{shown}: architecture {} is not published by {alias} ({})",
                    info.architecture(),
                    layout.architectures.join(", ")
                )));
            }
            let targets = match dists {
                Dists::Detect => match detect_dist(info.version(), &layout.dists) {
                    Some(d) => vec![d],
                    None => {
                        return Err(Error::Invalid(format!(
                            "{shown}: cannot tell the dist from version {:?} (no ~<dist> suffix); pass dists",
                            info.version()
                        )));
                    }
                },
                Dists::All => layout
                    .dists
                    .iter()
                    .filter(|d| layout.has_cell(d, component))
                    .cloned()
                    .collect(),
                Dists::List(ds) => {
                    for d in ds {
                        if !layout.dists.contains(d) {
                            return Err(Error::Invalid(format!(
                                "dist {d:?} is not in publish {alias:?}"
                            )));
                        }
                    }
                    ds.clone()
                }
            };
            for d in &targets {
                if !layout.has_cell(d, component) {
                    return Err(Error::Invalid(format!(
                        "{d}/{component} is not a cell of publish {alias:?}"
                    )));
                }
            }
            if let Some(w) = source_version_warning(&info) {
                warnings.push(format!("{shown}: {w}"));
            }
            let id = db::insert_package(&self.conn, &info, now)?;
            incoming.push((id, targets));
        }

        let mut cells = old.clone();
        let mut ids: BTreeSet<i64> = old.values().flatten().copied().collect();
        ids.extend(incoming.iter().map(|(id, _)| *id));
        let pkgs = db::packages(&self.conn, ids)?;
        let mut extra = Vec::new();
        let by_path = pool_index(&pkgs, &old, component)?;
        let mut batch: BTreeMap<String, i64> = BTreeMap::new();
        for (id, targets) in &incoming {
            let pkg = &pkgs[id];
            let path = pkg.pool_path(component)?;
            if let Some(other) = batch.insert(path.clone(), *id).filter(|o| o != id) {
                let o = &pkgs[&other];
                return Err(Error::Conflict(format!(
                    "two files are {} {} {} with different content ({} and {})",
                    pkg.name,
                    pkg.version,
                    pkg.arch,
                    &o.sha256[..12],
                    &pkg.sha256[..12]
                )));
            }
            if let Some(old_id) = by_path.get(&path).copied().filter(|o| o != id) {
                if !replace {
                    return Err(Error::Conflict(format!(
                        "{} {} {} is already in {alias}/{component} with different content (use --replace)",
                        pkg.name, pkg.version, pkg.arch
                    )));
                }
                // replace everywhere the old content is listed in this component
                for ((_, c), set) in cells.iter_mut() {
                    if c == component && set.remove(&old_id) {
                        set.insert(*id);
                    }
                }
                extra.push(Change::Replace {
                    component: component.into(),
                    package: package_ref(pkg),
                });
            }
            for d in targets {
                cells
                    .entry((d.clone(), component.into()))
                    .or_default()
                    .insert(*id);
            }
        }
        let names: BTreeSet<&str> = incoming
            .iter()
            .map(|(id, _)| pkgs[id].name.as_str())
            .collect();
        if layout.keep > 0 {
            let touched: BTreeSet<Cell> = incoming
                .iter()
                .flat_map(|(_, t)| t.iter().map(|d| (d.clone(), component.to_string())))
                .collect();
            apply_keep(&mut cells, &touched, &pkgs, layout.keep, Some(&names));
        }
        let msg = format!(
            "add {}",
            names.iter().copied().collect::<Vec<_>>().join(" ")
        );
        let mut part = self.part(&p, layout.clone(), &old, cells, msg)?;
        extra.append(&mut part.changes);
        part.changes = extra;
        Ok(Plan {
            parts: vec![part],
            warnings,
        })
    }

    /// Removes matching packages from `component` (all dists unless `dists`).
    /// With `keep`, only versions beyond the newest `keep` per name and arch.
    pub fn plan_rm(
        &self,
        alias: &str,
        component: &str,
        filter: &Filter,
        dists: Option<&[String]>,
        keep: Option<u32>,
    ) -> Result<Plan, Error> {
        let (p, old) = self.head(alias)?;
        if !p.layout.components.iter().any(|c| c == component) {
            return Err(Error::Invalid(format!(
                "component {component:?} is not in publish {alias:?}"
            )));
        }
        self.remove_matching(&p, &old, Some(component), filter, dists, keep, "rm")
    }

    /// Keeps the newest `keep` versions of every package (default: the
    /// layout's `keep`).
    pub fn plan_prune_versions(
        &self,
        alias: &str,
        component: Option<&str>,
        dists: Option<&[String]>,
        keep: Option<u32>,
        filter: &Filter,
    ) -> Result<Plan, Error> {
        let (p, old) = self.head(alias)?;
        let keep = keep.filter(|k| *k > 0).unwrap_or(p.layout.keep);
        if keep == 0 {
            return Err(Error::Invalid(format!(
                "no keep count: pass one or set keep on publish {alias:?}"
            )));
        }
        self.remove_matching(
            &p,
            &old,
            component,
            filter,
            dists,
            Some(keep),
            "prune-versions",
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn remove_matching(
        &self,
        p: &db::Publish,
        old: &Cells,
        component: Option<&str>,
        filter: &Filter,
        dists: Option<&[String]>,
        keep: Option<u32>,
        what: &str,
    ) -> Result<Plan, Error> {
        if let Some(ds) = dists {
            for d in ds {
                if !p.layout.dists.contains(d) {
                    return Err(Error::Invalid(format!(
                        "dist {d:?} is not in publish {:?}",
                        p.alias
                    )));
                }
            }
        }
        let pkgs = db::packages(&self.conn, old.values().flatten().copied())?;
        let mut cells = old.clone();
        for ((d, c), set) in cells.iter_mut() {
            if component.is_some_and(|x| x != c) || dists.is_some_and(|ds| !ds.contains(d)) {
                continue;
            }
            let matching: BTreeSet<i64> = set
                .iter()
                .copied()
                .filter(|id| {
                    let k = &pkgs[id];
                    filter.matches(&k.name, &k.version, &k.arch)
                })
                .collect();
            let victims = match keep {
                Some(n) => beyond_keep(&matching, &pkgs, n),
                None => matching,
            };
            for v in victims {
                set.remove(&v);
            }
        }
        let part = self.part(
            p,
            p.layout.clone(),
            old,
            cells,
            format!("{what} {}", p.alias),
        )?;
        Ok(Plan {
            parts: vec![part],
            warnings: vec![],
        })
    }

    /// Copies (or moves) packages between cells, possibly across publishes,
    /// without re-uploading.
    pub fn plan_promote(
        &self,
        src: (&str, &str),
        dst: (&str, &str),
        filter: &Filter,
        opt: &Promote,
    ) -> Result<Plan, Error> {
        let (sp, sold) = self.head(src.0)?;
        let (dp, dold) = if src.0 == dst.0 {
            (sp.clone(), sold.clone())
        } else {
            self.head(dst.0)?
        };
        for (p, c) in [(&sp, src.1), (&dp, dst.1)] {
            if !p.layout.components.iter().any(|x| x == c) {
                return Err(Error::Invalid(format!(
                    "component {c:?} is not in publish {:?}",
                    p.alias
                )));
            }
        }
        let src_dists: Vec<String> = match &opt.dists {
            Some(ds) => {
                for d in ds {
                    if !sp.layout.dists.contains(d) {
                        return Err(Error::Invalid(format!(
                            "dist {d:?} is not in publish {:?}",
                            sp.alias
                        )));
                    }
                }
                ds.clone()
            }
            None => sp.layout.dists.clone(),
        };
        if let Some(t) = &opt.to_dist {
            if src_dists.len() != 1 {
                return Err(Error::Invalid(
                    "--to-dist needs exactly one source dist".into(),
                ));
            }
            if !dp.layout.dists.contains(t) {
                return Err(Error::Invalid(format!(
                    "dist {t:?} is not in publish {:?}",
                    dp.alias
                )));
            }
        }
        let map = |d: &str| opt.to_dist.clone().unwrap_or_else(|| d.to_string());
        let pairs: Vec<(String, String)> = src_dists
            .iter()
            .filter(|d| sp.layout.has_cell(d, src.1) && dp.layout.has_cell(&map(d), dst.1))
            .map(|d| (d.clone(), map(d)))
            .collect();
        if pairs.is_empty() {
            return Err(Error::Invalid(format!(
                "no dist has both {}/{} and {}/{}",
                src.0, src.1, dst.0, dst.1
            )));
        }
        if src.0 == dst.0 && src.1 == dst.1 && pairs.iter().any(|(a, b)| a == b) {
            return Err(Error::Invalid("destination is the source cell".into()));
        }
        let mut ids: BTreeSet<i64> = sold.values().flatten().copied().collect();
        ids.extend(dold.values().flatten().copied());
        let pkgs = db::packages(&self.conn, ids)?;
        let only_newest =
            !opt.all_versions && !filter.has_exact_version() && !filter.has_version_conditions();
        let dst_index = pool_index(&pkgs, &dold, dst.1)?;

        let mut dcells = dold.clone();
        let mut scells = sold.clone();
        for (sd, dd) in &pairs {
            let set = sold
                .get(&(sd.clone(), src.1.to_string()))
                .cloned()
                .unwrap_or_default();
            let mut picked: BTreeSet<i64> = set
                .iter()
                .copied()
                .filter(|id| {
                    let k = &pkgs[id];
                    filter.matches(&k.name, &k.version, &k.arch)
                })
                .collect();
            if only_newest {
                let old_versions = beyond_keep(&picked, &pkgs, 1);
                picked.retain(|id| !old_versions.contains(id));
            }
            for id in picked {
                let pkg = &pkgs[&id];
                if !dp.layout.publishes_arch(&pkg.arch) {
                    return Err(Error::Invalid(format!(
                        "{} {}: architecture {} is not published by {}",
                        pkg.name, pkg.version, pkg.arch, dp.alias
                    )));
                }
                let path = pkg.pool_path(dst.1)?;
                if dst_index.get(&path).is_some_and(|o| *o != id) {
                    return Err(Error::Conflict(format!(
                        "{} {} {} is already in {}/{} with different content",
                        pkg.name, pkg.version, pkg.arch, dst.0, dst.1
                    )));
                }
                dcells
                    .entry((dd.clone(), dst.1.to_string()))
                    .or_default()
                    .insert(id);
                if opt.move_ {
                    let target = if src.0 == dst.0 {
                        &mut dcells
                    } else {
                        &mut scells
                    };
                    if let Some(s) = target.get_mut(&(sd.clone(), src.1.to_string())) {
                        s.remove(&id);
                    }
                }
            }
        }
        if dp.layout.keep > 0 {
            let touched: BTreeSet<Cell> = pairs
                .iter()
                .map(|(_, d)| (d.clone(), dst.1.to_string()))
                .collect();
            apply_keep(&mut dcells, &touched, &pkgs, dp.layout.keep, None);
        }
        let verb = if opt.move_ { "move" } else { "promote" };
        let msg = format!("{verb} {}/{} -> {}/{}", src.0, src.1, dst.0, dst.1);
        let mut parts = vec![self.part(&dp, dp.layout.clone(), &dold, dcells, msg.clone())?];
        if opt.move_ && src.0 != dst.0 {
            parts.push(self.part(&sp, sp.layout.clone(), &sold, scells, msg)?);
        }
        Ok(Plan {
            parts,
            warnings: vec![],
        })
    }

    /// Restores cell contents of an earlier revision or label. Only cells
    /// the current layout has are restored; others are reported.
    pub fn plan_rollback(&self, alias: &str, to: &RevRef) -> Result<Plan, Error> {
        let (p, old) = self.head(alias)?;
        let rev = self.resolve(&p, to)?;
        let target = db::revision_cells(&self.conn, rev)?;
        let mut warnings = Vec::new();
        for (cell, set) in &target {
            if !p.layout.has_cell(&cell.0, &cell.1) && !set.is_empty() {
                warnings.push(format!(
                    "{}/{} is not in the current layout; its {} package(s) are not restored",
                    cell.0,
                    cell.1,
                    set.len()
                ));
            }
        }
        let cells: Cells = p
            .layout
            .cells()
            .into_iter()
            .map(|c| {
                let s = target.get(&c).cloned().unwrap_or_default();
                (c, s)
            })
            .collect();
        let part = self.part(
            &p,
            p.layout.clone(),
            &old,
            cells,
            format!("rollback {alias} to {}", describe(to, rev)),
        )?;
        Ok(Plan {
            parts: vec![part],
            warnings,
        })
    }

    pub(crate) fn resolve(&self, p: &db::Publish, to: &RevRef) -> Result<i64, Error> {
        match to {
            RevRef::Head => Ok(p.head),
            RevRef::Revision(r) => {
                let owner: Option<i64> = self
                    .conn
                    .query_row("SELECT publish FROM revisions WHERE id = ?1", [r], |x| {
                        x.get(0)
                    })
                    .ok();
                if owner == Some(p.id) {
                    Ok(*r)
                } else {
                    Err(Error::NotFound(format!(
                        "no revision #{r} in publish {:?}",
                        p.alias
                    )))
                }
            }
            RevRef::Label(l) => self
                .conn
                .query_row(
                    "SELECT revision FROM snapshots WHERE publish = ?1 AND label = ?2",
                    rusqlite::params![p.id, l],
                    |x| x.get(0),
                )
                .map_err(|_| {
                    Error::NotFound(format!(
                        "no snapshot labelled {l:?} in publish {:?}",
                        p.alias
                    ))
                }),
        }
    }

    /// A part with changes computed from old -> new cells.
    pub(crate) fn part(
        &self,
        p: &db::Publish,
        layout: Layout,
        old: &Cells,
        cells: Cells,
        message: String,
    ) -> Result<Part, Error> {
        let mut ids: BTreeSet<i64> = old.values().flatten().copied().collect();
        ids.extend(cells.values().flatten().copied());
        let pkgs = db::packages(&self.conn, ids)?;
        let mut part = Part::new(
            &p.alias,
            PartKind::Update,
            Some(p.id),
            Some(p.head),
            layout,
            cells,
            message,
        );
        part.changes = cell_changes(old, &part.cells, &pkgs);
        Ok(part)
    }
}

/// Refuses to write into a directory that already holds a repository tree.
pub(crate) fn check_target(layout: &Layout, adopt: bool) -> Result<(), Error> {
    if adopt {
        return Ok(());
    }
    let mut found = Vec::new();
    if layout.path.join("pool").exists() {
        found.push("pool/".to_string());
    }
    for d in &layout.dists {
        if layout.path.join("dists").join(d).exists() {
            found.push(format!("dists/{d}"));
        }
    }
    if found.is_empty() {
        Ok(())
    } else {
        Err(Error::Blocked(format!(
            "{} already contains {}; pass adopt to take it over (its dists are rewritten and unlisted pool files deleted)",
            layout.path.display(),
            found.join(", ")
        )))
    }
}

fn describe(to: &RevRef, rev: i64) -> String {
    match to {
        RevRef::Label(l) => format!("{l} (#{rev})"),
        _ => format!("#{rev}"),
    }
}

/// pool path -> package id, over every cell of `component`.
fn pool_index(
    pkgs: &BTreeMap<i64, Package>,
    cells: &Cells,
    component: &str,
) -> Result<BTreeMap<String, i64>, Error> {
    let mut out = BTreeMap::new();
    for ((_, c), set) in cells {
        if c == component {
            for id in set {
                out.insert(pkgs[id].pool_path(component)?, *id);
            }
        }
    }
    Ok(out)
}

/// Ids not among the newest `keep` versions of their (name, arch).
fn beyond_keep(set: &BTreeSet<i64>, pkgs: &BTreeMap<i64, Package>, keep: u32) -> BTreeSet<i64> {
    let mut groups: BTreeMap<(&str, &str), Vec<i64>> = BTreeMap::new();
    for id in set {
        let p = &pkgs[id];
        groups
            .entry((p.name.as_str(), p.arch.as_str()))
            .or_default()
            .push(*id);
    }
    let mut out = BTreeSet::new();
    for (_, mut ids) in groups {
        ids.sort_by(|a, b| aptpod_core::version::compare(&pkgs[b].version, &pkgs[a].version));
        out.extend(ids.into_iter().skip(keep as usize));
    }
    out
}

fn apply_keep(
    cells: &mut Cells,
    touched: &BTreeSet<Cell>,
    pkgs: &BTreeMap<i64, Package>,
    keep: u32,
    names: Option<&BTreeSet<&str>>,
) {
    for cell in touched {
        if let Some(set) = cells.get_mut(cell) {
            let candidates: BTreeSet<i64> = set
                .iter()
                .copied()
                .filter(|id| names.is_none_or(|n| n.contains(pkgs[id].name.as_str())))
                .collect();
            for v in beyond_keep(&candidates, pkgs, keep) {
                set.remove(&v);
            }
        }
    }
}

pub(crate) fn cell_changes(old: &Cells, new: &Cells, pkgs: &BTreeMap<i64, Package>) -> Vec<Change> {
    let mut out = Vec::new();
    let keys: BTreeSet<&Cell> = old.keys().chain(new.keys()).collect();
    let empty = BTreeSet::new();
    for cell in keys {
        let (d, c) = cell;
        let (o, n) = (old.get(cell), new.get(cell));
        match (o, n) {
            (Some(o), None) => out.push(Change::RemoveCell {
                dist: d.clone(),
                component: c.clone(),
                packages: o.len(),
            }),
            (None, Some(_)) => out.push(Change::AddCell {
                dist: d.clone(),
                component: c.clone(),
            }),
            _ => {}
        }
        let (o, n) = (o.unwrap_or(&empty), n.unwrap_or(&empty));
        if new.contains_key(cell) {
            for id in n.difference(o) {
                out.push(Change::Add {
                    dist: d.clone(),
                    component: c.clone(),
                    package: package_ref(&pkgs[id]),
                });
            }
            for id in o.difference(n) {
                out.push(Change::Remove {
                    dist: d.clone(),
                    component: c.clone(),
                    package: package_ref(&pkgs[id]),
                });
            }
        }
    }
    out
}

pub(crate) fn layout_diff(a: &Layout, b: &Layout) -> Vec<Change> {
    let (va, vb) = (
        serde_json::to_value(a).unwrap_or_default(),
        serde_json::to_value(b).unwrap_or_default(),
    );
    let mut keys: BTreeSet<String> = BTreeSet::new();
    for v in [&va, &vb] {
        if let Some(o) = v.as_object() {
            keys.extend(o.keys().cloned());
        }
    }
    keys.into_iter()
        .filter_map(|k| {
            let (x, y) = (
                va.get(&k).cloned().unwrap_or_default(),
                vb.get(&k).cloned().unwrap_or_default(),
            );
            (x != y).then(|| Change::Layout {
                field: k,
                from: x.to_string(),
                to: y.to_string(),
            })
        })
        .collect()
}

/// The dist named by a `~<dist>` suffix of `version`, if it is one of `dists`.
pub fn detect_dist(version: &str, dists: &[String]) -> Option<String> {
    let tail = version.rsplit_once('~')?.1;
    dists.iter().find(|d| d.as_str() == tail).cloned()
}

/// apt 2.6 (Debian 12) fails with "Internal parsing error" on a binary
/// whose `Source: x (version)` is newer than the binary itself.
fn source_version_warning(info: &aptpod_core::deb::DebInfo) -> Option<String> {
    let src = info.control.get("Source")?;
    let v = src.split_once('(')?.1.trim_end_matches(')').trim();
    (aptpod_core::version::compare(v, info.version()) == std::cmp::Ordering::Greater).then(|| {
        format!("source version {v} is newer than the binary version {}; apt 2.6 (Debian 12) cannot install it", info.version())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dist_detection() {
        let d = vec!["jammy".to_string(), "noble".to_string()];
        assert_eq!(detect_dist("0.1.3+26~noble", &d).as_deref(), Some("noble"));
        assert_eq!(detect_dist("1.0~rc1~jammy", &d).as_deref(), Some("jammy"));
        assert_eq!(detect_dist("1.0~rc1", &d), None);
        assert_eq!(detect_dist("1.0", &d), None);
    }

    #[test]
    fn rev_ref_parse() {
        assert!(matches!(RevRef::parse("#12"), RevRef::Revision(12)));
        assert!(matches!(RevRef::parse("HEAD"), RevRef::Head));
        assert!(matches!(RevRef::parse("before-x"), RevRef::Label(l) if l == "before-x"));
    }
}
