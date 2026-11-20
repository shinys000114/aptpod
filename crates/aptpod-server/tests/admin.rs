// SPDX-License-Identifier: GPL-3.0-only

//! Offline admin commands (`aptpod-server user ...`). Also makes `cargo
//! test` build the server binary that the CLI end-to-end test runs.

use std::io::Write;
use std::process::{Command, Stdio};

fn server(cfg: &std::path::Path, args: &[&str], stdin: &str) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aptpod-server"))
        .arg("-c")
        .arg(cfg)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}

#[test]
fn user_commands() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("server.toml");
    std::fs::write(
        &cfg,
        format!(
            "data_dir = {:?}\n",
            dir.path().join("data").display().to_string()
        ),
    )
    .unwrap();

    let add = |name: &str, role: &str, pw: &str| {
        server(
            &cfg,
            &["user", "add", name, "--role", role, "--password-stdin"],
            &format!("{pw}\n"),
        )
    };
    assert!(add("admin", "admin", "admin-password").0);
    let (ok, msg) = add("admin", "admin", "admin-password");
    assert!(!ok && msg.contains("already exists"), "{msg}");
    let (ok, msg) = add("ci", "publisher", "short");
    assert!(!ok && msg.contains("at least 8"), "{msg}");
    let (ok, msg) = add("bad name", "reader", "long enough");
    assert!(!ok && msg.contains("invalid user name"), "{msg}");
    let (ok, msg) = add("ci", "root", "long enough");
    assert!(!ok && msg.contains("unknown role"), "{msg}");
    assert!(add("ci", "publisher", "ci-password").0);

    assert!(server(&cfg, &["user", "role", "ci", "reader"], "").0);
    let (_, list) = server(&cfg, &["user", "ls"], "");
    assert_eq!(list, "admin\tadmin\nci\treader\n");
    assert!(server(&cfg, &["user", "del", "ci"], "").0);
    let (ok, msg) = server(&cfg, &["user", "del", "ci"], "");
    assert!(!ok && msg.contains("no user"), "{msg}");

    let db = dir.path().join("data/server.db");
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(db).unwrap().permissions());
    assert_eq!(mode & 0o077, 0, "server.db must be private");

    let (ok, msg) = server(&dir.path().join("missing.toml"), &["user", "ls"], "");
    assert!(!ok && msg.contains("missing.toml"), "{msg}");
}

/// A data directory of another user (as after `sudo aptpod-server user
/// add`) is refused instead of filled with files the owner cannot read.
#[test]
fn foreign_data_dir() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let me = std::fs::metadata(dir.path()).unwrap().uid();
    if std::fs::metadata("/").unwrap().uid() == me {
        return; // running as root: no foreign directory to try
    }
    let cfg = dir.path().join("server.toml");
    std::fs::write(&cfg, "data_dir = \"/\"\n").unwrap();
    for args in [&["user", "ls"][..], &["serve"][..]] {
        let (ok, msg) = server(&cfg, args, "");
        assert!(!ok && msg.contains("sudo -u aptpod"), "{msg}");
    }
}
