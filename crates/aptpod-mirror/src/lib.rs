// SPDX-License-Identifier: GPL-3.0-only

//! Mirror engine: verbatim (HTTP, rsync) and re-signed mirrors.

pub mod cache;
pub mod fetch;
pub mod http;
pub mod release;

/// Errors that stop a whole sync. Problems of one dist are reported in
/// [`http::DistReport`] instead.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Core(#[from] aptpod_core::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Http(String),
    /// Upstream data that cannot be used (bad paths, missing checksums).
    #[error("{0}")]
    Upstream(String),
    #[error("signature: {0}")]
    Verify(String),
    #[error("interrupted")]
    Interrupted,
}

/// A path from upstream data, relative and without `.`, `..` or empty
/// components.
pub fn safe_rel(p: &str) -> Result<(), Error> {
    let bad = p.is_empty()
        || p.starts_with('/')
        || p.contains('\\')
        || p.contains('\0')
        || p.split('/').any(|c| c.is_empty() || c == "." || c == "..");
    if bad {
        return Err(Error::Upstream(format!(
            "unsafe path {p:?} in upstream data"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn safe_paths() {
        for ok in ["pool/main/a/a_1.deb", "main/binary-amd64/Packages.gz", "x"] {
            assert!(super::safe_rel(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "pool/../../x",
            "pool//x",
            "./x",
            "a/.",
            "a\\b",
        ] {
            assert!(super::safe_rel(bad).is_err(), "{bad}");
        }
    }
}
