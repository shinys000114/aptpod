// SPDX-License-Identifier: GPL-3.0-only

//! HTTP fetching: small files into memory, pool files to disk with
//! resume, size and sha256 checks, retries and cancellation.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, RANGE};
use sha2::{Digest, Sha256};

use crate::Error;

/// Suffix of partial downloads next to their target. Not `.aptpod-tmp`:
/// core's leftover cleaning deletes those, and partials must survive for
/// resuming.
pub const PART: &str = ".aptpod-part";

#[derive(Debug)]
pub enum FetchError {
    /// 404 or 410.
    Missing(u16),
    /// Network trouble, 5xx, 429: worth retrying.
    Transient(String),
    /// Wrong content or a refusal: retrying will not help.
    Bad(String),
    /// Writing locally failed (disk full, permissions): stop the run.
    Local(String),
    Interrupted,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Missing(s) => write!(f, "HTTP {s}"),
            FetchError::Transient(m) | FetchError::Bad(m) | FetchError::Local(m) => f.write_str(m),
            FetchError::Interrupted => f.write_str("interrupted"),
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

/// HTTP client for the (synchronous) engine, usable from many threads.
/// It owns a small tokio runtime and runs each request, body included, in
/// one `block_on` (reqwest's blocking client cannot combine a per-read
/// timeout with reading bodies). Create, use and drop it outside any tokio
/// runtime.
pub struct Http {
    client: reqwest::Client,
    backoff: Duration,
    rt: tokio::runtime::Runtime,
}

/// What a successful download did.
#[derive(Debug, Clone, Copy, Default)]
pub struct Downloaded {
    pub bytes: u64,
    pub resumed: bool,
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// sha256 of a file, checking `cancel` as it reads.
pub fn sha256_file(path: &Path, cancel: &AtomicBool) -> Result<String, Error> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Interrupted);
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

impl Http {
    /// `read_timeout` bounds every single read, so a stalled connection
    /// fails while a large, slow file still finishes.
    pub fn new(read_timeout: Duration) -> Result<Self, Error> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("aptpod-http")
            .enable_all()
            .build()?;
        let client = reqwest::Client::builder()
            .user_agent(format!("aptpod/{}", aptpod_core::version()))
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(read_timeout)
            .build()
            .map_err(|e| {
                let mut m = chain(&e);
                if m.contains("CA certificates") {
                    m.push_str(" (install the ca-certificates package)");
                }
                Error::Http(m)
            })?;
        Ok(Self {
            client,
            backoff: Duration::from_millis(500),
            rt,
        })
    }

    /// First retry delay (doubling up to 32x); short in tests.
    pub fn with_backoff(mut self, first: Duration) -> Self {
        self.backoff = first;
        self
    }

    fn classify(status: StatusCode) -> Option<FetchError> {
        match status.as_u16() {
            200..=299 => None,
            404 | 410 => Some(FetchError::Missing(status.as_u16())),
            408 | 429 | 500..=599 => Some(FetchError::Transient(format!("HTTP {status}"))),
            _ => Some(FetchError::Bad(format!("HTTP {status}"))),
        }
    }

    /// A small file (Release, an index) into memory.
    pub fn get(&self, url: &str, cancel: &AtomicBool) -> Result<Vec<u8>, FetchError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchError::Interrupted);
        }
        self.rt.block_on(async {
            let mut resp = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|e| FetchError::Transient(chain(&e)))?;
            if let Some(e) = Self::classify(resp.status()) {
                return Err(e);
            }
            let mut out = Vec::new();
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(FetchError::Interrupted);
                }
                match resp.chunk().await {
                    Ok(Some(c)) => {
                        out.extend_from_slice(&c);
                        if out.len() as u64 > MAX_GET {
                            return Err(FetchError::Bad(format!("larger than {MAX_GET} bytes")));
                        }
                    }
                    Ok(None) => return Ok(out),
                    Err(e) => return Err(FetchError::Transient(format!("reading: {}", chain(&e)))),
                }
            }
        })
    }

    /// [`Http::get`] with retries for transient failures.
    pub fn get_retry(
        &self,
        url: &str,
        retries: u32,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>, FetchError> {
        let mut attempt = 0;
        loop {
            match self.get(url, cancel) {
                Err(FetchError::Transient(m)) if attempt < retries => {
                    attempt += 1;
                    backoff(self.backoff, attempt, cancel)?;
                    let _ = m;
                }
                r => return r,
            }
        }
    }

    /// Downloads `url` to `dest` (via `dest` + [`PART`], resuming a partial
    /// file), checking size and sha256; retries transient failures.
    pub fn download(
        &self,
        url: &str,
        dest: &Path,
        size: u64,
        sha256: &str,
        retries: u32,
        cancel: &AtomicBool,
    ) -> Result<Downloaded, FetchError> {
        let part = part_path(dest);
        if let Some(dir) = dest.parent() {
            crate::retry_fs(|| fs::create_dir_all(dir))
                .map_err(|e| FetchError::Local(format!("{}: {e}", dir.display())))?;
        }
        let mut attempt = 0;
        let mut fresh_retry_left = true;
        loop {
            match self.try_download(url, &part, size, sha256, cancel) {
                Ok(d) => {
                    fs::set_permissions(&part, std::os::unix::fs::PermissionsExt::from_mode(0o644))
                        .and_then(|()| fs::rename(&part, dest))
                        .map_err(|e| FetchError::Local(format!("{}: {e}", dest.display())))?;
                    return Ok(d);
                }
                Err((FetchError::Bad(m), resumed)) => {
                    // A resumed file that does not match may have mixed two
                    // upstream versions: once more from zero.
                    let _ = fs::remove_file(&part);
                    if resumed && fresh_retry_left {
                        fresh_retry_left = false;
                        continue;
                    }
                    return Err(FetchError::Bad(m));
                }
                Err((FetchError::Transient(m), _)) => {
                    if attempt >= retries {
                        return Err(FetchError::Transient(m));
                    }
                    attempt += 1;
                    backoff(self.backoff, attempt, cancel)?;
                }
                Err((e, _)) => return Err(e),
            }
        }
    }

    fn try_download(
        &self,
        url: &str,
        part: &Path,
        size: u64,
        sha256: &str,
        cancel: &AtomicBool,
    ) -> Result<Downloaded, (FetchError, bool)> {
        self.rt
            .block_on(self.try_download_async(url, part, size, sha256, cancel))
    }

    async fn try_download_async(
        &self,
        url: &str,
        part: &Path,
        size: u64,
        sha256: &str,
        cancel: &AtomicBool,
    ) -> Result<Downloaded, (FetchError, bool)> {
        let io = |e: std::io::Error| (FetchError::Local(format!("{}: {e}", part.display())), false);
        if cancel.load(Ordering::Relaxed) {
            return Err((FetchError::Interrupted, false));
        }
        let mut have = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        if have >= size {
            have = 0;
        }
        let mut req = self.client.get(url);
        if have > 0 {
            req = req.header(RANGE, format!("bytes={have}-"));
        }
        let mut resp = req
            .send()
            .await
            .map_err(|e| (FetchError::Transient(chain(&e)), false))?;
        let status = resp.status();
        let resumed = if have > 0 && status == StatusCode::PARTIAL_CONTENT {
            // The range must start where the partial file ends.
            let ok = resp
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with(&format!("bytes {have}-")));
            if !ok {
                fs::remove_file(part).map_err(io)?;
                return Err((
                    FetchError::Transient("unexpected Content-Range".into()),
                    false,
                ));
            }
            true
        } else if status == StatusCode::RANGE_NOT_SATISFIABLE {
            fs::remove_file(part).map_err(io)?;
            return Err((
                FetchError::Transient("range not satisfiable; starting over".into()),
                false,
            ));
        } else {
            if let Some(e) = Self::classify(status) {
                return Err((e, false));
            }
            // 200 to a ranged request: the server ignores ranges.
            have = 0;
            false
        };

        let mut hasher = Sha256::new();
        let mut file = if resumed {
            let mut f = fs::File::open(part).map_err(io)?;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = f.read(&mut buf).map_err(io)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            fs::OpenOptions::new().append(true).open(part).map_err(io)?
        } else {
            let f = crate::retry_fs(|| fs::File::create(part)).map_err(io)?;
            // Reserve the space up front (fewer fragments when several
            // downloads write at once, e.g. on a disk); KEEP_SIZE leaves
            // the length alone, so resuming still reads how far we got.
            if size >= 1 << 20 {
                let _ = rustix::fs::fallocate(&f, rustix::fs::FallocateFlags::KEEP_SIZE, 0, size);
            }
            f
        };
        let mut total = have;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err((FetchError::Interrupted, resumed));
            }
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    return Err((
                        FetchError::Transient(format!("reading: {}", chain(&e))),
                        resumed,
                    ));
                }
            };
            let n = chunk.len();
            let buf = &chunk[..];
            total += n as u64;
            if total > size {
                return Err((
                    FetchError::Bad(format!("larger than the {size} bytes the index lists")),
                    resumed,
                ));
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n]).map_err(io)?;
        }
        file.flush().map_err(io)?;
        if total < size {
            return Err((
                FetchError::Transient(format!("got {total} of {size} bytes")),
                resumed,
            ));
        }
        let got = hex(&hasher.finalize());
        if got != sha256 {
            return Err((
                FetchError::Bad(format!("sha256 {got} does not match the index ({sha256})")),
                resumed,
            ));
        }
        Ok(Downloaded {
            bytes: total - have,
            resumed,
        })
    }
}

/// Largest Release or index file read into memory.
pub const MAX_GET: u64 = 1 << 30;

/// `base` + `path` with every path segment percent-encoded (file names
/// may contain `+`, `%`, `#`, `?`, `:` and more).
pub fn join_url(base: &str, path: &str) -> String {
    let mut out = String::from(base);
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        for b in seg.bytes() {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(PART);
    PathBuf::from(s)
}

fn backoff(first: Duration, attempt: u32, cancel: &AtomicBool) -> Result<(), FetchError> {
    let wait = first * (1 << (attempt - 1).min(5));
    let step = Duration::from_millis(50).min(wait);
    let mut slept = Duration::ZERO;
    while slept < wait {
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchError::Interrupted);
        }
        std::thread::sleep(step);
        slept += step;
    }
    Ok(())
}
