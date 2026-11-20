// SPDX-License-Identifier: GPL-3.0-only

//! Re-signed mirror over HTTP: the upstream Release and indices are
//! verified with the upstream key, the selected packages (after filters)
//! are fetched into the repository's content store, and an aptpod publish
//! is brought to that state and signed with our key
//! ([`aptpod_repo::Repo::sync_mirror`]). Only packages the repository does
//! not have yet are downloaded; every downloaded `.deb` must have the
//! name, version and architecture its index stanza claims, so a filter
//! cannot be bypassed by an index.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use aptpod_core::deb::DebInfo;
use aptpod_repo::Filter;
use aptpod_repo::mirror::{FetchedDist, MirrorReport, MirrorSync};
use aptpod_repo::store::Store;
use serde::Serialize;

use crate::Error;
use crate::fetch::{FetchError, Http, join_url, sha256_hex};
use crate::http::{Ctx, DistErr, DistReport, HttpMirror, fe, index_groups, par_map, release_of};
use crate::release::{
    PackageEntry, VARIANTS, decompress, packages_entries, parseable, variant_path,
};

/// What to mirror and how to publish it.
#[derive(Debug, Clone)]
pub struct ResignMirror {
    /// Source selection: url, dists, components, architectures, keyring
    /// (required), ignore_missing, parallel, retries. `udebs`, `sources`
    /// and `extras` are not supported.
    pub source: HttpMirror,
    /// Root of the published tree.
    pub path: PathBuf,
    /// Only packages matching (name globs with optional versions); `None`
    /// takes all.
    pub include: Option<Filter>,
    /// Packages to leave out.
    pub exclude: Option<Filter>,
    /// Release Origin / Label; default: the upstream's.
    pub origin: Option<String>,
    pub label: Option<String>,
    pub keep_snapshots: usize,
    /// Refuse a sync that removes more packages than this...
    pub max_delete: usize,
    /// ...unless forced.
    pub force: bool,
    /// The path may already hold a tree (first sync).
    pub adopt: bool,
}

impl ResignMirror {
    pub fn new(url: &str, dists: &[&str], path: impl Into<PathBuf>) -> Self {
        Self {
            source: HttpMirror::new(url, dists),
            path: path.into(),
            include: None,
            exclude: None,
            origin: None,
            label: None,
            keep_snapshots: 10,
            max_delete: 10_000,
            force: false,
            adopt: false,
        }
    }

    /// Checks the configuration (normalizes the source URL).
    pub fn validate(&mut self) -> Result<(), Error> {
        let s = &self.source;
        if s.udebs || s.sources || !s.extras.is_empty() {
            return Err(Error::Config(
                "udebs, sources and extras are not supported by re-signed mirrors".into(),
            ));
        }
        if s.keyring.is_none() {
            return Err(Error::Config(
                "a re-signed mirror needs the upstream key: its content is signed with ours".into(),
            ));
        }
        if !self.path.is_absolute() {
            return Err(Error::Config(format!(
                "path {} must be absolute",
                self.path.display()
            )));
        }
        self.source.validate()
    }

    fn wanted(&self, e: &PackageEntry) -> bool {
        self.include
            .as_ref()
            .is_none_or(|f| f.matches(&e.name, &e.version, &e.arch))
            && !self
                .exclude
                .as_ref()
                .is_some_and(|f| f.matches(&e.name, &e.version, &e.arch))
    }
}

/// The repository side of a sync (the server runs `apply` on its single
/// writer).
pub trait Target: Sync {
    /// The subset of `sha256` the repository already has packages for.
    fn known(&self, sha256: &[String]) -> Result<HashSet<String>, Error>;
    fn apply(&self, sync: MirrorSync) -> Result<MirrorReport, Error>;
}

/// Where and how a sync runs.
pub struct ResignCtx<'a> {
    pub http: &'a Http,
    /// Private state (`incoming/` downloads); on the store's filesystem so
    /// downloads are linked, not copied, into the store.
    pub state_dir: &'a Path,
    pub store: &'a Store,
    pub cancel: &'a AtomicBool,
    pub log: &'a (dyn Fn(&str) + Sync),
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ResignReport {
    pub dists: Vec<DistReport>,
    /// Packages selected (after filters), counted once.
    pub packages: usize,
    /// Upstream packages left out by the filters.
    pub filtered: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub resumed: usize,
    pub ignored_missing: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publish: Option<MirrorReport>,
    pub warnings: Vec<String>,
}

impl ResignReport {
    pub fn ok(&self) -> bool {
        self.dists.iter().all(|d| d.ok)
    }
}

/// A dist as read from upstream.
struct UpDist {
    dist: String,
    origin: Option<String>,
    label: Option<String>,
    archs: Vec<String>,
    /// component -> entries (deduplicated by sha256, filtered)
    comps: Vec<(String, Vec<PackageEntry>)>,
    indices: usize,
    filtered: usize,
}

/// Syncs `m` into the publish the target holds.
pub fn sync(m: &ResignMirror, ctx: &ResignCtx, target: &dyn Target) -> Result<ResignReport, Error> {
    let mut m = m.clone();
    m.validate()?;
    let mut report = ResignReport::default();
    let incoming = ctx.state_dir.join("incoming");
    crate::retry_fs(|| fs::create_dir_all(&incoming))?;

    // 1. Release and indices of every dist.
    let hctx = Ctx {
        http: ctx.http,
        dest: Path::new("/"),
        state_dir: ctx.state_dir,
        cancel: ctx.cancel,
        log: ctx.log,
    };
    let mut read: Vec<UpDist> = Vec::new();
    for dist in &m.source.dists {
        let mut restarts = 0;
        loop {
            match read_dist(&m, &hctx, dist) {
                Ok(d) => {
                    (ctx.log)(&format!(
                        "{dist}: {} index file(s), {} package(s) selected, {} filtered out",
                        d.indices,
                        d.comps.iter().map(|(_, e)| e.len()).sum::<usize>(),
                        d.filtered
                    ));
                    report.dists.push(DistReport {
                        dist: dist.clone(),
                        ok: true,
                        indices: d.indices,
                        packages: d.comps.iter().map(|(_, e)| e.len()).sum(),
                        restarts,
                        ..Default::default()
                    });
                    report.filtered += d.filtered;
                    read.push(d);
                    break;
                }
                Err(DistErr::Changed(why)) if restarts < 3 => {
                    restarts += 1;
                    (ctx.log)(&format!(
                        "{dist}: upstream changed ({why}); reading it again"
                    ));
                }
                Err(DistErr::Interrupted) => return Err(Error::Interrupted),
                Err(DistErr::Changed(why) | DistErr::Fatal(why)) => {
                    (ctx.log)(&format!("{dist}: FAILED: {why}"));
                    report.dists.push(DistReport {
                        dist: dist.clone(),
                        ok: false,
                        error: Some(why),
                        restarts,
                        ..Default::default()
                    });
                    break;
                }
            }
        }
    }

    // 2. Package files the repository does not have yet.
    let mut files: BTreeMap<String, &PackageEntry> = BTreeMap::new();
    for d in &read {
        for (_, entries) in &d.comps {
            for e in entries {
                files.entry(e.file.sha256.clone()).or_insert(e);
            }
        }
    }
    report.packages = files.len();
    let shas: Vec<String> = files.keys().cloned().collect();
    let known = target.known(&shas)?;
    let todo: Vec<&PackageEntry> = files
        .values()
        .copied()
        .filter(|e| !known.contains(&e.file.sha256))
        .collect();
    (ctx.log)(&format!(
        "pool: {} package(s), {} new to the repository",
        files.len(),
        todo.len()
    ));
    let (ingested, failures, ignored) = fetch_new(&m, ctx, &incoming, &todo, &mut report)?;
    report.ignored_missing = ignored.iter().cloned().collect();

    // 3. Dists with a file that could not be had keep their contents.
    let mut fetched = Vec::new();
    let mut origin = None;
    let mut label = None;
    let mut archs: Vec<String> = Vec::new();
    for d in &read {
        let bad: Vec<&PackageEntry> = d
            .comps
            .iter()
            .flat_map(|(_, es)| es)
            .filter(|e| failures.contains_key(&e.file.sha256))
            .collect();
        if let Some(e) = bad.first() {
            let why = format!(
                "{} package file(s) could not be fetched, e.g. {}: {}",
                bad.len(),
                e.file.path,
                failures[&e.file.sha256]
            );
            (ctx.log)(&format!("{}: FAILED: {why}; its contents stay", d.dist));
            fail(&mut report, &d.dist, why);
            continue;
        }
        origin = origin.or_else(|| d.origin.clone());
        label = label.or_else(|| d.label.clone());
        for a in &d.archs {
            if !archs.contains(a) {
                archs.push(a.clone());
            }
        }
        fetched.push(FetchedDist {
            dist: d.dist.clone(),
            components: d
                .comps
                .iter()
                .map(|(c, es)| {
                    (
                        c.clone(),
                        es.iter()
                            .filter(|e| !ignored.contains(&e.file.path))
                            .map(|e| e.file.sha256.clone())
                            .collect(),
                    )
                })
                .collect(),
        });
    }

    // 4. The publish.
    let keep: HashSet<String> = read
        .iter()
        .filter(|d| !fetched.iter().any(|f| f.dist == d.dist))
        .flat_map(|d| {
            d.comps
                .iter()
                .flat_map(|(_, es)| es.iter().map(|e| e.file.sha256.clone()))
        })
        .collect();
    if fetched.is_empty() {
        let w = "no dist could be read; the publish is unchanged".to_string();
        (ctx.log)(&w);
        report.warnings.push(w);
    } else {
        // Store files must be on disk before rows and indices refer to them.
        crate::http::sync_fs(ctx.state_dir)?;
        let sync = MirrorSync {
            path: m.path.clone(),
            dists: m.source.dists.clone(),
            fetched,
            architectures: archs,
            origin: m.origin.clone().or(origin),
            label: m.label.clone().or(label),
            ingested,
            max_delete: m.max_delete,
            force: m.force,
            keep_snapshots: m.keep_snapshots,
            adopt: m.adopt,
        };
        let r = target.apply(sync)?;
        for (dist, why) in &r.failed {
            (ctx.log)(&format!("{dist}: FAILED: {why}; its contents stay"));
            fail(&mut report, dist, why.clone());
        }
        for w in &r.warnings {
            (ctx.log)(&format!("warning: {w}"));
        }
        match &r.revision {
            Some(rev) => (ctx.log)(&format!(
                "publish: revision #{rev}{}: {} added, {} removed, {} replaced; {} dist(s) written",
                r.snapshot
                    .as_deref()
                    .map(|s| format!(" ({s})"))
                    .unwrap_or_default(),
                r.added,
                r.removed,
                r.replaced,
                r.published.len()
            )),
            None => (ctx.log)("publish: unchanged"),
        }
        report.publish = Some(r);
    }
    clean_incoming(&incoming, &keep);
    Ok(report)
}

fn fail(report: &mut ResignReport, dist: &str, why: String) {
    if let Some(d) = report.dists.iter_mut().find(|d| d.dist == dist) {
        d.ok = false;
        d.error = Some(why);
    }
}

/// Removes downloads no failed dist still needs (partial ones included).
fn clean_incoming(dir: &Path, keep: &HashSet<String>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let sha = name.split('.').next().unwrap_or("");
        if !keep.contains(sha) {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Release (verified) and the selected binary indices of `dist`.
fn read_dist(m: &ResignMirror, ctx: &Ctx, dist: &str) -> Result<UpDist, DistErr> {
    let src = &m.source;
    let base = join_url(&src.url, &format!("dists/{dist}")) + "/";
    let (rel, _) = release_of(src, ctx, &base)?;
    let (archs, groups) = index_groups(src, &rel)?;
    // The best variant listed per group: (group index, path, ext).
    let mut want: Vec<(usize, Vec<(String, &str)>)> = Vec::new();
    for (i, (dir, base_name, _)) in groups.iter().enumerate() {
        let vs: Vec<(String, &str)> = VARIANTS
            .iter()
            .filter(|e| parseable(e))
            .map(|e| (variant_path(dir, base_name, e), *e))
            .filter(|(p, _)| rel.files.contains_key(p))
            .collect();
        want.push((i, vs));
    }
    let got = par_map(
        &want,
        src.parallel,
        |(_, vs)| -> Result<Option<String>, DistErr> {
            for (path, ext) in vs {
                let entry = &rel.files[path];
                match ctx
                    .http
                    .get_retry(&join_url(&base, path), src.retries, ctx.cancel)
                {
                    Ok(data) => {
                        if data.len() as u64 != entry.size || sha256_hex(&data) != entry.sha256 {
                            return Err(DistErr::Changed(format!("{path} does not match Release")));
                        }
                        let text = decompress(ext, &data)
                            .map_err(|e| DistErr::Fatal(format!("{path}: {e}")))?;
                        return Ok(Some(text));
                    }
                    Err(FetchError::Missing(_)) => continue,
                    Err(e) => return Err(fe(path, e)),
                }
            }
            Ok(None)
        },
    );
    let mut comps: Vec<(String, Vec<PackageEntry>)> = Vec::new();
    let mut indices = 0;
    let mut filtered = 0;
    let mut first_err = None;
    let mut seen: BTreeMap<String, BTreeMap<String, PackageEntry>> = BTreeMap::new();
    for ((i, _), r) in want.iter().zip(got) {
        let (dir, base_name, required) = &groups[*i];
        let comp = dir.split('/').next().unwrap_or(dir).to_string();
        if !comps.iter().any(|(c, _)| *c == comp) {
            comps.push((comp.clone(), Vec::new()));
        }
        let text = match r {
            Ok(Some(t)) => t,
            Ok(None) if *required => {
                first_err.get_or_insert(DistErr::Fatal(format!(
                    "no usable {dir}/{base_name} index upstream"
                )));
                continue;
            }
            Ok(None) => continue,
            Err(e) => {
                let rank = |e: &DistErr| match e {
                    DistErr::Interrupted => 0,
                    DistErr::Changed(_) => 1,
                    DistErr::Fatal(_) => 2,
                };
                if first_err.as_ref().is_none_or(|f| rank(&e) < rank(f)) {
                    first_err = Some(e);
                }
                continue;
            }
        };
        indices += 1;
        let entries = packages_entries(&text).map_err(|e| DistErr::Fatal(format!("{dir}: {e}")))?;
        let list = seen.entry(comp).or_default();
        for e in entries {
            if e.arch != "all" && !archs.contains(&e.arch) {
                continue;
            }
            if !m.wanted(&e) {
                filtered += 1;
                continue;
            }
            if let Some(old) = list.get(&e.file.sha256) {
                if old.name != e.name || old.version != e.version || old.arch != e.arch {
                    return Err(DistErr::Fatal(format!(
                        "{} is listed as two different packages",
                        e.file.path
                    )));
                }
                continue;
            }
            list.insert(e.file.sha256.clone(), e);
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    for (comp, entries) in comps.iter_mut() {
        if let Some(list) = seen.remove(comp.as_str()) {
            *entries = list.into_values().collect();
        }
    }
    let field = |f: &str| {
        rel.fields
            .get(f)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Ok(UpDist {
        dist: dist.to_string(),
        origin: field("Origin"),
        label: field("Label"),
        archs,
        comps,
        indices,
        filtered,
    })
}

type Fetched = (Vec<DebInfo>, BTreeMap<String, String>, BTreeSet<String>);

/// Downloads (or finds in the store) and ingests `todo`; returns the
/// ingested packages, failures (sha256 -> why) and the pool paths that
/// were missing upstream and are ignored.
fn fetch_new(
    m: &ResignMirror,
    ctx: &ResignCtx,
    incoming: &Path,
    todo: &[&PackageEntry],
    report: &mut ResignReport,
) -> Result<Fetched, Error> {
    enum Got {
        Ok(Box<DebInfo>, Option<(u64, bool)>),
        Ignored,
        Failed(String),
    }
    let src = &m.source;
    let stop = AtomicBool::new(false);
    let fatal = std::sync::Mutex::new(None::<Error>);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let total = todo.len();
    let results = par_map(todo, src.parallel, |e| -> Got {
        if stop.load(Ordering::Relaxed) {
            return Got::Failed("stopped".into());
        }
        let sha = &e.file.sha256;
        let check = |info: DebInfo| -> Result<Box<DebInfo>, String> {
            if info.name() != e.name || info.version() != e.version || info.architecture() != e.arch
            {
                return Err(format!(
                    "the file is {} {} {}, the index says {} {} {}",
                    info.name(),
                    info.version(),
                    info.architecture(),
                    e.name,
                    e.version,
                    e.arch
                ));
            }
            Ok(Box::new(info))
        };
        // Already in the store (e.g. a sync that failed after ingesting).
        // No row refers to it (it is not known), so a bad one can go.
        let stored = ctx.store.path(sha);
        if stored.is_file() {
            match ctx.store.ingest(&stored, Some(sha), false) {
                Ok(info) => {
                    return match check(info) {
                        Ok(i) => Got::Ok(i, None),
                        Err(why) => Got::Failed(why),
                    };
                }
                Err(_) => {
                    let _ = fs::remove_file(&stored);
                }
            }
        }
        let dest = incoming.join(format!("{sha}.deb"));
        let url = join_url(&src.url, &e.file.path);
        let dl = match ctx
            .http
            .download(&url, &dest, e.file.size, sha, src.retries, ctx.cancel)
        {
            Ok(d) => d,
            Err(FetchError::Missing(_)) if src.ignore_missing.contains(&e.file.path) => {
                return Got::Ignored;
            }
            Err(FetchError::Interrupted) => {
                stop.store(true, Ordering::Relaxed);
                return Got::Failed("interrupted".into());
            }
            Err(FetchError::Local(why)) => {
                stop.store(true, Ordering::Relaxed);
                *fatal.lock().unwrap() = Some(Error::Io(std::io::Error::other(why.clone())));
                return Got::Failed(why);
            }
            Err(err) => return Got::Failed(err.to_string()),
        };
        let r = match ctx.store.ingest(&dest, Some(sha), true) {
            Ok(info) => match check(info) {
                Ok(i) => Got::Ok(i, Some((dl.bytes, dl.resumed))),
                Err(why) => Got::Failed(why),
            },
            Err(err) => Got::Failed(err.to_string()),
        };
        let _ = fs::remove_file(&dest);
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        if n % 200 == 0 || n == total {
            (ctx.log)(&format!("pool: {n}/{total}"));
        }
        r
    });
    if let Some(e) = fatal.into_inner().unwrap() {
        return Err(e);
    }
    if stop.load(Ordering::Relaxed) || ctx.cancel.load(Ordering::Relaxed) {
        return Err(Error::Interrupted);
    }
    let mut infos = Vec::new();
    let mut failures = BTreeMap::new();
    let mut ignored = BTreeSet::new();
    for (e, r) in todo.iter().zip(results) {
        match r {
            Got::Ok(info, dl) => {
                if let Some((bytes, resumed)) = dl {
                    report.downloaded += 1;
                    report.downloaded_bytes += bytes;
                    report.resumed += usize::from(resumed);
                }
                infos.push(*info);
            }
            Got::Ignored => {
                ignored.insert(e.file.path.clone());
            }
            Got::Failed(why) => {
                (ctx.log)(&format!("{}: {why}", e.file.path));
                failures.insert(e.file.sha256.clone(), why);
            }
        }
    }
    (ctx.log)(&format!(
        "pool: {} downloaded ({} bytes), {} failed, {} missing and ignored",
        report.downloaded,
        report.downloaded_bytes,
        failures.len(),
        ignored.len()
    ));
    Ok((infos, failures, ignored))
}
