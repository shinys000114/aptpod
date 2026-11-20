// SPDX-License-Identifier: GPL-3.0-only

//! Content store: every package file once, named by its sha256
//! (`<data_dir>/store/<aa>/<sha256>.deb`). Publish trees hard-link from here.

use std::fs;
use std::path::{Path, PathBuf};

use aptpod_core::deb::{DebInfo, read_deb};

use crate::Error;

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn path(&self, sha256: &str) -> PathBuf {
        self.root.join(&sha256[..2]).join(format!("{sha256}.deb"))
    }

    /// Reads `file` and copies it into the store (if not there yet); an
    /// `owned` file (one nobody else will change, such as a server upload)
    /// is hard-linked when it is on the same filesystem. The file is
    /// written before the caller records it in the database, so a crash
    /// leaves at most an orphan file.
    pub fn ingest(
        &self,
        file: &Path,
        expected_sha256: Option<&str>,
        owned: bool,
    ) -> Result<DebInfo, Error> {
        let info = read_deb(file)?;
        if let Some(want) = expected_sha256
            && !want.eq_ignore_ascii_case(&info.hashes.sha256)
        {
            return Err(Error::Invalid(format!(
                "{}: sha256 {} does not match the expected {want}",
                file.display(),
                info.hashes.sha256
            )));
        }
        let dst = self.path(&info.hashes.sha256);
        if dst.is_file() {
            return Ok(info);
        }
        let dir = dst.parent().expect("has parent");
        fs::create_dir_all(dir)?;
        // A caller's file is copied: the store must own its files. The pool
        // links made from them must be world-readable for the web server.
        let tmp = dir.join(format!(
            ".{}.tmp-{}-{:?}",
            info.hashes.sha256,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_file(&tmp);
        if !(owned && fs::hard_link(file, &tmp).is_ok()) {
            fs::copy(file, &tmp)?;
        }
        fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
        fs::rename(&tmp, &dst)?;
        Ok(info)
    }

    /// Deletes a stored file (after its database row is gone).
    pub fn remove(&self, sha256: &str) -> Result<(), Error> {
        match fs::remove_file(self.path(sha256)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn owned_files_are_linked_others_copied() {
        if !aptpod_core::testutil::have("dpkg-deb") {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let a = aptpod_core::testutil::build_deb(d.path(), "aa", "1.0", "all", "xz", "");
        let b = aptpod_core::testutil::build_deb(d.path(), "bb", "1.0", "all", "xz", "");
        let store = Store::new(d.path().join("store"));
        let ia = store.ingest(&a, None, true).unwrap();
        let ib = store.ingest(&b, None, false).unwrap();
        let ino = |p: &Path| fs::metadata(p).unwrap().ino();
        assert_eq!(ino(&a), ino(&store.path(&ia.hashes.sha256)), "linked");
        assert_ne!(ino(&b), ino(&store.path(&ib.hashes.sha256)), "copied");
        let mode = fs::metadata(store.path(&ia.hashes.sha256)).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }
}
