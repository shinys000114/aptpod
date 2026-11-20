// SPDX-License-Identifier: GPL-3.0-only

//! The real gate: apt in Debian and Ubuntu containers must `update` from a
//! tree we generated (signature verified, no warnings) and `install` from it.
//!
//! Slow (pulls images), so it only runs with `APTPOD_E2E=1`; CI sets it.

mod common;

use std::fs;
use std::process::Command;

use aptpod_core::deb::read_deb;
use aptpod_core::index::ReleaseMeta;
use aptpod_core::publish::{ComponentSpec, DistSpec, PublishPackage, publish_dist};
use aptpod_core::sign::Signer;
use common::{TestKey, build_deb, have, missing_tool};

const IMAGES: &[&str] = &[
    "debian:bookworm",
    "debian:trixie",
    "ubuntu:jammy",
    "ubuntu:noble",
    "ubuntu:resolute",
];

const SCRIPT: &str = r#"
set -eu
export DEBIAN_FRONTEND=noninteractive
cp -r /repo /srv/repo
chmod -R a+rX /srv/repo
# only our repository, so the result does not depend on the network
rm -f /etc/apt/sources.list /etc/apt/sources.list.d/*
echo "deb [signed-by=/srv/repo/key.gpg] file:/srv/repo aptpode2e main extra" > /etc/apt/sources.list.d/aptpod.list
apt-get update > /tmp/update.log 2>&1 || { cat /tmp/update.log; exit 1; }
if grep -E '^(W|E|Err):' /tmp/update.log; then cat /tmp/update.log; exit 1; fi
apt-get install -y --no-install-recommends hello-aptpod common-aptpod libfoo1 > /tmp/install.log 2>&1 || { cat /tmp/install.log; exit 1; }
cat /usr/share/hello-aptpod/marker /usr/share/common-aptpod/marker /usr/share/libfoo1/marker
dpkg-query -W -f='${Package} ${Version}\n' hello-aptpod common-aptpod libfoo1
"#;

#[test]
fn apt_installs_from_generated_repo() {
    if std::env::var("APTPOD_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set APTPOD_E2E=1 to run apt in containers");
        return;
    }
    if Command::new("docker")
        .arg("version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        missing_tool("docker");
        return;
    }
    if !have("dpkg-deb") {
        return;
    }
    let Some(key) = TestKey::new() else { return };
    let dir = tempfile::tempdir().unwrap();
    // APTPOD_E2E_KEEP=1 keeps the generated tree for debugging
    let d = if std::env::var("APTPOD_E2E_KEEP").as_deref() == Ok("1") {
        let p = dir.keep();
        eprintln!("keeping {}", p.display());
        p
    } else {
        dir.path().to_path_buf()
    };
    let d = d.as_path();
    let debs = [
        build_deb(d, "hello-aptpod", "1.0", "amd64", "gzip", ""),
        build_deb(d, "hello-aptpod", "1.1", "amd64", "xz", ""),
        build_deb(d, "hello-aptpod", "1.1", "arm64", "zstd", ""),
        build_deb(d, "common-aptpod", "1:0.5", "all", "none", ""),
        build_deb(
            d,
            "libfoo1",
            "2.0-1+b1",
            "amd64",
            "zstd",
            "Source: libfoo (2.0-1)\n",
        ),
    ];
    let packages = debs
        .iter()
        .map(|f| PublishPackage {
            info: read_deb(f).unwrap(),
            file: f.clone(),
        })
        .collect();
    let root = d.join("repo");
    let spec = DistSpec {
        name: "aptpode2e".into(),
        meta: ReleaseMeta::new("aptpode2e", "aptpod-test", "aptpod e2e"),
        architectures: vec!["amd64".into(), "arm64".into()],
        components: vec![
            ComponentSpec {
                name: "main".into(),
                packages,
            },
            ComponentSpec {
                name: "extra".into(),
                packages: vec![],
            },
        ],
    };
    let signer = key.signer();
    publish_dist(&root, &spec, Some(&signer as &dyn Signer)).unwrap();
    fs::write(root.join("key.gpg"), &key.public).unwrap();

    let mut failures = Vec::new();
    for image in IMAGES {
        let out = Command::new("docker")
            .args(["run", "--rm", "--network", "none", "-v"])
            .arg(format!("{}:/repo:ro", root.display()))
            .args([image, "sh", "-c", SCRIPT])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !out.status.success()
            || !stdout.contains("hello-aptpod 1.1")
            || !stdout.contains("common-aptpod 1:0.5")
        {
            failures.push(format!(
                "{image}:\n{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            ));
        } else {
            eprintln!("{image}: ok");
        }
    }
    assert!(
        failures.is_empty(),
        "apt failed:\n{}",
        failures.join("\n---\n")
    );
}
