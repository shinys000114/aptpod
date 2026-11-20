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
}

/// Puts `src` at `root/rel`. An existing file with the same size and
/// sha256 is kept; a different one is a conflict (published files must not
/// change under the same name).
pub fn place(src: &Path, root: &Path, rel: &str, hashes: &Hashes) -> Result<Placed, Error> {
    let dst = root.join(rel);
    if let Ok(meta) = fs::metadata(&dst) {
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
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return place(src, root, rel, hashes),
        Err(_) => {}
    }
    // Copy to a temporary name, then rename, so a crash never leaves a
    // partial file under the final name.
    let tmp = parent.join(format!(
        ".{}.aptpod-tmp",
        dst.file_name().unwrap().to_string_lossy()
    ));
    fs::copy(src, &tmp).map_err(|e| Error::io(&tmp, e))?;
    fs::rename(&tmp, &dst).map_err(|e| Error::io(&dst, e))?;
    Ok(Placed::Copied)
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
    fn place_links_and_detects_conflicts() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("a.deb");
        fs::write(&src, b"one").unwrap();
        let root = d.path().join("root");
        let h = hash_bytes(b"one");
        assert_eq!(
            place(&src, &root, "pool/main/a/a/a.deb", &h).unwrap(),
            Placed::Linked
        );
        assert_eq!(
            place(&src, &root, "pool/main/a/a/a.deb", &h).unwrap(),
            Placed::Present
        );
        let other = d.path().join("b.deb");
        fs::write(&other, b"two").unwrap();
        let err = place(&other, &root, "pool/main/a/a/a.deb", &hash_bytes(b"two")).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");
    }
}
