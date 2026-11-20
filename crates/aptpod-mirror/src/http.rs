// SPDX-License-Identifier: GPL-3.0-only

//! Verbatim mirror over HTTP: upstream index and Release files are kept
//! byte for byte (with their signature), pool files are fetched and
//! checked against the indices, and a dist is switched to its new indices
//! only when every file they reference is in place.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use aptpod_core::verify::{Keyring, inrelease_text_unverified, verify_detached, verify_inrelease};
use serde::Serialize;

use crate::cache::HashCache;
use crate::fetch::{FetchError, Http, PART, join_url, sha256_file, sha256_hex};
use crate::release::{
    PoolFile, Release, VARIANTS, decompress, packages_files, parseable, sources_files, variant_path,
};
use crate::{Error, safe_rel};

/// What to mirror.
#[derive(Debug, Clone)]
pub struct HttpMirror {
    /// Archive root, e.g. `http://deb.example.org/debian/`.
    pub url: String,
    pub dists: Vec<String>,
    /// Empty: every component the dist's Release lists.
    pub components: Vec<String>,
    /// Empty: every architecture the dist's Release lists.
    pub architectures: Vec<String>,
    /// `debian-installer` indices and their files.
    pub udebs: bool,
    /// Source indices and their files.
    pub sources: bool,
    /// Further index files to keep, as globs on paths relative to
    /// `dists/<dist>/` (e.g. `*/i18n/*`, `Contents-*`).
    pub extras: Vec<String>,
    /// Trusted upstream keys; `None` mirrors without checking signatures.
    pub keyring: Option<Keyring>,
    /// Pool paths the indices reference but upstream does not serve
    /// (404/410) that are accepted as missing.
    pub ignore_missing: BTreeSet<String>,
    pub parallel: usize,
    pub retries: u32,
    /// Re-hash every present pool file instead of trusting the cache.
    pub verify_all: bool,
    /// Delete pool files no dist references (otherwise only report them;
    /// the default, so an adopted tree is never cleaned unasked).
    pub prune: bool,
    /// Delete nothing when more than this many files would go.
    pub max_delete: usize,
}

impl HttpMirror {
    pub fn new(url: &str, dists: &[&str]) -> Self {
        Self {
            url: url.into(),
            dists: dists.iter().map(|d| d.to_string()).collect(),
            components: vec![],
            architectures: vec![],
            udebs: false,
            sources: false,
            extras: vec![],
            keyring: None,
            ignore_missing: BTreeSet::new(),
            parallel: 4,
            retries: 3,
            verify_all: false,
            prune: false,
            max_delete: 10_000,
        }
    }
}

/// Where and how a sync runs.
pub struct Ctx<'a> {
    pub http: &'a Http,
    /// The mirrored tree (served as is).
    pub dest: &'a Path,
    /// Private state (hash cache), outside the served tree.
    pub state_dir: &'a Path,
    pub cancel: &'a AtomicBool,
    pub log: &'a (dyn Fn(&str) + Sync),
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DistReport {
    pub dist: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub indices: usize,
    pub packages: usize,
    pub restarts: u32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncReport {
    pub dists: Vec<DistReport>,
    /// Pool files the new indices reference.
    pub pool_files: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub resumed: usize,
    /// Present files whose content did not match and were fetched again.
    pub replaced: usize,
    pub ignored_missing: Vec<String>,
    pub deleted: usize,
    /// Unreferenced files left in place (prune off, or the brake).
    pub would_delete: usize,
    pub brake: bool,
    pub warnings: Vec<String>,
}

impl SyncReport {
    pub fn ok(&self) -> bool {
        self.dists.iter().all(|d| d.ok)
    }
}

const STAGING: &str = ".aptpod-tmp-";
const OLD: &str = ".aptpod-old-";

/// Mirrors `m` into `ctx.dest`. Returns `Err` only for problems that stop
/// the whole run (configuration, the destination, interruption); a failed
/// dist keeps its previous indices and is reported.
pub fn sync(m: &HttpMirror, ctx: &Ctx) -> Result<SyncReport, Error> {
    let mut m = m.clone();
    m.validate()?;
    fs::create_dir_all(ctx.dest.join("dists"))?;
    clean_staging(&ctx.dest.join("dists"))?;
    let mut cache = HashCache::load(ctx.state_dir);
    let mut report = SyncReport::default();
    let res = run(&m, ctx, &mut cache, &mut report);
    // Keep what was learnt even when the run failed or was interrupted.
    cache.retain(|rel| ctx.dest.join(rel).is_file());
    let saved = cache.save();
    res?;
    saved?;
    Ok(report)
}

impl HttpMirror {
    /// Checks the configuration and normalizes it (URL ends with `/`).
    pub fn validate(&mut self) -> Result<(), Error> {
        check_config(self)
    }
}

fn check_config(m: &mut HttpMirror) -> Result<(), Error> {
    if !(m.url.starts_with("http://") || m.url.starts_with("https://")) {
        return Err(Error::Config(format!("{:?} is not an http(s) URL", m.url)));
    }
    if !m.url.ends_with('/') {
        m.url.push('/');
    }
    if m.dists.is_empty() {
        return Err(Error::Config("no dists to mirror".into()));
    }
    for d in &m.dists {
        safe_rel(d).map_err(|_| Error::Config(format!("invalid dist {d:?}")))?;
        if d.split('/').any(|c| c.starts_with('.')) {
            return Err(Error::Config(format!("invalid dist {d:?}")));
        }
    }
    for p in &m.extras {
        glob::Pattern::new(p).map_err(|e| Error::Config(format!("extras pattern {p:?}: {e}")))?;
    }
    m.parallel = m.parallel.max(1);
    Ok(())
}

/// Removes staging and old dist directories an interrupted run left.
fn clean_staging(dists: &Path) -> Result<(), Error> {
    fn walk(dir: &Path, depth: usize) -> Result<(), Error> {
        let Ok(rd) = fs::read_dir(dir) else {
            return Ok(());
        };
        for e in rd {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if !e.file_type()?.is_dir() {
                continue;
            }
            if name.starts_with(STAGING) || name.starts_with(OLD) {
                fs::remove_dir_all(e.path())?;
            } else if depth > 0 && !name.starts_with('.') {
                walk(&e.path(), depth - 1)?;
            }
        }
        Ok(())
    }
    walk(dists, 2)
}

/// A dist whose new indices are staged.
struct Staged {
    dist: String,
    staging: PathBuf,
    files: BTreeMap<String, PoolFile>,
    report: DistReport,
}

enum DistErr {
    /// Upstream changed while we read it: start the dist over.
    Changed(String),
    Fatal(String),
    Interrupted,
}

fn fe(what: &str, e: FetchError) -> DistErr {
    match e {
        FetchError::Interrupted => DistErr::Interrupted,
        e => DistErr::Fatal(format!("{what}: {e}")),
    }
}

fn run(
    m: &HttpMirror,
    ctx: &Ctx,
    cache: &mut HashCache,
    report: &mut SyncReport,
) -> Result<(), Error> {
    let dists_dir = ctx.dest.join("dists");

    // 1. Release and indices of every dist, into staging directories.
    let mut staged: Vec<Staged> = Vec::new();
    let mut failed: Vec<DistReport> = Vec::new();
    for (n, dist) in m.dists.iter().enumerate() {
        let staging = dists_dir.join(format!(
            "{STAGING}{}-{}-{n}",
            dist.replace('/', "_"),
            std::process::id()
        ));
        let mut restarts = 0;
        loop {
            let _ = fs::remove_dir_all(&staging);
            match stage_dist(m, ctx, dist, &staging) {
                Ok((files, indices, packages)) => {
                    (ctx.log)(&format!(
                        "{dist}: {indices} index file(s), {packages} package file(s)"
                    ));
                    staged.push(Staged {
                        dist: dist.clone(),
                        staging: staging.clone(),
                        files,
                        report: DistReport {
                            dist: dist.clone(),
                            ok: true,
                            error: None,
                            indices,
                            packages,
                            restarts,
                        },
                    });
                    break;
                }
                Err(DistErr::Changed(why)) if restarts < 3 => {
                    restarts += 1;
                    (ctx.log)(&format!(
                        "{dist}: upstream changed ({why}); starting the dist over"
                    ));
                }
                Err(DistErr::Interrupted) => {
                    remove_staging(&staged, &staging);
                    return Err(Error::Interrupted);
                }
                Err(DistErr::Changed(why) | DistErr::Fatal(why)) => {
                    let _ = fs::remove_dir_all(&staging);
                    (ctx.log)(&format!("{dist}: FAILED: {why}"));
                    failed.push(DistReport {
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

    // 2. Pool files of the staged dists. The first dist to list a path
    // decides its content; a dist that disagrees fails.
    let mut wanted: BTreeMap<String, PoolFile> = BTreeMap::new();
    let mut keep = Vec::new();
    let mut clashed: Vec<String> = Vec::new();
    for s in staged {
        let clash = s.files.values().find(|f| {
            wanted
                .get(&f.path)
                .is_some_and(|w| w.sha256 != f.sha256 || w.size != f.size)
        });
        if let Some(f) = clash {
            let why = format!("{} has other content than in an earlier dist", f.path);
            (ctx.log)(&format!("{}: FAILED: {why}", s.dist));
            let _ = fs::remove_dir_all(&s.staging);
            clashed.extend(s.files.keys().cloned());
            failed.push(DistReport {
                ok: false,
                error: Some(why),
                ..s.report
            });
            continue;
        }
        for f in s.files.values() {
            wanted.insert(f.path.clone(), f.clone());
        }
        keep.push(s);
    }
    let staged = keep;
    report.pool_files = wanted.len();
    let failures = match fetch_pool(m, ctx, cache, &wanted, report) {
        Ok(f) => f,
        Err(e) => {
            remove_staging(&staged, Path::new(""));
            return Err(e);
        }
    };

    // Files of dists that do not switch stay protected from cleanup: they
    // were (partly) fetched for them and the next run needs them.
    let mut protect: HashSet<String> = clashed.into_iter().collect();

    // 3. Switch every dist whose files are all in place.
    for s in staged {
        let bad: Vec<&String> = s
            .files
            .keys()
            .filter(|p| failures.contains_key(*p))
            .collect();
        if !bad.is_empty() {
            let why = format!(
                "{} pool file(s) could not be fetched, e.g. {}: {}",
                bad.len(),
                bad[0],
                failures[bad[0]]
            );
            (ctx.log)(&format!(
                "{}: FAILED: {why}; the previous indices stay",
                s.dist
            ));
            let _ = fs::remove_dir_all(&s.staging);
            protect.extend(s.files.keys().cloned());
            failed.push(DistReport {
                ok: false,
                error: Some(why),
                ..s.report
            });
            continue;
        }
        let target = dists_dir.join(&s.dist);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        aptpod_core::publish::swap_dir(&s.staging, &target)?;
        (ctx.log)(&format!("{}: switched to the new indices", s.dist));
        report.dists.push(s.report);
    }
    // A configured dist with neither new nor old indices: what it would
    // reference is unknown (e.g. an adopted tree), so nothing is deleted.
    let unknown: Vec<String> = failed
        .iter()
        .filter(|d| {
            let dir = dists_dir.join(&d.dist);
            !dir.join("Release").is_file() && !dir.join("InRelease").is_file()
        })
        .map(|d| d.dist.clone())
        .collect();
    report.dists.extend(failed);
    let order = |d: &str| m.dists.iter().position(|x| x == d);
    report.dists.sort_by_key(|d| order(&d.dist));

    // 4. Pool files no dist on disk references any more.
    if !unknown.is_empty() {
        let w = format!(
            "pool cleanup skipped: no indices for {} yet",
            unknown.join(", ")
        );
        (ctx.log)(&w);
        report.warnings.push(w);
        return Ok(());
    }
    cleanup(m, ctx, report, &protect)
}

fn remove_staging(staged: &[Staged], extra: &Path) {
    for s in staged {
        let _ = fs::remove_dir_all(&s.staging);
    }
    if !extra.as_os_str().is_empty() {
        let _ = fs::remove_dir_all(extra);
    }
}

/// The upstream Release files as fetched: (name, bytes).
type RawFiles = Vec<(&'static str, Vec<u8>)>;

/// Verified Release text of a dist, and the raw files to keep.
fn release_of(m: &HttpMirror, ctx: &Ctx, base: &str) -> Result<(Release, RawFiles), DistErr> {
    let get = |name: &str| match ctx
        .http
        .get_retry(&join_url(base, name), m.retries, ctx.cancel)
    {
        Ok(b) => Ok(Some(b)),
        Err(FetchError::Missing(_)) => Ok(None),
        Err(e) => Err(fe(name, e)),
    };
    let inrelease = get("InRelease")?;
    let release = get("Release")?;
    let gpg = get("Release.gpg")?;
    let sig = |e: aptpod_core::Error| DistErr::Fatal(e.to_string());
    let text = match (&m.keyring, &inrelease, &release, &gpg) {
        (Some(k), Some(ir), _, _) => verify_inrelease(k, ir).map_err(sig)?,
        (Some(k), None, Some(r), Some(g)) => {
            verify_detached(k, r, g).map_err(sig)?;
            String::from_utf8(r.clone())
                .map_err(|_| DistErr::Fatal("Release is not UTF-8".into()))?
        }
        (Some(_), _, _, _) => {
            return Err(DistErr::Fatal(
                "neither InRelease nor Release + Release.gpg is available".into(),
            ));
        }
        (None, _, Some(r), _) => String::from_utf8(r.clone())
            .map_err(|_| DistErr::Fatal("Release is not UTF-8".into()))?,
        (None, Some(ir), None, _) => inrelease_text_unverified(ir).map_err(sig)?,
        (None, None, None, _) => return Err(DistErr::Fatal("no Release file".into())),
    };
    let rel = Release::parse(&text).map_err(|e| DistErr::Fatal(e.to_string()))?;
    // Both forms served: they must describe the same files (a mismatch is
    // an update in progress upstream).
    if let (Some(_), Some(r)) = (&inrelease, &release) {
        if let (Some(k), Some(g)) = (&m.keyring, &gpg) {
            verify_detached(k, r, g).map_err(sig)?;
        }
        let other = std::str::from_utf8(r)
            .ok()
            .and_then(|t| Release::parse(t).ok())
            .ok_or_else(|| DistErr::Fatal("Release cannot be read".into()))?;
        if other.files != rel.files {
            return Err(DistErr::Changed("InRelease and Release differ".into()));
        }
    }
    let mut raw = Vec::new();
    for (name, data) in [
        ("InRelease", inrelease),
        ("Release", release),
        ("Release.gpg", gpg),
    ] {
        if let Some(d) = data {
            raw.push((name, d));
        }
    }
    Ok((rel, raw))
}

/// Fetches Release and the selected indices of `dist` into `staging`;
/// returns the pool files they reference.
fn stage_dist(
    m: &HttpMirror,
    ctx: &Ctx,
    dist: &str,
    staging: &Path,
) -> Result<(BTreeMap<String, PoolFile>, usize, usize), DistErr> {
    let base = join_url(&m.url, &format!("dists/{dist}")) + "/";
    let (rel, raw) = release_of(m, ctx, &base)?;
    let pick = |field: &str, want: &[String], what: &str| -> Result<Vec<String>, DistErr> {
        let listed = rel.list(field);
        if want.is_empty() {
            return Ok(listed
                .into_iter()
                .filter(|x| x != "all" && x != "source")
                .collect());
        }
        if let Some(x) = want.iter().find(|x| !listed.contains(x)) {
            return Err(DistErr::Fatal(format!(
                "{what} {x:?} is not in the dist's Release ({})",
                listed.join(" ")
            )));
        }
        Ok(want.to_vec())
    };
    let comps = pick("Components", &m.components, "component")?;
    let archs = pick("Architectures", &m.architectures, "architecture")?;

    // Index groups: (directory, base name, required, kind).
    let mut groups: Vec<(String, &str, bool)> = Vec::new();
    for c in &comps {
        for a in &archs {
            groups.push((format!("{c}/binary-{a}"), "Packages", true));
            if m.udebs {
                groups.push((
                    format!("{c}/debian-installer/binary-{a}"),
                    "Packages",
                    false,
                ));
            }
        }
        if rel
            .files
            .keys()
            .any(|p| p.starts_with(&format!("{c}/binary-all/")))
        {
            groups.push((format!("{c}/binary-all"), "Packages", false));
        }
        if m.sources {
            groups.push((format!("{c}/source"), "Sources", true));
        }
    }
    let mut todo: BTreeSet<String> = BTreeSet::new();
    for (dir, base_name, _) in &groups {
        for ext in VARIANTS {
            let p = variant_path(dir, base_name, ext);
            if rel.files.contains_key(&p) {
                todo.insert(p);
            }
        }
        let r = format!("{dir}/Release");
        if rel.files.contains_key(&r) {
            todo.insert(r);
        }
    }
    let patterns: Vec<glob::Pattern> = m
        .extras
        .iter()
        .map(|p| glob::Pattern::new(p).expect("checked"))
        .collect();
    for p in rel.files.keys() {
        if patterns.iter().any(|g| g.matches(p)) {
            todo.insert(p.clone());
        }
    }

    // Fetch them; a listed file upstream does not serve is absent.
    fs::create_dir_all(staging).map_err(|e| DistErr::Fatal(e.to_string()))?;
    let by_hash = rel.by_hash();
    let mut got: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for path in &todo {
        let entry = &rel.files[path];
        let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
        let mut urls = vec![];
        if by_hash {
            let sep = if dir.is_empty() { "" } else { "/" };
            urls.push(join_url(
                &base,
                &format!("{dir}{sep}by-hash/SHA256/{}", entry.sha256),
            ));
        }
        urls.push(join_url(&base, path));
        let mut data = None;
        for url in &urls {
            match ctx.http.get_retry(url, m.retries, ctx.cancel) {
                Ok(b) => {
                    data = Some(b);
                    break;
                }
                Err(FetchError::Missing(_)) => continue,
                Err(e) => return Err(fe(path, e)),
            }
        }
        let Some(data) = data else { continue };
        if data.len() as u64 != entry.size || sha256_hex(&data) != entry.sha256 {
            return Err(DistErr::Changed(format!("{path} does not match Release")));
        }
        let out = staging.join(path);
        let w = |p: &Path, d: &[u8]| -> Result<(), DistErr> {
            fs::create_dir_all(p.parent().expect("has parent"))
                .and_then(|()| fs::write(p, d))
                .map_err(|e| DistErr::Fatal(format!("{}: {e}", p.display())))
        };
        w(&out, &data)?;
        if by_hash {
            let sep = if dir.is_empty() { "" } else { "/" };
            w(
                &staging.join(format!("{dir}{sep}by-hash/SHA256/{}", entry.sha256)),
                &data,
            )?;
        }
        got.insert(path.clone(), data);
    }

    // Parse the best variant of every group.
    let mut files: BTreeMap<String, PoolFile> = BTreeMap::new();
    for (dir, base_name, required) in &groups {
        let best = VARIANTS
            .iter()
            .filter(|ext| parseable(ext))
            .find_map(|ext| {
                got.get(&variant_path(dir, base_name, ext))
                    .map(|d| (*ext, d))
            });
        let Some((ext, data)) = best else {
            if *required {
                return Err(DistErr::Fatal(format!(
                    "no usable {dir}/{base_name} index upstream"
                )));
            }
            continue;
        };
        let text = decompress(ext, data).map_err(|e| DistErr::Fatal(format!("{dir}: {e}")))?;
        let list = if *base_name == "Sources" {
            sources_files(&text)
        } else {
            packages_files(&text)
        }
        .map_err(|e| DistErr::Fatal(format!("{dir}: {e}")))?;
        for f in list {
            if f.path.starts_with("dists/") || f.path == "dists" {
                return Err(DistErr::Fatal(format!(
                    "{dir}: {} points into dists/",
                    f.path
                )));
            }
            if let Some(old) = files.get(&f.path)
                && (old.sha256 != f.sha256 || old.size != f.size)
            {
                return Err(DistErr::Fatal(format!(
                    "{} is listed twice with different content",
                    f.path
                )));
            }
            files.insert(f.path.clone(), f);
        }
    }
    for (name, data) in raw {
        fs::write(staging.join(name), data).map_err(|e| DistErr::Fatal(e.to_string()))?;
    }
    let n = files.len();
    Ok((files, got.len(), n))
}

/// Brings every wanted pool file in place; returns the ones that failed.
fn fetch_pool(
    m: &HttpMirror,
    ctx: &Ctx,
    cache: &mut HashCache,
    wanted: &BTreeMap<String, PoolFile>,
    report: &mut SyncReport,
) -> Result<BTreeMap<String, String>, Error> {
    enum Outcome {
        Present,
        Downloaded {
            bytes: u64,
            resumed: bool,
            replaced: bool,
        },
        Ignored,
        Failed(String),
    }
    let items: Vec<&PoolFile> = wanted.values().collect();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let cache = Mutex::new(cache);
    let results: Mutex<Vec<(String, Outcome)>> = Mutex::new(Vec::new());
    let interrupted = AtomicBool::new(false);
    let fatal: Mutex<Option<Error>> = Mutex::new(None);
    let total = items.len();
    std::thread::scope(|scope| {
        for _ in 0..m.parallel.min(total.max(1)) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= total || interrupted.load(Ordering::Relaxed) {
                        return;
                    }
                    let f = items[i];
                    let full = ctx.dest.join(&f.path);
                    let outcome = (|| -> Result<Outcome, Error> {
                        let mut replaced = false;
                        if let Ok(meta) = fs::metadata(&full) {
                            let known = if m.verify_all {
                                None
                            } else {
                                cache.lock().unwrap().get(&f.path, &full).map(String::from)
                            };
                            if known.as_deref() == Some(f.sha256.as_str()) {
                                return Ok(Outcome::Present);
                            }
                            if meta.len() == f.size {
                                let h = sha256_file(&full, ctx.cancel)?;
                                if h == f.sha256 {
                                    cache.lock().unwrap().put(&f.path, &full, &h);
                                    return Ok(Outcome::Present);
                                }
                            }
                            replaced = true;
                        }
                        let url = join_url(&m.url, &f.path);
                        match ctx
                            .http
                            .download(&url, &full, f.size, &f.sha256, m.retries, ctx.cancel)
                        {
                            Ok(d) => {
                                cache.lock().unwrap().put(&f.path, &full, &f.sha256);
                                Ok(Outcome::Downloaded {
                                    bytes: d.bytes,
                                    resumed: d.resumed,
                                    replaced,
                                })
                            }
                            Err(FetchError::Missing(_)) if m.ignore_missing.contains(&f.path) => {
                                Ok(Outcome::Ignored)
                            }
                            Err(FetchError::Interrupted) => Err(Error::Interrupted),
                            Err(FetchError::Local(m)) => Err(Error::Io(std::io::Error::other(m))),
                            Err(e) => Ok(Outcome::Failed(e.to_string())),
                        }
                    })();
                    match outcome {
                        Ok(o) => {
                            results.lock().unwrap().push((f.path.clone(), o));
                            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                            if n % 500 == 0 || n == total {
                                (ctx.log)(&format!("pool: {n}/{total}"));
                            }
                        }
                        Err(Error::Interrupted) => {
                            interrupted.store(true, Ordering::Relaxed);
                            return;
                        }
                        Err(e) => {
                            *fatal.lock().unwrap() = Some(e);
                            interrupted.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = fatal.into_inner().unwrap() {
        return Err(e);
    }
    if interrupted.load(Ordering::Relaxed) || ctx.cancel.load(Ordering::Relaxed) {
        return Err(Error::Interrupted);
    }
    let mut failures = BTreeMap::new();
    for (path, o) in results.into_inner().unwrap() {
        match o {
            Outcome::Present => {}
            Outcome::Downloaded {
                bytes,
                resumed,
                replaced,
            } => {
                report.downloaded += 1;
                report.downloaded_bytes += bytes;
                report.resumed += usize::from(resumed);
                report.replaced += usize::from(replaced);
            }
            Outcome::Ignored => report.ignored_missing.push(path),
            Outcome::Failed(why) => {
                (ctx.log)(&format!("{path}: {why}"));
                failures.insert(path, why);
            }
        }
    }
    report.ignored_missing.sort();
    (ctx.log)(&format!(
        "pool: {} downloaded ({} bytes), {} failed, {} missing and ignored",
        report.downloaded,
        report.downloaded_bytes,
        failures.len(),
        report.ignored_missing.len()
    ));
    Ok(failures)
}

/// Every pool path referenced by the index files under `dists/`, as they
/// are on disk now (new indices of switched dists, old ones of failed
/// dists, and dists this mirror does not manage). `Err` when a set cannot
/// be known; then nothing may be deleted.
pub fn disk_refs(dest: &Path) -> Result<HashSet<String>, String> {
    // (directory, base name) -> variants present
    let mut groups: BTreeMap<(PathBuf, String), Vec<String>> = BTreeMap::new();
    fn walk(
        dir: &Path,
        groups: &mut BTreeMap<(PathBuf, String), Vec<String>>,
    ) -> Result<(), String> {
        let rd = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for e in rd {
            let e = e.map_err(|e| e.to_string())?;
            let name = e.file_name().to_string_lossy().to_string();
            let ft = e.file_type().map_err(|e| e.to_string())?;
            if ft.is_dir() {
                if name != "by-hash" && !name.starts_with('.') {
                    walk(&e.path(), groups)?;
                }
                continue;
            }
            for base in ["Packages", "Sources"] {
                if let Some(rest) = name.strip_prefix(base)
                    && (rest.is_empty() || rest.starts_with('.'))
                {
                    let ext = rest.trim_start_matches('.').to_string();
                    groups
                        .entry((dir.to_path_buf(), base.to_string()))
                        .or_default()
                        .push(ext);
                }
            }
        }
        Ok(())
    }
    let dists = dest.join("dists");
    if dists.is_dir() {
        walk(&dists, &mut groups)?;
    }
    let mut refs = HashSet::new();
    for ((dir, base), exts) in groups {
        let ext = VARIANTS
            .iter()
            .find(|e| parseable(e) && exts.iter().any(|x| x == *e))
            .ok_or_else(|| format!("cannot read {}/{base} ({})", dir.display(), exts.join(", ")))?;
        let p = dir.join(variant_path("", &base, ext).trim_start_matches('/'));
        let data = fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let text = decompress(ext, &data).map_err(|e| format!("{}: {e}", p.display()))?;
        let list = if base == "Sources" {
            sources_files(&text)
        } else {
            packages_files(&text)
        }
        .map_err(|e| format!("{}: {e}", p.display()))?;
        refs.extend(list.into_iter().map(|f| f.path));
    }
    Ok(refs)
}

fn cleanup(
    m: &HttpMirror,
    ctx: &Ctx,
    report: &mut SyncReport,
    protect: &HashSet<String>,
) -> Result<(), Error> {
    let refs = match disk_refs(ctx.dest) {
        Ok(mut r) => {
            r.extend(protect.iter().cloned());
            r
        }
        Err(why) => {
            let w = format!("pool cleanup skipped: {why}");
            (ctx.log)(&w);
            report.warnings.push(w);
            return Ok(());
        }
    };
    let pool = ctx.dest.join("pool");
    let mut unreferenced = Vec::new();
    let mut stale_parts = Vec::new();
    fn walk(
        dir: &Path,
        root: &Path,
        refs: &HashSet<String>,
        out: &mut Vec<PathBuf>,
        parts: &mut Vec<PathBuf>,
    ) -> Result<(), Error> {
        let Ok(rd) = fs::read_dir(dir) else {
            return Ok(());
        };
        for e in rd {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                walk(&p, root, refs, out, parts)?;
                continue;
            }
            let rel = p
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .to_string();
            if let Some(target) = rel.strip_suffix(PART) {
                // Keep partials of wanted files for resuming.
                if !refs.contains(target) {
                    parts.push(p);
                }
            } else if !refs.contains(&rel) {
                out.push(p);
            }
        }
        Ok(())
    }
    walk(&pool, ctx.dest, &refs, &mut unreferenced, &mut stale_parts)?;
    if unreferenced.is_empty() && stale_parts.is_empty() {
        return Ok(());
    }
    let n = unreferenced.len();
    if !m.prune {
        report.would_delete = n;
        (ctx.log)(&format!(
            "{n} unreferenced pool file(s) left in place (prune is off)"
        ));
        return Ok(());
    }
    if n > m.max_delete {
        report.would_delete = n;
        report.brake = true;
        let w = format!(
            "{n} pool files would be deleted, more than max_delete ({}); nothing deleted",
            m.max_delete
        );
        (ctx.log)(&w);
        report.warnings.push(w);
        return Ok(());
    }
    for p in unreferenced.iter().chain(&stale_parts) {
        fs::remove_file(p)?;
    }
    report.deleted = n;
    remove_empty_dirs(&pool)?;
    if n > 0 {
        (ctx.log)(&format!("deleted {n} unreferenced pool file(s)"));
    }
    Ok(())
}

fn remove_empty_dirs(dir: &Path) -> Result<bool, Error> {
    let mut empty = true;
    let Ok(rd) = fs::read_dir(dir) else {
        return Ok(false);
    };
    for e in rd {
        let e = e?;
        if e.file_type()?.is_dir() && remove_empty_dirs(&e.path())? {
            fs::remove_dir(e.path())?;
        } else {
            empty = false;
        }
    }
    Ok(empty)
}
