// SPDX-License-Identifier: GPL-3.0-only

//! Revision history, snapshot labels, diffs, retention and garbage
//! collection.

use std::collections::BTreeSet;

use rusqlite::params;
use serde::Serialize;

use crate::Error;
use crate::db;
use crate::ops::RevRef;
use crate::plan::PackageRef;
use crate::repo::{Repo, package_ref};

#[derive(Debug, Clone, Serialize)]
pub struct RevisionInfo {
    pub id: i64,
    pub created: i64,
    pub author: String,
    pub message: String,
    pub labels: Vec<String>,
    pub head: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotInfo {
    pub label: String,
    pub revision: i64,
    pub created: i64,
    pub description: String,
}

/// One difference between two revisions: `+` present only in `to`.
#[derive(Debug, Clone, Serialize)]
pub struct DiffEntry {
    pub dist: String,
    pub component: String,
    pub change: char,
    #[serde(flatten)]
    pub package: PackageRef,
}

/// Seconds a freshly ingested, still unreferenced package survives `gc`.
pub const GC_GRACE: i64 = 3600;

#[derive(Debug, Clone, Default, Serialize)]
pub struct GcReport {
    pub cellsets: usize,
    pub packages: usize,
}

impl Repo {
    /// Newest first.
    pub fn history(&self, alias: &str, limit: usize) -> Result<Vec<RevisionInfo>, Error> {
        let p = db::publish(&self.conn, alias)?;
        let mut stmt = self.conn.prepare("SELECT id, created, author, message FROM revisions WHERE publish = ?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = stmt.query_map(params![p.id, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, created, author, message) = row?;
            out.push(RevisionInfo {
                id,
                created,
                author,
                message,
                labels: self.labels_of(p.id, id)?,
                head: id == p.head,
            });
        }
        Ok(out)
    }

    fn labels_of(&self, publish: i64, rev: i64) -> Result<Vec<String>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT label FROM snapshots WHERE publish = ?1 AND revision = ?2 ORDER BY label",
        )?;
        let rows = stmt.query_map(params![publish, rev], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Labels a revision (default: head).
    pub fn snapshot_create(
        &mut self,
        alias: &str,
        label: &str,
        description: &str,
        at: &RevRef,
    ) -> Result<i64, Error> {
        if label.is_empty()
            || label.starts_with('#')
            || label == "HEAD"
            || !label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        {
            return Err(Error::Invalid(format!("invalid snapshot label {label:?}")));
        }
        let p = db::publish(&self.conn, alias)?;
        let rev = self.resolve(&p, at)?;
        let now = self.now();
        self.conn
            .execute(
                "INSERT INTO snapshots (publish, label, revision, created, description) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![p.id, label, rev, now, description],
            )
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
                    Error::Invalid(format!("snapshot {label:?} already exists in publish {alias:?}"))
                }
                e => e.into(),
            })?;
        Ok(rev)
    }

    /// Newest first.
    pub fn snapshots(&self, alias: &str) -> Result<Vec<SnapshotInfo>, Error> {
        let p = db::publish(&self.conn, alias)?;
        let mut stmt = self.conn.prepare("SELECT label, revision, created, description FROM snapshots WHERE publish = ?1 ORDER BY created DESC, id DESC")?;
        let rows = stmt.query_map([p.id], |r| {
            Ok(SnapshotInfo {
                label: r.get(0)?,
                revision: r.get(1)?,
                created: r.get(2)?,
                description: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn snapshot_drop(&mut self, alias: &str, label: &str) -> Result<(), Error> {
        let p = db::publish(&self.conn, alias)?;
        let n = self.conn.execute(
            "DELETE FROM snapshots WHERE publish = ?1 AND label = ?2",
            params![p.id, label],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!(
                "no snapshot labelled {label:?} in publish {alias:?}"
            )));
        }
        Ok(())
    }

    /// Labels older than `older_than` seconds, beyond the newest
    /// `keep_last`; dropped unless `dry_run`.
    pub fn snapshot_prune(
        &mut self,
        alias: &str,
        older_than: Option<i64>,
        keep_last: usize,
        dry_run: bool,
    ) -> Result<Vec<SnapshotInfo>, Error> {
        let now = self.now();
        let victims: Vec<SnapshotInfo> = self
            .snapshots(alias)?
            .into_iter()
            .skip(keep_last)
            .filter(|s| older_than.is_none_or(|age| s.created <= now - age))
            .collect();
        if !dry_run {
            for s in &victims {
                self.snapshot_drop(alias, &s.label)?;
            }
        }
        Ok(victims)
    }

    /// What changed from `from` to `to`.
    pub fn diff(&self, alias: &str, from: &RevRef, to: &RevRef) -> Result<Vec<DiffEntry>, Error> {
        let p = db::publish(&self.conn, alias)?;
        let (a, b) = (
            db::revision_cells(&self.conn, self.resolve(&p, from)?)?,
            db::revision_cells(&self.conn, self.resolve(&p, to)?)?,
        );
        let mut ids: BTreeSet<i64> = a.values().flatten().copied().collect();
        ids.extend(b.values().flatten().copied());
        let pkgs = db::packages(&self.conn, ids)?;
        let empty = BTreeSet::new();
        let cells: BTreeSet<_> = a.keys().chain(b.keys()).collect();
        let mut out = Vec::new();
        for cell in cells {
            let (x, y) = (a.get(cell).unwrap_or(&empty), b.get(cell).unwrap_or(&empty));
            for (set, other, ch) in [(x, y, '-'), (y, x, '+')] {
                for id in set.difference(other) {
                    out.push(DiffEntry {
                        dist: cell.0.clone(),
                        component: cell.1.clone(),
                        change: ch,
                        package: package_ref(&pkgs[id]),
                    });
                }
            }
        }
        let pos = |d: &str| {
            p.layout
                .dists
                .iter()
                .position(|x| x == d)
                .unwrap_or(usize::MAX)
        };
        out.sort_by(|l, r| {
            pos(&l.dist)
                .cmp(&pos(&r.dist))
                .then_with(|| l.component.cmp(&r.component))
                .then_with(|| l.package.name.cmp(&r.package.name))
                .then_with(|| aptpod_core::version::compare(&r.package.version, &l.package.version))
                .then_with(|| l.change.cmp(&r.change))
        });
        Ok(out)
    }

    /// Forgets revisions older than the publish's `history_max_age`, except
    /// the head, labelled ones and ones a dist is published from.
    pub fn expire_history(&mut self, alias: &str) -> Result<usize, Error> {
        let p = db::publish(&self.conn, alias)?;
        let Some(age) = p.layout.history_max_age else {
            return Ok(0);
        };
        let cutoff = self.now() - age as i64;
        let n = self.conn.execute(
            "DELETE FROM revisions WHERE publish = ?1 AND created < ?2 AND id != ?3
               AND id NOT IN (SELECT revision FROM snapshots WHERE publish = ?1)
               AND id NOT IN (SELECT revision FROM published WHERE publish = ?1)",
            params![p.id, cutoff, p.head],
        )?;
        Ok(n)
    }

    /// Removes cell sets and packages no revision references; package rows
    /// go before their files, so a crash leaves orphan files, never rows
    /// without files. Packages ingested in the last [`GC_GRACE`] seconds are
    /// kept: a plan that is still waiting to be applied may refer to them.
    pub fn gc(&mut self) -> Result<GcReport, Error> {
        let cutoff = self.now() - GC_GRACE;
        let tx = self.conn.transaction()?;
        let cellsets = tx.execute(
            "DELETE FROM cellsets WHERE id NOT IN (SELECT cellset FROM revision_cells)",
            [],
        )?;
        let shas: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT sha256 FROM packages
                 WHERE added < ?1 AND id NOT IN (SELECT package FROM cellset_members)",
            )?;
            stmt.query_map([cutoff], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        tx.execute(
            "DELETE FROM packages
             WHERE added < ?1 AND id NOT IN (SELECT package FROM cellset_members)",
            [cutoff],
        )?;
        tx.commit()?;
        for sha in &shas {
            self.store.remove(sha)?;
        }
        Ok(GcReport {
            cellsets,
            packages: shas.len(),
        })
    }
}
