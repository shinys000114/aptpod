// SPDX-License-Identifier: GPL-3.0-only

//! apt in containers against a mirrored subset: the upstream signature
//! must still verify (`signed-by`, never `trusted=yes`), `update` must be
//! clean although Release lists components and architectures that were
//! not mirrored, and `install` must work. A re-signed mirror must verify
//! with our key and not with the upstream's. Only with `APTPOD_E2E=1`.

mod common;

use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use aptpod_core::testutil::missing_tool;
use aptpod_core::verify::Keyring;
use aptpod_mirror::fetch::Http;
use aptpod_mirror::http::{Ctx, HttpMirror, sync};
use common::{Resign, Server, Upstream};

const IMAGES: &[&str] = &[
    "debian:bookworm",
    "debian:trixie",
    "ubuntu:jammy",
    "ubuntu:noble",
];

const SCRIPT: &str = r#"
set -eu
export DEBIAN_FRONTEND=noninteractive
rm -f /etc/apt/sources.list /etc/apt/sources.list.d/*
cat > /etc/apt/sources.list.d/mirror.list <<LIST
deb [arch=amd64 signed-by=/key/key.gpg] file:/mirror noble main
deb [arch=amd64 signed-by=/key/key.gpg] file:/mirror focal main
LIST
apt-get update > /tmp/update.log 2>&1 || { cat /tmp/update.log; exit 1; }
if grep -E '^(W|E|Err):' /tmp/update.log; then cat /tmp/update.log; exit 1; fi
apt-get install -y --no-install-recommends hello-mirror tool-mirror > /tmp/install.log 2>&1 || { cat /tmp/install.log; exit 1; }
dpkg-query -W -f='${Package} ${Version}\n' hello-mirror tool-mirror
"#;

fn docker_ready() -> bool {
    if std::env::var("APTPOD_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set APTPOD_E2E=1 to run apt in containers");
        return false;
    }
    if Command::new("docker")
        .arg("version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        missing_tool("docker");
        return false;
    }
    true
}

/// The upstream of both tests: noble and focal (no InRelease, like the
/// reference PPA), main + extra, amd64 + arm64.
fn upstream() -> Option<Upstream> {
    let up = Upstream::new()?;
    let hello = up.deb("hello-mirror", "1.0", "amd64", "a");
    let hello_arm = up.deb("hello-mirror", "1.0", "arm64", "a");
    let tool = up.deb("tool-mirror", "2.0", "all", "a");
    let other = up.deb("other-mirror", "1.0", "amd64", "a");
    for dist in ["noble", "focal"] {
        up.publish(
            dist,
            &["amd64", "arm64"],
            &[
                ("main", vec![hello.clone(), hello_arm.clone(), tool.clone()]),
                ("extra", vec![other.clone()]),
            ],
        );
    }
    // focal like the reference PPA: no InRelease.
    up.resign(
        "focal",
        &["amd64", "arm64"],
        &["main", "extra"],
        &Resign {
            no_inrelease: true,
            ..Default::default()
        },
    );
    Some(up)
}

/// Runs `script` in every image with `tree` at /mirror and `key` as
/// /key/key.gpg; returns the failures (`check` judges stdout).
fn run_apt(
    tree: &std::path::Path,
    key: &[u8],
    script: &str,
    check: &dyn Fn(bool, &str) -> bool,
) -> Vec<String> {
    let keydir = tempfile::tempdir().unwrap();
    std::fs::write(keydir.path().join("key.gpg"), key).unwrap();
    // apt's sandbox user must be able to read the tree and the key.
    for p in [tree, keydir.path()] {
        Command::new("chmod")
            .arg("-R")
            .arg("a+rX")
            .arg(p)
            .status()
            .unwrap();
    }
    let mut failures = Vec::new();
    for image in IMAGES {
        let o = Command::new("docker")
            .args(["run", "--rm", "--network", "none", "-v"])
            .arg(format!("{}:/mirror:ro", tree.display()))
            .arg("-v")
            .arg(format!("{}:/key:ro", keydir.path().display()))
            .args([image, "sh", "-c", script])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&o.stdout);
        if !check(o.status.success(), &stdout) {
            failures.push(format!(
                "{image}:\n{stdout}\n{}",
                String::from_utf8_lossy(&o.stderr)
            ));
        } else {
            eprintln!("{image}: ok");
        }
    }
    failures
}

fn installed(ok: bool, stdout: &str) -> bool {
    ok && stdout.contains("hello-mirror 1.0") && stdout.contains("tool-mirror 2.0")
}

#[test]
fn apt_uses_the_mirror_with_the_upstream_key() {
    if !docker_ready() {
        return;
    }
    let Some(up) = upstream() else { return };
    let srv = Server::start(&up.root);
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("mirror");
    let mut m = HttpMirror::new(&srv.url, &["noble", "focal"]);
    m.keyring = Some(Keyring::from_bytes(&up.key.public).unwrap());
    m.components = vec!["main".into()];
    m.architectures = vec!["amd64".into()];
    let http = Http::new(Duration::from_secs(10)).unwrap();
    let log = |l: &str| eprintln!("mirror: {l}");
    let r = sync(
        &m,
        &Ctx {
            http: &http,
            dest: &dest,
            state_dir: &out.path().join("state"),
            cancel: &AtomicBool::new(false),
            log: &log,
        },
    )
    .unwrap();
    assert!(r.ok(), "{r:#?}");
    let failures = run_apt(&dest, &up.key.public, SCRIPT, &installed);
    assert!(
        failures.is_empty(),
        "apt failed:\n{}",
        failures.join("\n---\n")
    );
}

/// Our key verifies the re-signed tree; the upstream key must not.
const UPDATE_ONLY: &str = r#"
set -u
export DEBIAN_FRONTEND=noninteractive
rm -f /etc/apt/sources.list /etc/apt/sources.list.d/*
echo "deb [arch=amd64 signed-by=/key/key.gpg] file:/mirror noble main" > /etc/apt/sources.list.d/mirror.list
apt-get update > /tmp/update.log 2>&1; echo "exit=$?"
cat /tmp/update.log
"#;

#[test]
fn apt_uses_the_resigned_mirror_with_our_key() {
    use aptpod_mirror::resign::{ResignCtx, ResignMirror, Target};
    use aptpod_repo::mirror::{MirrorReport, MirrorSync};
    use std::collections::HashSet;
    use std::sync::Mutex;

    if !docker_ready() {
        return;
    }
    let Some(up) = upstream() else { return };
    let Some(ours) = aptpod_core::testutil::TestKey::new() else {
        return;
    };
    struct T(Mutex<aptpod_repo::Repo>);
    impl Target for T {
        fn known(&self, s: &[String]) -> Result<HashSet<String>, aptpod_mirror::Error> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .known_packages(s)?
                .into_iter()
                .collect())
        }
        fn apply(&self, s: MirrorSync) -> Result<MirrorReport, aptpod_mirror::Error> {
            Ok(self.0.lock().unwrap().sync_mirror("rs", &s, "test")?)
        }
    }
    let srv = Server::start(&up.root);
    let out = tempfile::tempdir().unwrap();
    let data = out.path().join("data");
    let tree = out.path().join("tree");
    let mut repo = aptpod_repo::Repo::open(&data).unwrap();
    repo.set_signer(Some(Box::new(ours.signer())));
    let target = T(Mutex::new(repo));
    let mut m = ResignMirror::new(&srv.url, &["noble", "focal"], &tree);
    m.source.keyring = Some(Keyring::from_bytes(&up.key.public).unwrap());
    m.source.components = vec!["main".into()];
    m.source.architectures = vec!["amd64".into()];
    let http = Http::new(Duration::from_secs(10)).unwrap();
    let log = |l: &str| eprintln!("resign: {l}");
    let store = aptpod_repo::store::Store::new(data.join("store"));
    let r = aptpod_mirror::resign::sync(
        &m,
        &ResignCtx {
            http: &http,
            state_dir: &data.join("mirrors/rs"),
            store: &store,
            cancel: &AtomicBool::new(false),
            log: &log,
        },
        &target,
    )
    .unwrap();
    assert!(r.ok(), "{r:#?}");

    let mut failures = run_apt(&tree, &ours.public, SCRIPT, &installed);
    failures.extend(run_apt(&tree, &up.key.public, UPDATE_ONLY, &|_, out| {
        !out.contains("exit=0")
            || out
                .lines()
                .any(|l| l.starts_with("W:") || l.starts_with("E:"))
    }));
    assert!(
        failures.is_empty(),
        "apt failed:\n{}",
        failures.join("\n---\n")
    );
}
