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

    /// Reads `file` and copies it into the store (if not there yet). The file
    /// is written before the caller records it in the database, so a crash
    /// leaves at most an orphan file.
    pub fn ingest(&self, file: &Path, expected_sha256: Option<&str>) -> Result<DebInfo, Error> {
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
        // Copy (never link): the store must own its files, and the pool links
        // made from them must be world-readable for the web server.
        let tmp = dir.join(format!(
            ".{}.tmp-{}",
            info.hashes.sha256,
            std::process::id()
        ));
        fs::copy(file, &tmp)?;
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
