// SPDX-License-Identifier: GPL-3.0-only

//! Cost of add + publish in the repository engine.
//!
//!     cargo run --release -p aptpod-repo --example bench_add -- DIR [N] [KB]
//!
//! Builds N packages of KB KiB each once (dpkg-deb), then times: a first
//! add of all of them to 4 dists x 2 components, an add of one more, and
//! the same add again (nothing to do).

use std::path::PathBuf;
use std::time::Instant;

use aptpod_repo::ops::{AddFile, Dists};
use aptpod_repo::{Layout, Repo};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let dir = PathBuf::from(&a[0]);
    let n: usize = a.get(1).map_or(300, |s| s.parse().unwrap());
    let kb: usize = a.get(2).map_or(1024, |s| s.parse().unwrap());
    let debs = dir.join(format!("debs-{n}-{kb}"));
    if !debs.join(".done").exists() {
        std::fs::create_dir_all(&debs).unwrap();
        for i in 0..=n {
            let root = debs.join(format!("b{i}"));
            std::fs::create_dir_all(root.join("DEBIAN")).unwrap();
            std::fs::create_dir_all(root.join("usr/share/p")).unwrap();
            std::fs::write(
                root.join("DEBIAN/control"),
                format!("Package: p{i:04}\nVersion: 1.0\nArchitecture: amd64\nMaintainer: T <t@example.invalid>\nDescription: bench\n bench\n"),
            )
            .unwrap();
            let mut data = vec![0u8; kb << 10];
            let mut x = i as u64 + 1;
            for b in data.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            std::fs::write(root.join("usr/share/p/data"), data).unwrap();
            let st = std::process::Command::new("dpkg-deb")
                .args(["--root-owner-group", "-Znone", "--build"])
                .arg(&root)
                .arg(debs.join(format!("p{i:04}.deb")))
                .output()
                .unwrap();
            assert!(st.status.success());
            std::fs::remove_dir_all(&root).unwrap();
        }
        std::fs::write(debs.join(".done"), "").unwrap();
    }
    let work = dir.join("work");
    let _ = std::fs::remove_dir_all(&work);
    let mut repo = Repo::open(&work.join("data")).unwrap();
    let mut layout = Layout::new(
        work.join("tree"),
        &["d1", "d2", "d3", "d4"],
        &["main", "extra"],
        &["amd64"],
    );
    layout.sign = false;
    let plan = repo.plan_create("b", layout, false).unwrap();
    repo.apply(&plan, "bench").unwrap();
    let files: Vec<AddFile> = (0..n)
        .map(|i| debs.join(format!("p{i:04}.deb")).into())
        .collect();
    let time = |label: &str, f: &mut dyn FnMut()| {
        let t = Instant::now();
        f();
        println!("{label:28} {:7.2}s", t.elapsed().as_secs_f64());
    };
    let mut plan = None;
    time(&format!("plan add {n} x {kb} KiB"), &mut || {
        plan = Some(
            repo.plan_add("b", "main", &files, &Dists::All, false)
                .unwrap(),
        );
    });
    time("apply (publish 4 dists)", &mut || {
        repo.apply(plan.as_ref().unwrap(), "bench").unwrap();
    });
    let one: Vec<AddFile> = vec![debs.join(format!("p{n:04}.deb")).into()];
    time("plan + apply one more", &mut || {
        let p = repo
            .plan_add("b", "main", &one, &Dists::All, false)
            .unwrap();
        repo.apply(&p, "bench").unwrap();
    });
    time("same add again (no-op plan)", &mut || {
        let p = repo
            .plan_add("b", "main", &one, &Dists::All, false)
            .unwrap();
        assert!(p.is_empty());
    });
    let du = |p: &std::path::Path| {
        String::from_utf8(
            std::process::Command::new("du")
                .args(["-sk"])
                .arg(p)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
    };
    println!(
        "disk: debs {} KiB, store {} KiB, tree {} KiB (du counts hard links once per tree)",
        du(&debs),
        du(&work.join("data/store")),
        du(&work.join("tree"))
    );
}
