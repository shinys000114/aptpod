// SPDX-License-Identifier: GPL-3.0-only

//! Local repository engine: publishes (dist x component layouts), packages
//! in a content store, revisions of every change, snapshot labels, and the
//! published trees that follow the database.

pub mod db;
pub mod filter;
pub mod history;
pub mod layout;
pub mod mirror;
pub mod ops;
pub mod plan;
pub mod repo;
pub mod store;

pub use filter::Filter;
pub use layout::Layout;
pub use plan::{Applied, Change, PackageRef, Plan};
pub use repo::{CellStatus, Repo, Row};

/// Errors from the repository engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Core(#[from] aptpod_core::Error),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Bad input (layout, names, arguments).
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    /// Same pool path, different content.
    #[error("{0}")]
    Conflict(String),
    /// A destructive change that needs `force`.
    #[error("{0}")]
    Blocked(String),
    /// The plan was computed from a head that has moved since.
    #[error("{0}")]
    Stale(String),
    /// The change is committed but a published tree is behind; `repair`.
    #[error("{0}")]
    Unsynced(String),
}
