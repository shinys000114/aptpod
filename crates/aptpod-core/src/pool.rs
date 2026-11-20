// SPDX-License-Identifier: GPL-3.0-only

//! Package pool layout (`pool/<component>/<prefix>/<source>/<file>`) and
//! placing files into a publish tree.
//!
//! The directory rule follows aptly (`deb/package.go`, `PoolDirectory`):
//! `lib*` sources use the first four characters as prefix, others the first.

use std::fs;
use std::io;
use std::path::Path;

use crate::Error;
use crate::deb::DebInfo;
use crate::hash::{Hashes, hash_file};

/// `pool/<component>/<prefix>/<source>`, relative to the publish root.
pub fn pool_dir(component: &str, source: &str) -> Result<String, Error> {
    if source.len() < 2 {
        return Err(Error::Format(format!(
            "source name {source:?} is too short for the pool layout"
        )));
    }
    if !source
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return Err(Error::Format(format!("invalid source name {source:?}")));
    }
    let prefix = if source.starts_with("lib") && source.len() > 3 {
        &source[..4]
    } else {
        &source[..1]
    };
    Ok(format!("pool/{component}/{prefix}/{source}"))
}

/// Pool path of a package, relative to the publish root.
pub fn pool_path(component: &str, deb: &DebInfo) -> Result<String, Error> {
    Ok(format!(
        "{}/{}",
        pool_dir(component, deb.source())?,
        deb.canonical_filename()
    ))
}

/// How a file got into the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placed {
    /// Already present with the same content.
    Present,
    Linked,
    /// Hard links were not possible (e.g. another filesystem, mergerfs).
    Copied,
    /// An existing file with other content was replaced ([`PlaceMode::Replace`]).
    Replaced,
}

/// What to do when the destination already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceMode {
    /// Keep it if size and sha256 match, otherwise refuse (conflict).
    #[default]
    Verify,
    /// The caller knows it put this content there: keep it if the size
    /// matches, without re-hashing (large pools are re-published often).
    Trust,
    /// Replace it with `src` (a deliberate content change under the same
    /// name); clients that cached the old file will see a hash mismatch.
    Replace,
}

/// Puts `src` at `root/rel`. See [`PlaceMode`] for existing destinations;
/// the same inode as `src` always counts as present.
pub fn place(
    src: &Path,
    root: &Path,
    rel: &str,
    hashes: &Hashes,
    mode: PlaceMode,
) -> Result<Placed, Error> {
    let dst = root.join(rel);
    if let Ok(meta) = fs::metadata(&dst) {
        if same_inode(src, &meta) {
            return Ok(Placed::Present);
        }
        match mode {
            PlaceMode::Trust if meta.len() == hashes.size => return Ok(Placed::Present),
            PlaceMode::Replace => {
                write_atomic(src, &dst)?;
                return Ok(Placed::Replaced);
            }
            _ => {}
        }
        if meta.len() == hashes.size
            && hash_file(&dst).map_err(|e| Error::io(&dst, e))?.sha256 == hashes.sha256
        {
            return Ok(Placed::Present);
        }
        return Err(Error::Conflict(format!(
            "{rel} already exists with different content"
        )));
    }
    let parent = dst.parent().expect("pool path has a parent");
    fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    match fs::hard_link(src, &dst) {
        Ok(()) => return Ok(Placed::Linked),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return place(src, root, rel, hashes, mode);
        }
        Err(_) => {}
    }
    write_atomic(src, &dst)?;
    Ok(Placed::Copied)
}

fn same_inode(src: &Path, dst_meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(src).is_ok_and(|m| m.dev() == dst_meta.dev() && m.ino() == dst_meta.ino())
}

/// Links (or copies) `src` to a temporary name next to `dst`, then renames
/// it over `dst`, so readers never see a partial file.
fn write_atomic(src: &Path, dst: &Path) -> Result<(), Error> {
    let parent = dst.parent().expect("has parent");
    let tmp = parent.join(format!(
        ".{}.aptpod-tmp",
        dst.file_name().unwrap().to_string_lossy()
    ));
    let _ = fs::remove_file(&tmp);
    if fs::hard_link(src, &tmp).is_err() {
        fs::copy(src, &tmp).map_err(|e| Error::io(&tmp, e))?;
    }
    fs::rename(&tmp, dst).map_err(|e| Error::io(dst, e))
}

/// Removes files under `root/pool` that are not in `keep` (paths relative to
/// `root`, as written by [`pool_path`]), then empty directories. Only
/// `pool/` is touched. Returns the removed paths.
pub fn cleanup_pool(
    root: &Path,
    keep: &std::collections::HashSet<String>,
) -> Result<Vec<String>, Error> {
    let pool = root.join("pool");
    let mut removed = Vec::new();
    if !pool.is_dir() {
        return Ok(removed);
    }
    fn walk(
        dir: &Path,
        root: &Path,
        keep: &std::collections::HashSet<String>,
        removed: &mut Vec<String>,
    ) -> Result<bool, Error> {
        let mut empty = true;
        for entry in fs::read_dir(dir).map_err(|e| Error::io(dir, e))? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                if walk(&path, root, keep, removed)? {
                    fs::remove_dir(&path).map_err(|e| Error::io(&path, e))?;
                } else {
                    empty = false;
                }
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .to_string();
            if keep.contains(&rel) {
                empty = false;
            } else {
                fs::remove_file(&path).map_err(|e| Error::io(&path, e))?;
                removed.push(rel);
            }
        }
        Ok(empty)
    }
    walk(&pool, root, keep, &mut removed)?;
    removed.sort();
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_bytes;

    #[test]
    fn layout_follows_aptly() {
        assert_eq!(pool_dir("main", "hello").unwrap(), "pool/main/h/hello");
        assert_eq!(pool_dir("main", "libfoo").unwrap(), "pool/main/libf/libfoo");
        assert_eq!(pool_dir("rk3588", "lib").unwrap(), "pool/rk3588/l/lib");
        assert_eq!(
            pool_dir("main", "linux-image-6.6").unwrap(),
            "pool/main/l/linux-image-6.6"
        );
        assert!(pool_dir("main", "k").is_err());
        assert!(pool_dir("main", "../x").is_err());
    }

    #[test]
    fn place_modes() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        let rel = "pool/main/a/a/a.deb";
        let src = d.path().join("a.deb");
        fs::write(&src, b"one").unwrap();
        let h = hash_bytes(b"one");
        assert_eq!(
            place(&src, &root, rel, &h, PlaceMode::Verify).unwrap(),
            Placed::Linked
        );
        assert_eq!(
            place(&src, &root, rel, &h, PlaceMode::Verify).unwrap(),
            Placed::Present,
            "same inode"
        );

        // a copy elsewhere with the same content is verified by hash
        let copy = d.path().join("copy.deb");
        fs::write(&copy, b"one").unwrap();
        assert_eq!(
            place(&copy, &root, rel, &h, PlaceMode::Verify).unwrap(),
            Placed::Present
        );

        let other = d.path().join("b.deb");
        fs::write(&other, b"two").unwrap();
        let h2 = hash_bytes(b"two");
        let err = place(&other, &root, rel, &h2, PlaceMode::Verify).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");
        // Trust only compares sizes (both 3 bytes): no re-hash, no conflict
        assert_eq!(
            place(&other, &root, rel, &h2, PlaceMode::Trust).unwrap(),
            Placed::Present
        );
        assert_eq!(fs::read(root.join(rel)).unwrap(), b"one");
        assert_eq!(
            place(&other, &root, rel, &h2, PlaceMode::Replace).unwrap(),
            Placed::Replaced
        );
        assert_eq!(fs::read(root.join(rel)).unwrap(), b"two");
    }

    #[test]
    fn cleanup_only_touches_pool() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        for f in [
            "pool/main/a/a/keep.deb",
            "pool/main/b/b/old.deb",
            "dists/x/Release",
            "README",
        ] {
            let p = root.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, f).unwrap();
        }
        let keep: std::collections::HashSet<String> = ["pool/main/a/a/keep.deb".to_string()].into();
        assert_eq!(
            cleanup_pool(root, &keep).unwrap(),
            vec!["pool/main/b/b/old.deb".to_string()]
        );
        assert!(root.join("pool/main/a/a/keep.deb").is_file());
        assert!(!root.join("pool/main/b").exists(), "empty dirs removed");
        assert!(root.join("dists/x/Release").is_file() && root.join("README").is_file());
    }
}
