// SPDX-License-Identifier: GPL-3.0-only

//! SQLite schema and low-level row access.
//!
//! Revisions store each cell as a reference to a *cellset* (a set of
//! package ids identified by its digest), so unchanged cells are shared
//! between revisions.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use aptpod_core::control::parse_paragraph;
use aptpod_core::deb::DebInfo;
use aptpod_core::hash::Hashes;

use crate::Error;
use crate::layout::{Cell, Layout};

pub type Cells = BTreeMap<Cell, BTreeSet<i64>>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS publishes (
    id INTEGER PRIMARY KEY,
    alias TEXT NOT NULL UNIQUE,
    layout TEXT NOT NULL,
    head INTEGER,
    created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS packages (
    id INTEGER PRIMARY KEY,
    sha256 TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    arch TEXT NOT NULL,
    source TEXT NOT NULL,
    size INTEGER NOT NULL,
    md5 TEXT NOT NULL,
    sha1 TEXT NOT NULL,
    sha512 TEXT NOT NULL,
    control TEXT NOT NULL,
    added INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS packages_name ON packages(name);
CREATE TABLE IF NOT EXISTS cellsets (id INTEGER PRIMARY KEY, digest TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS cellset_members (
    cellset INTEGER NOT NULL REFERENCES cellsets(id) ON DELETE CASCADE,
    package INTEGER NOT NULL REFERENCES packages(id),
    PRIMARY KEY (cellset, package)
);
CREATE INDEX IF NOT EXISTS cellset_members_package ON cellset_members(package);
CREATE TABLE IF NOT EXISTS revisions (
    id INTEGER PRIMARY KEY,
    publish INTEGER NOT NULL REFERENCES publishes(id) ON DELETE CASCADE,
    created INTEGER NOT NULL,
    author TEXT NOT NULL,
    message TEXT NOT NULL,
    layout TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS revision_cells (
    revision INTEGER NOT NULL REFERENCES revisions(id) ON DELETE CASCADE,
    dist TEXT NOT NULL,
    component TEXT NOT NULL,
    cellset INTEGER NOT NULL REFERENCES cellsets(id),
    PRIMARY KEY (revision, dist, component)
);
CREATE INDEX IF NOT EXISTS revision_cells_cellset ON revision_cells(cellset);
CREATE TABLE IF NOT EXISTS published (
    publish INTEGER NOT NULL REFERENCES publishes(id) ON DELETE CASCADE,
    dist TEXT NOT NULL,
    revision INTEGER NOT NULL,
    PRIMARY KEY (publish, dist)
);
CREATE TABLE IF NOT EXISTS snapshots (
    id INTEGER PRIMARY KEY,
    publish INTEGER NOT NULL REFERENCES publishes(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    revision INTEGER NOT NULL REFERENCES revisions(id),
    created INTEGER NOT NULL,
    description TEXT NOT NULL,
    UNIQUE (publish, label)
);
";

pub fn open(path: &std::path::Path) -> Result<Connection, Error> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 10_000)?;
    conn.execute_batch(SCHEMA)?;
    conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema', '1')",
        [],
    )?;
    Ok(conn)
}

/// A package row.
#[derive(Debug, Clone)]
pub struct Package {
    pub id: i64,
    pub sha256: String,
    pub name: String,
    pub version: String,
    pub arch: String,
    pub source: String,
    pub size: u64,
    pub md5: String,
    pub sha1: String,
    pub sha512: String,
    pub control: String,
}

impl Package {
    pub fn hashes(&self) -> Hashes {
        Hashes {
            size: self.size,
            md5: self.md5.clone(),
            sha1: self.sha1.clone(),
            sha256: self.sha256.clone(),
            sha512: self.sha512.clone(),
        }
    }

    pub fn deb_info(&self) -> Result<DebInfo, Error> {
        Ok(DebInfo {
            control: parse_paragraph(&self.control)?,
            hashes: self.hashes(),
        })
    }

    /// Pool path of this package in a component of a publish.
    pub fn pool_path(&self, component: &str) -> Result<String, Error> {
        Ok(aptpod_core::pool::pool_path(component, &self.deb_info()?)?)
    }
}

const PACKAGE_COLS: &str =
    "id, sha256, name, version, arch, source, size, md5, sha1, sha512, control";

fn row_to_package(r: &rusqlite::Row<'_>) -> rusqlite::Result<Package> {
    Ok(Package {
        id: r.get(0)?,
        sha256: r.get(1)?,
        name: r.get(2)?,
        version: r.get(3)?,
        arch: r.get(4)?,
        source: r.get(5)?,
        size: r.get::<_, i64>(6)? as u64,
        md5: r.get(7)?,
        sha1: r.get(8)?,
        sha512: r.get(9)?,
        control: r.get(10)?,
    })
}

pub fn package_by_sha(conn: &Connection, sha256: &str) -> Result<Option<Package>, Error> {
    Ok(conn
        .query_row(
            &format!("SELECT {PACKAGE_COLS} FROM packages WHERE sha256 = ?1"),
            [sha256],
            row_to_package,
        )
        .optional()?)
}

/// Loads packages by id.
pub fn packages(
    conn: &Connection,
    ids: impl IntoIterator<Item = i64>,
) -> Result<BTreeMap<i64, Package>, Error> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {PACKAGE_COLS} FROM packages WHERE id = ?1"
    ))?;
    let mut out = BTreeMap::new();
    for id in ids {
        if out.contains_key(&id) {
            continue;
        }
        let p = stmt.query_row([id], row_to_package)?;
        out.insert(id, p);
    }
    Ok(out)
}

pub fn insert_package(conn: &Connection, info: &DebInfo, now: i64) -> Result<i64, Error> {
    let h = &info.hashes;
    conn.execute(
        "INSERT OR IGNORE INTO packages (sha256, name, version, arch, source, size, md5, sha1, sha512, control, added)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![h.sha256, info.name(), info.version(), info.architecture(), info.source(), h.size as i64, h.md5, h.sha1, h.sha512, info.control.to_text(), now],
    )?;
    Ok(conn.query_row(
        "SELECT id FROM packages WHERE sha256 = ?1",
        [&h.sha256],
        |r| r.get(0),
    )?)
}

/// A publish row.
#[derive(Debug, Clone)]
pub struct Publish {
    pub id: i64,
    pub alias: String,
    pub layout: Layout,
    pub head: i64,
}

pub fn publishes(conn: &Connection) -> Result<Vec<Publish>, Error> {
    let mut stmt = conn.prepare("SELECT id, alias, layout, head FROM publishes ORDER BY alias")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, alias, layout, head) = row?;
        out.push(Publish {
            id,
            alias,
            layout: serde_json::from_str(&layout)?,
            head,
        });
    }
    Ok(out)
}

pub fn publish(conn: &Connection, alias: &str) -> Result<Publish, Error> {
    publishes(conn)?
        .into_iter()
        .find(|p| p.alias == alias)
        .ok_or_else(|| Error::NotFound(format!("no publish named {alias:?}")))
}

/// Cells of a revision.
pub fn revision_cells(conn: &Connection, revision: i64) -> Result<Cells, Error> {
    let mut stmt = conn.prepare_cached(
        "SELECT rc.dist, rc.component, m.package FROM revision_cells rc
         LEFT JOIN cellset_members m ON m.cellset = rc.cellset WHERE rc.revision = ?1",
    )?;
    let rows = stmt.query_map([revision], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
        ))
    })?;
    let mut out = Cells::new();
    for row in rows {
        let (d, c, p) = row?;
        let set = out.entry((d, c)).or_default();
        if let Some(p) = p {
            set.insert(p);
        }
    }
    Ok(out)
}

pub fn revision_layout(conn: &Connection, revision: i64) -> Result<Layout, Error> {
    let s: String = conn
        .query_row(
            "SELECT layout FROM revisions WHERE id = ?1",
            [revision],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("no revision {revision}")))?;
    Ok(serde_json::from_str(&s)?)
}

fn cellset_id(tx: &Transaction<'_>, members: &BTreeSet<i64>) -> Result<i64, Error> {
    let bytes: Vec<u8> = members.iter().flat_map(|id| id.to_le_bytes()).collect();
    let digest = aptpod_core::hash::hash_bytes(&bytes).sha256;
    if let Some(id) = tx
        .query_row(
            "SELECT id FROM cellsets WHERE digest = ?1",
            [&digest],
            |r| r.get(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    tx.execute("INSERT INTO cellsets (digest) VALUES (?1)", [&digest])?;
    let id = tx.last_insert_rowid();
    let mut stmt =
        tx.prepare_cached("INSERT INTO cellset_members (cellset, package) VALUES (?1, ?2)")?;
    for p in members {
        stmt.execute(params![id, p])?;
    }
    Ok(id)
}

/// Writes a new revision and moves the publish's head to it.
pub fn insert_revision(
    tx: &Transaction<'_>,
    publish: i64,
    layout: &Layout,
    cells: &Cells,
    author: &str,
    message: &str,
    now: i64,
) -> Result<i64, Error> {
    tx.execute(
        "INSERT INTO revisions (publish, created, author, message, layout) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![publish, now, author, message, serde_json::to_string(layout)?],
    )?;
    let rev = tx.last_insert_rowid();
    for ((d, c), members) in cells {
        let set = cellset_id(tx, members)?;
        tx.execute("INSERT INTO revision_cells (revision, dist, component, cellset) VALUES (?1, ?2, ?3, ?4)", params![rev, d, c, set])?;
    }
    tx.execute(
        "UPDATE publishes SET head = ?1, layout = ?2 WHERE id = ?3",
        params![rev, serde_json::to_string(layout)?, publish],
    )?;
    Ok(rev)
}

/// dist -> revision currently in the tree.
pub fn published(conn: &Connection, publish: i64) -> Result<BTreeMap<String, i64>, Error> {
    let mut stmt =
        conn.prepare_cached("SELECT dist, revision FROM published WHERE publish = ?1")?;
    let rows = stmt.query_map([publish], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn set_published(
    conn: &Connection,
    publish: i64,
    dist: &str,
    revision: Option<i64>,
) -> Result<(), Error> {
    match revision {
        Some(r) => conn.execute(
            "INSERT INTO published (publish, dist, revision) VALUES (?1, ?2, ?3)
             ON CONFLICT (publish, dist) DO UPDATE SET revision = excluded.revision",
            params![publish, dist, r],
        )?,
        None => conn.execute(
            "DELETE FROM published WHERE publish = ?1 AND dist = ?2",
            params![publish, dist],
        )?,
    };
    Ok(())
}
