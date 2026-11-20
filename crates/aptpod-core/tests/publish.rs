// SPDX-License-Identifier: GPL-3.0-only

//! Publishes a signed dist from real `.deb` files and checks it the way
//! clients and other tools would.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use aptpod_core::Error;
use aptpod_core::control::parse_paragraphs;
use aptpod_core::deb::read_deb;
use aptpod_core::hash::hash_file;
use aptpod_core::index::ReleaseMeta;
use aptpod_core::publish::{ComponentSpec, DistSpec, PublishPackage, publish_dist};
use aptpod_core::sign::Signer;
use aptpod_core::verify::{Keyring, verify_detached, verify_inrelease};
use common::{TestKey, build_deb, have};

const DIST: &str = "aptpodtest";

struct Fixture {
    dir: tempfile::TempDir,
    root: PathBuf,
    debs: Vec<PathBuf>,
}

fn fixture() -> Option<Fixture> {
    if !have("dpkg-deb") {
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let debs = vec![
        build_deb(d, "hello-aptpod", "1.0", "amd64", "gzip", ""),
        build_deb(d, "hello-aptpod", "1.1", "amd64", "xz", ""),
        build_deb(d, "hello-aptpod", "1.1", "arm64", "zstd", ""),
        build_deb(d, "common-aptpod", "1:0.5", "all", "none", ""),
        build_deb(
            d,
            "libfoo1",
            "2.0-1+b1",
            "amd64",
            "xz",
            "Source: libfoo (2.0-1)\n",
        ),
    ];
    let root = d.join("root");
    Some(Fixture { dir, root, debs })
}

fn spec(debs: &[PathBuf]) -> DistSpec {
    let packages = debs
        .iter()
        .map(|f| PublishPackage {
            info: read_deb(f).unwrap(),
            file: f.clone(),
        })
        .collect();
    let mut meta = ReleaseMeta::new(DIST, "aptpod-test", "aptpod test");
    meta.description = Some("aptpod integration test".into());
    DistSpec {
        name: DIST.into(),
        meta,
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
    }
}

fn dist(root: &Path) -> PathBuf {
    root.join("dists").join(DIST)
}

/// (name, version, arch) -> stanza fields
fn stanzas(text: &str) -> BTreeMap<(String, String, String), BTreeMap<String, String>> {
    parse_paragraphs(text)
        .unwrap()
        .into_iter()
        .map(|p| {
            let key = (
                p.get("Package").unwrap().to_string(),
                p.get("Version").unwrap().to_string(),
                p.get("Architecture").unwrap().to_string(),
            );
            (
                key,
                p.fields()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn signed_publish_is_consistent() {
    let (Some(fx), Some(key)) = (fixture(), TestKey::new()) else {
        return;
    };
    let signer = key.signer();
    let report = publish_dist(&fx.root, &spec(&fx.debs), Some(&signer as &dyn Signer)).unwrap();
    assert_eq!(report.pool_linked + report.pool_copied, 5);
    let dir = dist(&fx.root);

    // layout
    for f in [
        "Release",
        "InRelease",
        "Release.gpg",
        "main/binary-amd64/Packages.xz",
        "extra/binary-arm64/Packages",
        "main/binary-arm64/Release",
    ] {
        assert!(dir.join(f).is_file(), "missing {f}");
    }
    assert!(
        fx.root
            .join("pool/main/libf/libfoo/libfoo1_2.0-1+b1_amd64.deb")
            .is_file(),
        "lib* pool prefix uses the Source name"
    );
    assert!(
        fx.root
            .join("pool/main/c/common-aptpod/common-aptpod_0.5_all.deb")
            .is_file(),
        "epoch is not in the file name"
    );
    assert_eq!(
        fs::read(dir.join("extra/binary-amd64/Packages")).unwrap(),
        b"",
        "empty component"
    );

    // signatures
    let release = fs::read(dir.join("Release")).unwrap();
    let ring = Keyring::from_bytes(&key.public).unwrap();
    let signed = verify_inrelease(&ring, &fs::read(dir.join("InRelease")).unwrap()).unwrap();
    assert_eq!(
        signed.as_bytes(),
        &release[..],
        "InRelease payload is the Release file"
    );
    verify_detached(&ring, &release, &fs::read(dir.join("Release.gpg")).unwrap()).unwrap();
    let mut tampered = release.clone();
    tampered[0] ^= 1;
    assert!(
        verify_detached(
            &ring,
            &tampered,
            &fs::read(dir.join("Release.gpg")).unwrap()
        )
        .is_err()
    );
    if let Some(other) = TestKey::new() {
        let wrong = Keyring::from_bytes(&other.public).unwrap();
        assert!(verify_inrelease(&wrong, &fs::read(dir.join("InRelease")).unwrap()).is_err());
    }
    if have("gpgv") {
        let kr = fx.dir.path().join("key.gpg");
        fs::write(&kr, &key.public).unwrap();
        let st = Command::new("gpgv")
            .arg("--keyring")
            .arg(&kr)
            .arg(dir.join("InRelease"))
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "gpgv InRelease: {}",
            String::from_utf8_lossy(&st.stderr)
        );
        let st = Command::new("gpgv")
            .arg("--keyring")
            .arg(&kr)
            .arg(dir.join("Release.gpg"))
            .arg(dir.join("Release"))
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "gpgv Release.gpg: {}",
            String::from_utf8_lossy(&st.stderr)
        );
    }

    // every file listed in Release matches
    let rel = parse_paragraphs(std::str::from_utf8(&release).unwrap())
        .unwrap()
        .remove(0);
    let listed: Vec<&str> = rel
        .get("SHA256")
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(listed.len(), 4 * 2 * 2, "4 files x 2 archs x 2 components");
    for line in listed {
        let f: Vec<&str> = line.split_whitespace().collect();
        let h = hash_file(&dir.join(f[2])).unwrap();
        assert_eq!(
            (h.sha256.as_str(), h.size.to_string().as_str()),
            (f[0], f[1]),
            "{}",
            f[2]
        );
    }
    assert_eq!(rel.get("Components"), Some("main extra"));
    assert_eq!(rel.get("Architectures"), Some("amd64 arm64"));

    // all-arch packages are in every binary-<arch>
    let amd = fs::read_to_string(dir.join("main/binary-amd64/Packages")).unwrap();
    let arm = fs::read_to_string(dir.join("main/binary-arm64/Packages")).unwrap();
    assert!(amd.contains("Package: common-aptpod") && arm.contains("Package: common-aptpod"));
    assert_eq!(stanzas(&amd).len(), 4);
    assert_eq!(stanzas(&arm).len(), 2);
}

/// Our Packages stanzas carry the same fields and values as
/// apt-ftparchive's for the same pool (order aside).
#[test]
fn packages_match_apt_ftparchive() {
    let Some(fx) = fixture() else { return };
    if !have("apt-ftparchive") {
        return;
    }
    publish_dist(&fx.root, &spec(&fx.debs), None).unwrap();
    let dir = dist(&fx.root);
    let mut ours = stanzas(&fs::read_to_string(dir.join("main/binary-amd64/Packages")).unwrap());
    ours.extend(stanzas(
        &fs::read_to_string(dir.join("main/binary-arm64/Packages")).unwrap(),
    ));
    let out = Command::new("apt-ftparchive")
        .args(["packages", "pool/main"])
        .current_dir(&fx.root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let theirs = stanzas(&String::from_utf8(out.stdout).unwrap());
    assert_eq!(
        ours.keys().collect::<Vec<_>>(),
        theirs.keys().collect::<Vec<_>>()
    );
    const IGNORE: &[&str] = &["Description-md5"];
    for (k, t) in &theirs {
        let o = &ours[k];
        let tf: BTreeMap<_, _> = t
            .iter()
            .filter(|(f, _)| !IGNORE.contains(&f.as_str()))
            .collect();
        let of: BTreeMap<_, _> = o
            .iter()
            .filter(|(f, _)| !IGNORE.contains(&f.as_str()))
            .collect();
        assert_eq!(of, tf, "stanza {k:?}");
    }
}

#[test]
fn republish_swaps_and_conflicts_are_refused() {
    let Some(fx) = fixture() else { return };
    publish_dist(&fx.root, &spec(&fx.debs[..2]), None).unwrap();
    let first = fs::read_to_string(dist(&fx.root).join("main/binary-amd64/Packages")).unwrap();
    let report = publish_dist(&fx.root, &spec(&fx.debs), None).unwrap();
    assert!(report.atomic_swap);
    let second = fs::read_to_string(dist(&fx.root).join("main/binary-amd64/Packages")).unwrap();
    assert_ne!(first, second);
    let leftovers: Vec<_> = fs::read_dir(fx.root.join("dists"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        leftovers,
        vec![std::ffi::OsString::from(DIST)],
        "no temporary dirs left"
    );

    // same name/version/arch, different content
    let other = tempfile::tempdir().unwrap();
    let evil = build_deb(
        other.path(),
        "hello-aptpod",
        "1.1",
        "amd64",
        "gzip",
        "X-Changed: yes\n",
    );
    let mut bad = fx.debs.clone();
    bad.push(evil);
    bad.remove(1);
    let err = publish_dist(&fx.root, &spec(&bad), None).unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err}");
    let after = fs::read_to_string(dist(&fx.root).join("main/binary-amd64/Packages")).unwrap();
    assert_eq!(after, second, "a failed publish leaves the dist untouched");
}

#[test]
fn rejects_packages_of_unpublished_arch() {
    let Some(fx) = fixture() else { return };
    let mut s = spec(&fx.debs);
    s.architectures = vec!["amd64".into()];
    let err = publish_dist(&fx.root, &s, None).unwrap_err();
    assert!(err.to_string().contains("arm64"), "{err}");
}
