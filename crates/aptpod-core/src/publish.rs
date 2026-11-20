// SPDX-License-Identifier: GPL-3.0-only

//! Writing a dist into a publish tree:
//!
//! ```text
//! <root>/pool/<component>/<prefix>/<source>/<file>.deb
//! <root>/dists/<dist>/{Release,InRelease,Release.gpg}
//! <root>/dists/<dist>/<component>/binary-<arch>/{Packages,Packages.gz,Packages.xz,Release}
//! ```
//!
//! Pool files are placed first, then the whole `dists/<dist>` directory is
//! built under a temporary name and swapped in, so clients never see
//! indices that point at missing files or a half-written dist.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::deb::DebInfo;
use crate::hash::hash_bytes;
use crate::index::{
    IndexFile, IndexPackage, ReleaseMeta, component_release, gzip, packages_text, release_text, xz,
};
use crate::pool::{PlaceMode, Placed, place, pool_path};
use crate::sign::Signer;

/// A package to publish and where its bytes are.
#[derive(Debug, Clone)]
pub struct PublishPackage {
    pub info: DebInfo,
    /// Where the bytes are (e.g. the content store).
    pub file: PathBuf,
    /// How to treat an existing pool file of the same name.
    pub mode: PlaceMode,
}

#[derive(Debug, Clone)]
pub struct ComponentSpec {
    pub name: String,
    pub packages: Vec<PublishPackage>,
}

/// One dist to publish.
#[derive(Debug, Clone)]
pub struct DistSpec {
    pub name: String,
    pub meta: ReleaseMeta,
    pub architectures: Vec<String>,
    pub components: Vec<ComponentSpec>,
}

/// What [`publish_dist`] did.
#[derive(Debug, Default)]
pub struct PublishReport {
    pub pool_linked: usize,
    pub pool_copied: usize,
    pub pool_present: usize,
    pub pool_replaced: usize,
    /// Pool files used by this dist, relative to the root.
    pub pool_files: Vec<String>,
    /// Index files written, relative to `dists/<dist>/`.
    pub files: Vec<String>,
    /// Whether the dist directory was swapped atomically.
    pub atomic_swap: bool,
}

fn check_name(kind: &str, s: &str) -> Result<(), Error> {
    let ok = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-._~".contains(c))
        && !s.starts_with('.');
    if ok {
        Ok(())
    } else {
        Err(Error::Format(format!("invalid {kind} name {s:?}")))
    }
}

/// Publishes `spec` under `root`. `signer = None` writes an unsigned dist
/// (only `Release`).
pub fn publish_dist(
    root: &Path,
    spec: &DistSpec,
    signer: Option<&dyn Signer>,
) -> Result<PublishReport, Error> {
    check_name("dist", &spec.name)?;
    if spec.architectures.is_empty() {
        return Err(Error::Format(
            "a dist needs at least one architecture".into(),
        ));
    }
    for a in &spec.architectures {
        check_name("architecture", a)?;
        if a == "all" {
            return Err(Error::Format("'all' is not a publish architecture; all-arch packages go into every binary-<arch>".into()));
        }
    }
    let mut report = PublishReport::default();

    // 1. Pool files.
    let mut comps: Vec<(String, Vec<IndexPackage>)> = Vec::new();
    for comp in &spec.components {
        check_name("component", &comp.name)?;
        let mut pkgs = Vec::new();
        for p in &comp.packages {
            let arch = p.info.architecture();
            if arch != "all" && !spec.architectures.iter().any(|a| a == arch) {
                return Err(Error::Format(format!(
                    "{} {} is {arch}, which dist {} does not publish",
                    p.info.name(),
                    p.info.version(),
                    spec.name
                )));
            }
            let rel = pool_path(&comp.name, &p.info)?;
            match place(&p.file, root, &rel, &p.info.hashes, p.mode)? {
                Placed::Linked => report.pool_linked += 1,
                Placed::Copied => report.pool_copied += 1,
                Placed::Present => report.pool_present += 1,
                Placed::Replaced => report.pool_replaced += 1,
            }
            report.pool_files.push(rel.clone());
            pkgs.push(IndexPackage {
                control: p.info.control.clone(),
                filename: rel,
                hashes: p.info.hashes.clone(),
            });
        }
        comps.push((comp.name.clone(), pkgs));
    }

    // 2. Indices in a temporary dist directory.
    let dists = root.join("dists");
    fs::create_dir_all(&dists).map_err(|e| Error::io(&dists, e))?;
    let tmp = dists.join(format!(".aptpod-tmp-{}-{}", spec.name, std::process::id()));
    if tmp.exists() {
        fs::remove_dir_all(&tmp).map_err(|e| Error::io(&tmp, e))?;
    }
    let result = write_indices(&tmp, spec, &comps, signer, &mut report);
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }

    // 3. Swap it in.
    report.atomic_swap = swap_in(&tmp, &dists.join(&spec.name))?;
    Ok(report)
}

fn write_indices(
    dir: &Path,
    spec: &DistSpec,
    comps: &[(String, Vec<IndexPackage>)],
    signer: Option<&dyn Signer>,
    report: &mut PublishReport,
) -> Result<(), Error> {
    let mut files = Vec::new();
    let mut write = |rel: String, data: &[u8]| -> Result<(), Error> {
        let path = dir.join(&rel);
        let parent = path.parent().expect("has parent");
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        fs::write(&path, data).map_err(|e| Error::io(&path, e))?;
        files.push(IndexFile {
            path: rel,
            hashes: hash_bytes(data),
        });
        Ok(())
    };
    for (comp, pkgs) in comps {
        for arch in &spec.architectures {
            let sel: Vec<&IndexPackage> = pkgs
                .iter()
                .filter(|p| p.architecture() == arch || p.architecture() == "all")
                .collect();
            let text = packages_text(&sel);
            let base = format!("{comp}/binary-{arch}");
            write(format!("{base}/Packages"), text.as_bytes())?;
            write(format!("{base}/Packages.gz"), &gzip(text.as_bytes())?)?;
            write(format!("{base}/Packages.xz"), &xz(text.as_bytes())?)?;
            write(
                format!("{base}/Release"),
                component_release(&spec.meta, comp, arch).as_bytes(),
            )?;
        }
    }
    let comp_names: Vec<String> = comps.iter().map(|(c, _)| c.clone()).collect();
    let release = release_text(&spec.meta, &spec.architectures, &comp_names, &files);
    report.files = files.iter().map(|f| f.path.clone()).collect();
    fs::write(dir.join("Release"), &release).map_err(|e| Error::io(&dir.join("Release"), e))?;
    report.files.push("Release".into());
    if let Some(s) = signer {
        fs::write(dir.join("InRelease"), s.clearsign(release.as_bytes())?)?;
        fs::write(dir.join("Release.gpg"), s.detach_sign(release.as_bytes())?)?;
        report.files.push("InRelease".into());
        report.files.push("Release.gpg".into());
    }
    Ok(())
}

/// Replaces `target` by `new`. Uses `RENAME_EXCHANGE` (one atomic step);
/// filesystems without it (e.g. some FUSE ones) fall back to two renames.
fn swap_in(new: &Path, target: &Path) -> Result<bool, Error> {
    if !target.exists() {
        fs::rename(new, target).map_err(|e| Error::io(target, e))?;
        return Ok(true);
    }
    let atomic = rustix::fs::renameat_with(
        rustix::fs::CWD,
        new,
        rustix::fs::CWD,
        target,
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .is_ok();
    if atomic {
        // `new` now holds the previous generation.
        fs::remove_dir_all(new).map_err(|e| Error::io(new, e))?;
        return Ok(true);
    }
    let old = target.with_file_name(format!(
        ".aptpod-old-{}-{}",
        target.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    fs::rename(target, &old).map_err(|e| Error::io(&old, e))?;
    if let Err(e) = fs::rename(new, target) {
        let _ = fs::rename(&old, target);
        return Err(Error::io(target, e));
    }
    fs::remove_dir_all(&old).map_err(|e| Error::io(&old, e))?;
    Ok(false)
}

/// Removes `dists/<dist>` (a dist this tree no longer publishes).
pub fn remove_dist(root: &Path, dist: &str) -> Result<bool, Error> {
    check_name("dist", dist)?;
    let dir = root.join("dists").join(dist);
    if !dir.exists() {
        return Ok(false);
    }
    fs::remove_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
    Ok(true)
}

/// Removes what an interrupted publish can leave behind: temporary and
/// old dist directories in `dists/` and temporary pool files.
pub fn clean_leftovers(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut removed = Vec::new();
    let dists = root.join("dists");
    if let Ok(rd) = fs::read_dir(&dists) {
        for e in rd {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(".aptpod-tmp-") || name.starts_with(".aptpod-old-") {
                fs::remove_dir_all(e.path()).map_err(|err| Error::io(&e.path(), err))?;
                removed.push(e.path());
            }
        }
    }
    fn walk(dir: &Path, removed: &mut Vec<PathBuf>) -> Result<(), Error> {
        let Ok(rd) = fs::read_dir(dir) else {
            return Ok(());
        };
        for e in rd {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                walk(&p, removed)?;
            } else if e.file_name().to_string_lossy().ends_with(".aptpod-tmp") {
                fs::remove_file(&p).map_err(|err| Error::io(&p, err))?;
                removed.push(p);
            }
        }
        Ok(())
    }
    walk(&root.join("pool"), &mut removed)?;
    Ok(removed)
}
