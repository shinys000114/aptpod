// SPDX-License-Identifier: GPL-3.0-only

//! Repository formats shared by every aptpod component: `.deb` parsing,
//! dpkg version ordering, index writing, signing and the package pool.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub mod control;
pub mod deb;
pub mod hash;
pub mod index;
pub mod pool;
pub mod publish;
pub mod sign;
pub mod term;
pub mod verify;
pub mod version;

/// Helpers for tests of aptpod crates (dpkg-deb fixtures, gpg keys); not
/// part of the API.
#[cfg(any(test, feature = "testutil"))]
#[doc(hidden)]
pub mod testutil;

/// Errors from aptpod-core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    IoAt { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("{0}")]
    Format(String),
    #[error("{}: {msg}", path.display())]
    At { path: PathBuf, msg: String },
    #[error("{0}")]
    Conflict(String),
    #[error("signing failed: {0}")]
    Sign(String),
    #[error("signature verification failed: {0}")]
    Verify(String),
}

impl Error {
    pub(crate) fn io(path: &Path, source: io::Error) -> Self {
        Error::IoAt {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Attaches a path to an error that does not carry one yet.
    pub(crate) fn context(self, path: &Path) -> Self {
        match self {
            e @ (Error::IoAt { .. } | Error::At { .. }) => e,
            e => Error::At {
                path: path.to_path_buf(),
                msg: e.to_string(),
            },
        }
    }
}

/// Version of aptpod, with the git commit when the build provided one
/// (`APTPOD_COMMIT` at build time).
pub fn version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let v = env!("CARGO_PKG_VERSION");
        match option_env!("APTPOD_COMMIT") {
            Some(c) if !c.is_empty() => format!("{v} ({c})"),
            _ => v.to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_starts_with_package_version() {
        assert!(super::version().starts_with(env!("CARGO_PKG_VERSION")));
    }
}
