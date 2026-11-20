// SPDX-License-Identifier: GPL-3.0-only

//! Repository formats shared by every aptpod component: `.deb` parsing,
//! dpkg version ordering, index writing, signing and the package pool.

use std::sync::OnceLock;

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
