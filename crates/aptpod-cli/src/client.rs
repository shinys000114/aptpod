// SPDX-License-Identifier: GPL-3.0-only

//! HTTP client for the server API, saved credentials and exit codes.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Method;
use reqwest::blocking::{RequestBuilder, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// An error with the server's error kind, which picks the exit code.
#[derive(Debug)]
pub struct CliError {
    pub kind: String,
    pub message: String,
}

impl CliError {
    pub fn other(message: impl Into<String>) -> Self {
        Self {
            kind: "other".into(),
            message: message.into(),
        }
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            kind: "usage".into(),
            message: message.into(),
        }
    }

    /// 2 usage, 3 stale (preview again), 4 conflict, 5 blocked (needs
    /// force), 6 unsynced (committed but a tree is behind), 7 auth.
    pub fn exit_code(&self) -> u8 {
        match self.kind.as_str() {
            "usage" => 2,
            "stale" => 3,
            "conflict" => 4,
            "blocked" => 5,
            "unsynced" => 6,
            "unauthorized" | "forbidden" => 7,
            _ => 1,
        }
    }
}

fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        let t = c.to_string();
        if !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        cur = c.source();
    }
    s
}

pub struct Client {
    base: String,
    token: Option<String>,
    http: reqwest::blocking::Client,
}

impl Client {
    pub fn new(server: &str, token: Option<String>) -> Result<Self, CliError> {
        let base = server.trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(CliError::usage(format!(
                "server {server:?} must be an http:// or https:// URL"
            )));
        }
        let http = reqwest::blocking::Client::builder()
            .user_agent(format!("aptpod/{}", aptpod_core::version()))
            .connect_timeout(Duration::from_secs(30))
            // Uploads and downloads may take long; no total timeout.
            .timeout(None)
            .build()
            .map_err(|e| {
                let mut m = chain(&e);
                if m.contains("CA certificates") {
                    m.push_str(" (install the ca-certificates package)");
                }
                CliError::other(m)
            })?;
        Ok(Self { base, token, http })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn req(&self, method: Method, path: &str) -> RequestBuilder {
        let r = self.http.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    fn send<T: DeserializeOwned>(&self, rb: RequestBuilder) -> Result<T, CliError> {
        let resp = rb
            .send()
            .map_err(|e| CliError::other(format!("{}: {}", self.base, chain(&e))))?;
        let status = resp.status();
        let text = resp
            .text()
            .map_err(|e| CliError::other(format!("{}: {}", self.base, chain(&e))))?;
        if status.is_success() {
            return serde_json::from_str(&text)
                .map_err(|e| CliError::other(format!("{}: unexpected response: {e}", self.base)));
        }
        let v: Value = serde_json::from_str(&text)
            .unwrap_or_else(|_| json!({ "error": format!("{status}: {}", text.trim()) }));
        Err(CliError {
            kind: v["kind"].as_str().unwrap_or("other").to_string(),
            message: v["error"].as_str().unwrap_or("request failed").to_string(),
        })
    }

    pub fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, CliError> {
        self.send(self.req(Method::GET, path).query(query))
    }

    pub fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, CliError> {
        self.send(self.req(Method::POST, path).json(body))
    }

    pub fn delete(&self, path: &str) -> Result<(), CliError> {
        let _: Value = self.delete_json(path)?;
        Ok(())
    }

    pub fn delete_json(&self, path: &str) -> Result<Value, CliError> {
        self.send(self.req(Method::DELETE, path))
    }

    /// Job events from `/api/v1/events` (server-sent events), until the
    /// server ends the stream.
    pub fn events(&self) -> Result<impl Iterator<Item = Result<Value, CliError>>, CliError> {
        use std::io::BufRead;
        let resp = self
            .req(Method::GET, "/api/v1/events")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .map_err(|e| CliError::other(format!("{}: {}", self.base, chain(&e))))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let v: Value = resp
                .json()
                .unwrap_or(json!({ "error": status.to_string() }));
            return Err(CliError {
                kind: v["kind"].as_str().unwrap_or("other").to_string(),
                message: v["error"].as_str().unwrap_or("request failed").to_string(),
            });
        }
        let lines = std::io::BufReader::new(resp).lines();
        Ok(lines.filter_map(|l| match l {
            Err(e) => Some(Err(CliError::other(format!("event stream: {e}")))),
            Ok(l) => l.strip_prefix("data:").map(|d| {
                serde_json::from_str(d.trim()).map_err(|e| CliError::other(e.to_string()))
            }),
        }))
    }

    /// Streams a file to `/api/v1/uploads`.
    pub fn upload(&self, file: &Path) -> Result<Value, CliError> {
        let f = fs::File::open(file)
            .map_err(|e| CliError::other(format!("{}: {e}", file.display())))?;
        let len = f
            .metadata()
            .map_err(|e| CliError::other(format!("{}: {e}", file.display())))?
            .len();
        self.send(
            self.req(Method::POST, "/api/v1/uploads")
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/vnd.debian.binary-package",
                )
                .body(reqwest::blocking::Body::sized(f, len)),
        )
        .map_err(|e| CliError {
            message: format!("uploading {}: {}", file.display(), e.message),
            ..e
        })
    }

    /// Downloads `url` into a new directory under `dir`.
    pub fn download(&self, url: &str, dir: &Path) -> Result<PathBuf, CliError> {
        let fail = |m: String| CliError::other(format!("{url}: {m}"));
        let name = url
            .split(['?', '#'])
            .next()
            .and_then(|u| u.rsplit('/').next())
            .filter(|n| !n.is_empty() && *n != "." && *n != "..")
            .unwrap_or("download.deb");
        let sub = dir.join(fs::read_dir(dir).map_or(0, |d| d.count()).to_string());
        fs::create_dir(&sub).map_err(|e| fail(e.to_string()))?;
        let path = sub.join(name);
        let mut resp: Response = self.http.get(url).send().map_err(|e| fail(chain(&e)))?;
        if !resp.status().is_success() {
            return Err(fail(resp.status().to_string()));
        }
        let mut out = fs::File::create(&path).map_err(|e| fail(e.to_string()))?;
        resp.copy_to(&mut out).map_err(|e| fail(chain(&e)))?;
        Ok(path)
    }
}

/// `~/.config/aptpod/credentials.toml` (or `$APTPOD_CREDENTIALS`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub server: String,
    pub token: String,
}

impl Credentials {
    fn path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("APTPOD_CREDENTIALS") {
            return Some(p.into());
        }
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(config.join("aptpod").join("credentials.toml"))
    }

    pub fn load() -> Result<Option<Self>, CliError> {
        let Some(path) = Self::path() else {
            return Ok(None);
        };
        match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map(Some)
                .map_err(|e| CliError::other(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CliError::other(format!("{}: {e}", path.display()))),
        }
    }

    /// Writes the file readable by the owner only.
    pub fn save(&self) -> Result<PathBuf, CliError> {
        let path = Self::path().ok_or_else(|| CliError::other("no home directory"))?;
        let fail = |e: std::io::Error| CliError::other(format!("{}: {e}", path.display()));
        let dir = path.parent().expect("has parent");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(fail)?;
        let tmp = dir.join(format!(".credentials.{}.tmp", std::process::id()));
        let text = toml::to_string(self).map_err(|e| CliError::other(e.to_string()))?;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(fail)?;
        f.write_all(text.as_bytes()).map_err(fail)?;
        drop(f);
        fs::rename(&tmp, &path).map_err(fail)?;
        Ok(path)
    }

    pub fn remove() -> Result<Option<PathBuf>, CliError> {
        let Some(path) = Self::path() else {
            return Ok(None);
        };
        match fs::remove_file(&path) {
            Ok(()) => Ok(Some(path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CliError::other(format!("{}: {e}", path.display()))),
        }
    }
}
