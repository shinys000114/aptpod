// SPDX-License-Identifier: GPL-3.0-only

//! Throughput of the HTTP mirror engine against a fast local server.
//!
//!     cargo run --release -p aptpod-mirror --example bench -- \
//!         --dir /tmp/bench --files 4000 --size 65536 --parallel 8
//!
//! Generates a synthetic archive once (unsigned, one dist), then runs a
//! first sync, a second sync (everything present) and a full re-verify,
//! printing wall time, CPU time and throughput for each.

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
}

fn args() -> Args {
    let mut a = Args {
        dir: std::env::temp_dir().join("aptpod-bench"),
        files: 2000,
        size: 64 << 10,
        parallel: 4,
        fresh: false,
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

fn generate(root: &Path, files: usize, size: usize) {
    let stamp = root.join(".generated");
    let want = format!("{files} {size}");
    if std::fs::read_to_string(&stamp).ok().as_deref() == Some(want.as_str()) {
        return;
    }
    let _ = std::fs::remove_dir_all(root);
    let mut packages = String::new();
    let mut buf = vec![0u8; size];
    for n in 0..files {
        let name = format!("pkg{n:05}");
        let rel = format!("pool/main/p/{name}/{name}_1.0_amd64.deb");
        let p = root.join(&rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        fill(&mut buf, n as u64 + 1);
        std::fs::write(&p, &buf).unwrap();
        packages.push_str(&format!(
            "Package: {name}\nVersion: 1.0\nArchitecture: amd64\nFilename: {rel}\nSize: {size}\nSHA256: {}\n\n",
            sha256_hex(&buf)
        ));
    }
    let dist = root.join("dists/bench");
    std::fs::create_dir_all(dist.join("main/binary-amd64")).unwrap();
    let gz = aptpod_core::index::gzip(packages.as_bytes()).unwrap();
    std::fs::write(dist.join("main/binary-amd64/Packages"), &packages).unwrap();
    std::fs::write(dist.join("main/binary-amd64/Packages.gz"), &gz).unwrap();
    let release = format!(
        "Suite: bench\nCodename: bench\nArchitectures: amd64\nComponents: main\nSHA256:\n {} {} main/binary-amd64/Packages\n {} {} main/binary-amd64/Packages.gz\n",
        sha256_hex(packages.as_bytes()),
        packages.len(),
        sha256_hex(&gz),
        gz.len()
    );
    std::fs::write(dist.join("Release"), release).unwrap();
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

fn serve(root: PathBuf) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(l.local_addr().unwrap()).unwrap();
            let app =
                axum::Router::new().fallback_service(tower_http::services::ServeDir::new(root));
            axum::serve(l, app).await.unwrap();
        });
    });
    format!("http://{}/", rx.recv().unwrap())
}

fn main() {
    let a = args();
    let up = a.dir.join("upstream");
    let t = Instant::now();
    generate(&up, a.files, a.size);
    eprintln!("fixture ready ({:.1}s)", t.elapsed().as_secs_f64());
    let url = serve(up);
    let dest = a.dir.join("mirror");
    let state = a.dir.join("state");
    if a.fresh || true {
        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_dir_all(&state);
    }
    let http = Http::new(Duration::from_secs(30)).unwrap();
    let mut m = HttpMirror::new(&url, &["bench"]);
    m.parallel = a.parallel;
    m.prune = true;
    let total = (a.files * a.size) as f64;
    let cancel = AtomicBool::new(false);
    let log = |_: &str| {};
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
            "{label:12} files={} size={} parallel={}: {dt:6.2}s wall, {dc:6.2}s cpu, {:7.1} MB/s, downloaded {}",
            a.files,
            a.size,
            a.parallel,
            total / dt / 1e6,
            r.downloaded
        )
        .unwrap();
    }
}
