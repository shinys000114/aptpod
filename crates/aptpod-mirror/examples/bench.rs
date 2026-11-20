// SPDX-License-Identifier: GPL-3.0-only

//! Throughput of the HTTP mirror engine against a fast local server.
//!
//!     cargo run --release -p aptpod-mirror --example bench -- \
//!         --dir /tmp/bench --files 4000 --size 65536 --parallel 8
//!
//! Generates a synthetic archive once (unsigned, one dist), then runs a
//! first sync, a second sync (everything present) and a full re-verify,
//! printing wall time, CPU time and throughput for each. The server runs
//! in a separate process (its CPU is not counted) and can model a real
//! link: `--latency-ms` before every response, `--link-mbps` shared by
//! all connections.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use aptpod_mirror::fetch::{Http, sha256_hex};
use aptpod_mirror::http::{Ctx, HttpMirror, sync};

struct Args {
    dir: PathBuf,
    files: usize,
    size: usize,
    parallel: usize,
    fresh: bool,
    latency_ms: u64,
    link_mbps: f64,
    serve: Option<PathBuf>,
    dists: usize,
    comps: usize,
}

fn args() -> Args {
    let mut a = Args {
        dir: std::env::temp_dir().join("aptpod-bench"),
        files: 2000,
        size: 64 << 10,
        parallel: 4,
        fresh: false,
        latency_ms: 0,
        link_mbps: 0.0,
        serve: None,
        dists: 1,
        comps: 1,
    };
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < v.len() {
        let next = || v.get(i + 1).cloned().unwrap_or_default();
        match v[i].as_str() {
            "--dir" => a.dir = next().into(),
            "--files" => a.files = next().parse().unwrap(),
            "--size" => a.size = next().parse().unwrap(),
            "--parallel" => a.parallel = next().parse().unwrap(),
            "--latency-ms" => a.latency_ms = next().parse().unwrap(),
            "--link-mbps" => a.link_mbps = next().parse().unwrap(),
            "--serve" => a.serve = Some(next().into()),
            "--dists" => a.dists = next().parse().unwrap(),
            "--comps" => a.comps = next().parse().unwrap(),
            "--fresh" => {
                a.fresh = true;
                i -= 1;
            }
            x => panic!("unknown argument {x}"),
        }
        i += 2;
    }
    a
}

/// xorshift: fast, deterministic file contents.
fn fill(buf: &mut [u8], mut seed: u64) {
    for chunk in buf.chunks_mut(8) {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let b = seed.to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
}

/// `files` pool files spread over `comps` components; every one of
/// `dists` dists lists all components (Packages, Packages.gz, Release).
fn generate(root: &Path, files: usize, size: usize, dists: usize, comps: usize) {
    let stamp = root.join(".generated");
    let want = format!("{files} {size} {dists} {comps}");
    if std::fs::read_to_string(&stamp).ok().as_deref() == Some(want.as_str()) {
        return;
    }
    let _ = std::fs::remove_dir_all(root);
    let mut packages = vec![String::new(); comps];
    let mut buf = vec![0u8; size];
    for n in 0..files {
        let c = n % comps;
        let name = format!("pkg{n:05}");
        let rel = format!("pool/c{c}/p/{name}/{name}_1.0_amd64.deb");
        let p = root.join(&rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        fill(&mut buf, n as u64 + 1);
        std::fs::write(&p, &buf).unwrap();
        packages[c].push_str(&format!(
            "Package: {name}\nVersion: 1.0\nArchitecture: amd64\nFilename: {rel}\nSize: {size}\nSHA256: {}\n\n",
            sha256_hex(&buf)
        ));
    }
    for d in 0..dists {
        let dist = root.join(format!("dists/d{d}"));
        let mut sums = String::new();
        let comp_names: Vec<String> = (0..comps).map(|c| format!("c{c}")).collect();
        for (c, text) in packages.iter().enumerate() {
            let dir = dist.join(format!("c{c}/binary-amd64"));
            std::fs::create_dir_all(&dir).unwrap();
            let gz = aptpod_core::index::gzip(text.as_bytes()).unwrap();
            let rel = format!("Archive: d{d}\nComponent: c{c}\nArchitecture: amd64\n");
            for (name, data) in [
                ("Packages", text.as_bytes()),
                ("Packages.gz", &gz[..]),
                ("Release", rel.as_bytes()),
            ] {
                std::fs::write(dir.join(name), data).unwrap();
                sums.push_str(&format!(
                    " {} {} c{c}/binary-amd64/{name}\n",
                    sha256_hex(data),
                    data.len()
                ));
            }
        }
        std::fs::write(
            dist.join("Release"),
            format!(
                "Suite: d{d}\nCodename: d{d}\nArchitectures: amd64\nComponents: {}\nSHA256:\n{sums}",
                comp_names.join(" ")
            ),
        )
        .unwrap();
    }
    std::fs::write(&stamp, want).unwrap();
}

/// (user + system) CPU seconds of this process.
fn cpu() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let f: Vec<&str> = stat
        .rsplit(')')
        .next()
        .unwrap()
        .split_whitespace()
        .collect();
    let ticks: f64 = f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap();
    ticks / 100.0
}

/// Serves `root` (with Range) after `latency`, all connections sharing
/// one `link` rate; prints the address on stdout.
fn serve_forever(root: PathBuf, latency: Duration, link_bps: f64) {
    use axum::body::Body;
    use axum::http::{HeaderMap, StatusCode, Uri, header};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    // Token bucket as "the link is busy until": each chunk books its time.
    let busy = Arc::new(Mutex::new(tokio::time::Instant::now()));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        println!("http://{}/", l.local_addr().unwrap());
        let app = axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
            let (root, busy) = (root.clone(), busy.clone());
            async move {
                tokio::time::sleep(latency).await;
                let rel = uri.path().trim_start_matches('/').replace("%2B", "+");
                let path = root.join(&rel);
                let Ok(mut f) = tokio::fs::File::open(&path).await else {
                    return (StatusCode::NOT_FOUND, Body::empty()).into_response();
                };
                let len = f.metadata().await.unwrap().len();
                let start = headers
                    .get(header::RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| {
                        v.strip_prefix("bytes=")?
                            .strip_suffix('-')?
                            .parse::<u64>()
                            .ok()
                    })
                    .filter(|s| *s < len);
                if let Some(s) = start {
                    f.seek(std::io::SeekFrom::Start(s)).await.unwrap();
                }
                let stream =
                    futures_util::stream::unfold((f, busy), move |(mut f, busy)| async move {
                        let mut buf = vec![0u8; 64 << 10];
                        let n = f.read(&mut buf).await.ok()?;
                        if n == 0 {
                            return None;
                        }
                        buf.truncate(n);
                        if link_bps > 0.0 {
                            let slot = {
                                let mut b = busy.lock().unwrap();
                                let now = tokio::time::Instant::now();
                                // A little burst credit, so a stream that is
                                // a moment late can catch up (a real link
                                // queues).
                                let start = (*b).max(now - Duration::from_millis(20));
                                *b = start + Duration::from_secs_f64(n as f64 / link_bps);
                                *b
                            };
                            tokio::time::sleep_until(slot).await;
                        }
                        Some((
                            Ok::<_, std::io::Error>(axum::body::Bytes::from(buf)),
                            (f, busy),
                        ))
                    });
                let mut r = Body::from_stream(stream).into_response();
                let n = len - start.unwrap_or(0);
                r.headers_mut().insert(header::CONTENT_LENGTH, n.into());
                if let Some(s) = start {
                    *r.status_mut() = StatusCode::PARTIAL_CONTENT;
                    r.headers_mut().insert(
                        header::CONTENT_RANGE,
                        format!("bytes {s}-{}/{len}", len - 1).parse().unwrap(),
                    );
                }
                r
            }
        });
        // Like a real web server: no Nagle delay on small responses.
        let l = l.tap_io(|tcp| {
            let _ = tcp.set_nodelay(true);
        });
        axum::serve(l, app).await.unwrap();
    });
}

use axum::response::IntoResponse;
use axum::serve::ListenerExt;

/// Starts this program as the server process.
fn serve(root: PathBuf, a: &Args) -> (std::process::Child, String) {
    use std::io::BufRead;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--serve")
        .arg(&root)
        .args([
            "--latency-ms",
            &a.latency_ms.to_string(),
            "--link-mbps",
            &a.link_mbps.to_string(),
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    (child, line.trim().to_string())
}

fn main() {
    let a = args();
    if let Some(root) = &a.serve {
        serve_forever(
            root.clone(),
            Duration::from_millis(a.latency_ms),
            a.link_mbps * 1e6 / 8.0,
        );
        return;
    }
    let up = a.dir.join("upstream");
    let t = Instant::now();
    generate(&up, a.files, a.size, a.dists, a.comps);
    eprintln!("fixture ready ({:.1}s)", t.elapsed().as_secs_f64());
    let (mut server, url) = serve(up, &a);
    let dest = a.dir.join("mirror");
    let state = a.dir.join("state");
    let _ = a.fresh;
    let _ = std::fs::remove_dir_all(&dest);
    let _ = std::fs::remove_dir_all(&state);
    let http = Http::new(Duration::from_secs(30)).unwrap();
    let dists: Vec<String> = (0..a.dists).map(|d| format!("d{d}")).collect();
    let dist_refs: Vec<&str> = dists.iter().map(String::as_str).collect();
    let mut m = HttpMirror::new(&url, &dist_refs);
    m.parallel = a.parallel;
    m.prune = true;
    let total = (a.files * a.size) as f64;
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let verbose = std::env::var_os("BENCH_LOG").is_some();
    let log = |l: &str| {
        if verbose {
            eprintln!("{:8.2}s {l}", started.elapsed().as_secs_f64());
        }
    };
    let ctx = Ctx {
        http: &http,
        dest: &dest,
        state_dir: &state,
        cancel: &cancel,
        log: &log,
    };
    let mut out = std::io::stdout();
    for (label, verify_all) in [
        ("first sync", false),
        ("second sync", false),
        ("verify all", true),
    ] {
        m.verify_all = verify_all;
        let (c0, t0) = (cpu(), Instant::now());
        let r = sync(&m, &ctx).unwrap();
        let (dt, dc) = (t0.elapsed().as_secs_f64(), cpu() - c0);
        assert!(r.ok(), "{r:?}");
        writeln!(
            out,
            "{label:12} files={} size={} dists={}x{} parallel={} latency={}ms link={}Mbps: {dt:6.2}s wall, {dc:6.2}s cpu, {:7.1} MB/s, downloaded {}",
            a.files,
            a.size,
            a.dists,
            a.comps,
            a.parallel,
            a.latency_ms,
            a.link_mbps,
            total / dt / 1e6,
            r.downloaded
        )
        .unwrap();
    }
    let _ = server.kill();
}
