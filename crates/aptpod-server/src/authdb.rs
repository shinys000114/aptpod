// SPDX-License-Identifier: GPL-3.0-only

//! The server database (`server.db`, apart from the repository database):
//! users, API tokens and the job log. Offline admin commands
//! (`aptpod-server user ...`) open only this database, so they work next to
//! a running server.

use std::path::Path;
use std::sync::Mutex;

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    password TEXT NOT NULL,
    role TEXT NOT NULL,
    created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tokens (
    id INTEGER PRIMARY KEY,
    user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    hash TEXT NOT NULL UNIQUE,
    created INTEGER NOT NULL,
    last_used INTEGER
);
CREATE TABLE IF NOT EXISTS mirrors (
    name TEXT PRIMARY KEY,
    def TEXT NOT NULL,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS mirror_status (
    name TEXT PRIMARY KEY,
    job INTEGER,
    started INTEGER,
    finished INTEGER,
    state TEXT,
    summary TEXT,
    report TEXT
);
CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    resource TEXT NOT NULL,
    user TEXT NOT NULL,
    state TEXT NOT NULL,
    created INTEGER NOT NULL,
    finished INTEGER,
    summary TEXT NOT NULL DEFAULT '',
    error TEXT
);
";

/// What a user may do; each role includes the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Read-only API access.
    Reader,
    /// Adds, removes and promotes packages; uploads.
    Publisher,
    /// Creates, edits and deletes publishes; manages users; gc, repair.
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Reader => "reader",
            Role::Publisher => "publisher",
            Role::Admin => "admin",
        }
    }
}

impl std::str::FromStr for Role {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "reader" => Ok(Role::Reader),
            "publisher" => Ok(Role::Publisher),
            "admin" => Ok(Role::Admin),
            _ => Err(format!("unknown role {s:?} (reader, publisher or admin)")),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenInfo {
    pub id: i64,
    pub name: String,
    pub created: i64,
    pub last_used: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub resource: String,
    pub user: String,
    pub state: String,
    pub created: i64,
    pub finished: Option<i64>,
    pub summary: String,
    pub error: Option<String>,
}

pub struct AuthDb {
    conn: Mutex<Connection>,
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A random hex string of `n` bytes.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("system random source");
    hex(&buf)
}

fn token_hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

pub fn check_user_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c))
    {
        return Err(format!("invalid user name {name:?}"));
    }
    Ok(())
}

pub fn hash_password(password: &str) -> Result<String, String> {
    if password.len() < 8 {
        return Err("the password must have at least 8 characters".into());
    }
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| format!("hashing the password: {e}"))
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| {
        Argon2::default()
            .verify_password(password.as_bytes(), &h)
            .is_ok()
    })
}

/// Hash of a password nobody has, checked when the user does not exist so
/// that a failed login takes the same time either way.
fn dummy_hash() -> &'static str {
    static H: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    H.get_or_init(|| hash_password(&random_hex(16)).expect("hashing"))
}

type R<T> = Result<T, rusqlite::Error>;

impl AuthDb {
    pub fn open(data_dir: &Path) -> R<Self> {
        let path = data_dir.join("server.db");
        let conn = Connection::open(&path)?;
        // Password and token hashes: owner only.
        let _ =
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 10_000)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub(crate) fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn user_add(&self, name: &str, password_hash: &str, role: Role) -> R<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO users (name, password, role, created) VALUES (?1, ?2, ?3, ?4)",
            params![name, password_hash, role.as_str(), now()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn user(&self, name: &str) -> R<Option<User>> {
        self.conn()
            .query_row(
                "SELECT id, name, role FROM users WHERE name = ?1",
                [name],
                row_user,
            )
            .optional()
    }

    pub fn users(&self) -> R<Vec<User>> {
        let c = self.conn();
        let mut stmt = c.prepare("SELECT id, name, role FROM users ORDER BY name")?;
        let rows = stmt.query_map([], row_user)?;
        rows.collect()
    }

    /// Removes a user and their tokens; false if there was none.
    pub fn user_del(&self, name: &str) -> R<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM users WHERE name = ?1", [name])?
            > 0)
    }

    pub fn set_password(&self, name: &str, password_hash: &str) -> R<bool> {
        Ok(self.conn().execute(
            "UPDATE users SET password = ?2 WHERE name = ?1",
            params![name, password_hash],
        )? > 0)
    }

    pub fn set_role(&self, name: &str, role: Role) -> R<bool> {
        Ok(self.conn().execute(
            "UPDATE users SET role = ?2 WHERE name = ?1",
            params![name, role.as_str()],
        )? > 0)
    }

    pub fn admin_count(&self) -> R<i64> {
        self.conn()
            .query_row("SELECT count(*) FROM users WHERE role = 'admin'", [], |r| {
                r.get(0)
            })
    }

    /// Checks a password; slow on purpose (argon2), call off the async
    /// runtime.
    pub fn check_password(&self, name: &str, password: &str) -> R<Option<User>> {
        let row: Option<(String, User)> = self
            .conn()
            .query_row(
                "SELECT password, id, name, role FROM users WHERE name = ?1",
                [name],
                |r| {
                    Ok((
                        r.get(0)?,
                        User {
                            id: r.get(1)?,
                            name: r.get(2)?,
                            role: parse_role(r, 3)?,
                        },
                    ))
                },
            )
            .optional()?;
        Ok(match row {
            Some((hash, user)) => verify_password(password, &hash).then_some(user),
            None => {
                verify_password(password, dummy_hash());
                None
            }
        })
    }

    /// Creates a token; the plain token is returned once and only its hash
    /// is kept.
    pub fn token_create(&self, user: i64, name: &str) -> R<String> {
        let token = format!("aptpod_{}", random_hex(32));
        self.conn().execute(
            "INSERT INTO tokens (user, name, hash, created) VALUES (?1, ?2, ?3, ?4)",
            params![user, name, token_hash(&token), now()],
        )?;
        Ok(token)
    }

    pub fn authenticate(&self, token: &str) -> R<Option<User>> {
        let c = self.conn();
        let hash = token_hash(token);
        let user = c
            .query_row(
                "SELECT u.id, u.name, u.role FROM tokens t JOIN users u ON u.id = t.user
                 WHERE t.hash = ?1",
                [&hash],
                row_user,
            )
            .optional()?;
        if user.is_some() {
            c.execute(
                "UPDATE tokens SET last_used = ?2 WHERE hash = ?1",
                params![hash, now()],
            )?;
        }
        Ok(user)
    }

    pub fn tokens(&self, user: i64) -> R<Vec<TokenInfo>> {
        let c = self.conn();
        let mut stmt = c.prepare(
            "SELECT id, name, created, last_used FROM tokens WHERE user = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([user], |r| {
            Ok(TokenInfo {
                id: r.get(0)?,
                name: r.get(1)?,
                created: r.get(2)?,
                last_used: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Revokes the token presented in a request.
    pub fn token_revoke_plain(&self, token: &str) -> R<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM tokens WHERE hash = ?1", [token_hash(token)])?
            > 0)
    }

    pub fn token_revoke(&self, user: i64, id: i64) -> R<bool> {
        Ok(self.conn().execute(
            "DELETE FROM tokens WHERE user = ?1 AND id = ?2",
            params![user, id],
        )? > 0)
    }

    pub fn job_start(&self, kind: &str, resource: &str, user: &str) -> R<i64> {
        let c = self.conn();
        c.execute(
            "INSERT INTO jobs (kind, resource, user, state, created) VALUES (?1, ?2, ?3, 'running', ?4)",
            params![kind, resource, user, now()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn job_finish(&self, id: i64, result: Result<&str, &str>) -> R<()> {
        let (state, summary, error) = match result {
            Ok(s) => ("done", s, None),
            Err(e) => ("failed", "", Some(e)),
        };
        self.job_end(id, state, summary, error)
    }

    /// Ends a job in `state` (done, failed, interrupted).
    pub fn job_end(&self, id: i64, state: &str, summary: &str, error: Option<&str>) -> R<()> {
        self.conn().execute(
            "UPDATE jobs SET state = ?2, finished = ?3, summary = ?4, error = ?5 WHERE id = ?1",
            params![id, state, now(), summary, error],
        )?;
        Ok(())
    }

    /// Jobs left running by a previous process.
    pub fn jobs_interrupted(&self) -> R<usize> {
        self.conn().execute(
            "UPDATE jobs SET state = 'interrupted', finished = ?1 WHERE state = 'running'",
            [now()],
        )
    }

    pub fn job(&self, id: i64) -> R<Option<Job>> {
        Ok(self.jobs_where("WHERE id = ?1", id, 1)?.pop())
    }

    pub fn jobs(&self, limit: usize) -> R<Vec<Job>> {
        self.jobs_where("WHERE ?1 = ?1", 0, limit)
    }

    fn jobs_where(&self, cond: &str, arg: i64, limit: usize) -> R<Vec<Job>> {
        let c = self.conn();
        let mut stmt = c.prepare(&format!(
            "SELECT id, kind, resource, user, state, created, finished, summary, error
             FROM jobs {cond} ORDER BY id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![arg, limit as i64], |r| {
            Ok(Job {
                id: r.get(0)?,
                kind: r.get(1)?,
                resource: r.get(2)?,
                user: r.get(3)?,
                state: r.get(4)?,
                created: r.get(5)?,
                finished: r.get(6)?,
                summary: r.get(7)?,
                error: r.get(8)?,
            })
        })?;
        rows.collect()
    }
}

fn parse_role(r: &rusqlite::Row, i: usize) -> R<Role> {
    let s: String = r.get(i)?;
    s.parse().map_err(|e: String| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, e.into())
    })
}

fn row_user(r: &rusqlite::Row) -> R<User> {
    Ok(User {
        id: r.get(0)?,
        name: r.get(1)?,
        role: parse_role(r, 2)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn users_tokens_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let db = AuthDb::open(dir.path()).unwrap();
        assert!(hash_password("short").is_err());
        let id = db
            .user_add(
                "ci",
                &hash_password("correct horse").unwrap(),
                Role::Publisher,
            )
            .unwrap();
        assert!(db.check_password("ci", "wrong horse").unwrap().is_none());
        assert!(
            db.check_password("nobody", "correct horse")
                .unwrap()
                .is_none()
        );
        let u = db.check_password("ci", "correct horse").unwrap().unwrap();
        assert_eq!((u.id, u.role), (id, Role::Publisher));

        let t = db.token_create(id, "laptop").unwrap();
        assert!(t.starts_with("aptpod_"));
        assert_eq!(db.authenticate(&t).unwrap().unwrap().name, "ci");
        assert!(db.authenticate("aptpod_nope").unwrap().is_none());
        assert!(db.tokens(id).unwrap()[0].last_used.is_some());
        assert!(db.token_revoke_plain(&t).unwrap());
        assert!(db.authenticate(&t).unwrap().is_none());

        let t = db.token_create(id, "x").unwrap();
        assert!(db.user_del("ci").unwrap());
        assert!(
            db.authenticate(&t).unwrap().is_none(),
            "tokens go with the user"
        );

        let j = db.job_start("add", "publish:x", "ci").unwrap();
        db.job_start("add", "publish:y", "ci").unwrap();
        db.job_finish(j, Ok("1 change")).unwrap();
        assert_eq!(db.jobs_interrupted().unwrap(), 1);
        let jobs = db.jobs(10).unwrap();
        assert_eq!(jobs[0].state, "interrupted");
        assert_eq!(jobs[1].state, "done");
        assert!(Role::Admin > Role::Publisher && Role::Publisher > Role::Reader);
    }
}
