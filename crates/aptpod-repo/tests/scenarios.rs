// SPDX-License-Identifier: GPL-3.0-only

//! End-to-end scenarios of the repository engine, ported from the aptrix
//! integration tests (minus the parts that only existed for aptly) plus the
//! consistency rules of aptpod.

mod common;

use std::cell::Cell as StdCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use aptpod_core::sign::Signer;
use aptpod_repo::ops::{AddFile, Dists, Promote, RevRef};
use aptpod_repo::{Change, Error, Filter, Layout, Plan, Repo};
use common::{TestKey, build_deb, have};

struct Env {
    dir: tempfile::TempDir,
    repo: Repo,
    n: StdCell<usize>,
}

impl Env {
    fn new() -> Option<Self> {
        if !have("dpkg-deb") {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open(&dir.path().join("data")).unwrap();
        Some(Self {
            dir,
            repo,
            n: StdCell::new(0),
        })
    }

    fn root(&self, alias: &str) -> PathBuf {
        self.dir.path().join("trees").join(alias)
    }

    fn layout(&self, alias: &str, dists: &[&str], comps: &[&str], archs: &[&str]) -> Layout {
        let mut l = Layout::new(self.root(alias), dists, comps, archs);
        l.sign = false;
        l.origin = "aptpod-test".into();
        l.label = alias.into();
        l
    }

    /// A unique .deb (marker distinguishes same name/version).
    fn deb(&self, name: &str, version: &str, arch: &str, marker: &str) -> AddFile {
        self.n.set(self.n.get() + 1);
        let d = self
            .dir
            .path()
            .join(format!("debs-{}-{marker}", self.n.get()));
        fs::create_dir_all(&d).unwrap();
        build_deb(
            &d,
            name,
            version,
            arch,
            "xz",
            &format!("X-Marker: {marker}\n"),
        )
        .into()
    }

    fn apply(&mut self, plan: Plan) -> Plan {
        self.repo.apply(&plan, "test").unwrap();
        plan
    }

    fn packages(&self, alias: &str, dist: &str, comp: &str, arch: &str) -> String {
        fs::read_to_string(
            self.root(alias)
                .join(format!("dists/{dist}/{comp}/binary-{arch}/Packages")),
        )
        .unwrap_or_default()
    }

    fn versions(&self, alias: &str, dist: &str, comp: &str, name: &str) -> Vec<String> {
        self.repo
            .list(
                alias,
                Some(&[comp.to_string()]),
                Some(&[dist.to_string()]),
                &Filter::new(&[name], None, None).unwrap(),
            )
            .unwrap()
            .into_iter()
            .map(|r| r.package.version)
            .collect()
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn f(specs: &[&str]) -> Filter {
    Filter::new(specs, None, None).unwrap()
}

#[test]
fn matrix_lifecycle() {
    let Some(mut e) = Env::new() else { return };
    let mut l = e.layout("t", &["jammy", "noble"], &["main", "extra"], &["arm64"]);
    l.only.insert("extra".into(), s(&["noble"]));
    let plan = e.repo.plan_create("t", l.clone(), false).unwrap();
    assert!(
        plan.lines().contains(&"+ cell     noble/extra".to_string()),
        "{:?}",
        plan.lines()
    );
    e.apply(plan);
    assert_eq!(e.repo.status("t").unwrap().len(), 3);
    assert!(e.root("t").join("dists/jammy/Release").is_file());
    assert_eq!(
        fs::read_to_string(e.root("t").join("dists/noble/Release"))
            .unwrap()
            .lines()
            .find(|l| l.starts_with("Components")),
        Some("Components: main extra")
    );

    // add with dist detection, multi-dist add, and errors
    let files = [
        e.deb("hello", "1.0~noble", "arm64", "a"),
        e.deb("hello", "1.1~noble", "arm64", "a"),
        e.deb("hello", "1.2~noble", "arm64", "a"),
    ];
    e.apply(
        e.repo
            .plan_add("t", "main", &files, &Dists::Detect, false)
            .unwrap(),
    );
    let tool = e.deb("tool", "2.0", "all", "a");
    e.apply(
        e.repo
            .plan_add("t", "main", std::slice::from_ref(&tool), &Dists::All, false)
            .unwrap(),
    );
    assert!(
        e.packages("t", "noble", "main", "arm64")
            .contains("Version: 1.2~noble")
    );
    assert!(
        e.packages("t", "jammy", "main", "arm64")
            .contains("Package: tool")
    );
    let err = |r: Result<Plan, Error>| r.unwrap_err().to_string();
    assert!(
        err(e.repo.plan_add(
            "t",
            "main",
            &[e.deb("nodist", "1.0", "arm64", "a")],
            &Dists::Detect,
            false
        ))
        .contains("cannot tell the dist")
    );
    assert!(
        err(e.repo.plan_add(
            "t",
            "extra",
            &[e.deb("x", "1.0~jammy", "arm64", "a")],
            &Dists::Detect,
            false
        ))
        .contains("not a cell")
    );
    assert!(
        err(e.repo.plan_add(
            "t",
            "main",
            &[e.deb("y", "1.0~noble", "amd64", "a")],
            &Dists::Detect,
            false
        ))
        .contains("architecture amd64")
    );
    let changed = e.deb("hello", "1.0~noble", "arm64", "b");
    assert!(matches!(
        e.repo.plan_add(
            "t",
            "main",
            std::slice::from_ref(&changed),
            &Dists::Detect,
            false
        ),
        Err(Error::Conflict(_))
    ));
    let plan = e
        .repo
        .plan_add(
            "t",
            "main",
            std::slice::from_ref(&changed),
            &Dists::Detect,
            true,
        )
        .unwrap();
    assert!(
        plan.changes()
            .any(|(_, c)| matches!(c, Change::Replace { .. }))
    );
    e.apply(plan);
    assert!(
        fs::read(
            e.root("t")
                .join("pool/main/h/hello/hello_1.0~noble_arm64.deb")
        )
        .unwrap()
            == fs::read(&changed.path).unwrap(),
        "pool file replaced"
    );

    // listing: newest first
    assert_eq!(
        e.versions("t", "noble", "main", "hello"),
        ["1.2~noble", "1.1~noble", "1.0~noble"]
    );

    // rm --keep 1, rm name=version in one dist
    e.apply(
        e.repo
            .plan_rm("t", "main", &f(&["hello"]), None, Some(1))
            .unwrap(),
    );
    assert_eq!(e.versions("t", "noble", "main", "hello"), ["1.2~noble"]);
    assert!(
        !e.root("t")
            .join("pool/main/h/hello/hello_1.0~noble_arm64.deb")
            .exists(),
        "unused pool file removed"
    );
    e.apply(
        e.repo
            .plan_rm("t", "main", &f(&["tool=2.0"]), Some(&s(&["jammy"])), None)
            .unwrap(),
    );
    assert!(
        !e.packages("t", "jammy", "main", "arm64")
            .contains("Package: tool")
    );
    assert!(
        e.packages("t", "noble", "main", "arm64")
            .contains("Package: tool")
    );
    assert!(
        e.repo
            .plan_rm("t", "main", &f(&["nothing-*"]), None, None)
            .unwrap()
            .is_empty()
    );

    // new component everywhere
    let mut l2 = l.clone();
    l2.components.push("new".into());
    let plan = e.repo.plan_layout("t", l2.clone(), false, false).unwrap();
    assert!(
        plan.lines().iter().any(|x| x == "+ cell     jammy/new"),
        "{:?}",
        plan.lines()
    );
    e.apply(plan);
    assert_eq!(
        fs::read_to_string(e.root("t").join("dists/jammy/Release"))
            .unwrap()
            .lines()
            .find(|x| x.starts_with("Components")),
        Some("Components: main new")
    );

    // removing a component that holds packages needs force
    e.apply(
        e.repo
            .plan_add(
                "t",
                "extra",
                &[e.deb("ext", "1.0~noble", "arm64", "a")],
                &Dists::Detect,
                false,
            )
            .unwrap(),
    );
    let mut l3 = l2.clone();
    l3.components.retain(|c| c != "extra");
    l3.only.clear();
    assert!(matches!(
        e.repo.plan_layout("t", l3.clone(), false, false),
        Err(Error::Blocked(_))
    ));
    e.apply(e.repo.plan_layout("t", l3.clone(), true, false).unwrap());
    assert!(!e.root("t").join("pool/extra").exists());

    // dropping a dist removes its directory
    let mut l4 = l3.clone();
    l4.dists = s(&["noble"]);
    e.apply(e.repo.plan_layout("t", l4, true, false).unwrap());
    assert!(!e.root("t").join("dists/jammy").exists());
    assert!(e.root("t").join("dists/noble/Release").is_file());
}

#[test]
fn rollback_restores_and_is_undoable() {
    let Some(mut e) = Env::new() else { return };
    e.apply(
        e.repo
            .plan_create(
                "t",
                e.layout("t", &["noble"], &["main", "extra"], &["arm64"]),
                false,
            )
            .unwrap(),
    );
    let files = [
        e.deb("hello", "1.0~noble", "arm64", "a"),
        e.deb("hello", "1.1~noble", "arm64", "a"),
        e.deb("hello-dbg", "1.0~noble", "arm64", "a"),
        e.deb("hello-dbg", "1.1~noble", "arm64", "a"),
        e.deb("tool", "1.0~noble", "arm64", "a"),
    ];
    e.apply(
        e.repo
            .plan_add("t", "main", &files, &Dists::Detect, false)
            .unwrap(),
    );
    e.apply(
        e.repo
            .plan_add(
                "t",
                "extra",
                &[e.deb("x-dbg", "1.0~noble", "arm64", "a")],
                &Dists::Detect,
                false,
            )
            .unwrap(),
    );
    e.repo
        .snapshot_create("t", "before-rm", "", &RevRef::Head)
        .unwrap();
    assert!(
        e.repo
            .snapshot_create("t", "before-rm", "", &RevRef::Head)
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );

    let plan = e
        .repo
        .plan_rm("t", "main", &f(&["*-dbg"]), None, None)
        .unwrap();
    assert_eq!(
        plan.changes().count(),
        2,
        "glob only in the named component"
    );
    e.apply(plan);
    assert!(
        !e.packages("t", "noble", "main", "arm64")
            .contains("hello-dbg")
    );
    assert!(e.packages("t", "noble", "extra", "arm64").contains("x-dbg"));
    let after_rm = e.repo.history("t", 1).unwrap()[0].id;

    let plan = e
        .repo
        .plan_rollback("t", &RevRef::parse("before-rm"))
        .unwrap();
    assert_eq!(plan.changes().count(), 2);
    e.apply(plan);
    assert_eq!(e.versions("t", "noble", "main", "hello-dbg").len(), 2);
    assert!(
        e.repo
            .plan_rollback("t", &RevRef::parse("before-rm"))
            .unwrap()
            .is_empty(),
        "already there"
    );
    // the rollback is itself a revision: undo it
    e.apply(
        e.repo
            .plan_rollback("t", &RevRef::Revision(after_rm))
            .unwrap(),
    );
    assert!(e.versions("t", "noble", "main", "hello-dbg").is_empty());

    // keep per package name with a glob
    e.apply(
        e.repo
            .plan_rollback("t", &RevRef::parse("before-rm"))
            .unwrap(),
    );
    e.apply(
        e.repo
            .plan_rm("t", "main", &f(&["hello*"]), None, Some(1))
            .unwrap(),
    );
    assert_eq!(e.versions("t", "noble", "main", "hello"), ["1.1~noble"]);
    assert_eq!(e.versions("t", "noble", "main", "hello-dbg"), ["1.1~noble"]);

    assert_eq!(e.repo.snapshots("t").unwrap().len(), 1);
    e.repo.snapshot_drop("t", "before-rm").unwrap();
    assert!(matches!(
        e.repo.plan_rollback("t", &RevRef::parse("before-rm")),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn retention_promote_find_and_diff() {
    let Some(mut e) = Env::new() else { return };
    let mut l = e.layout("b", &["jammy", "noble"], &["main", "testing"], &["arm64"]);
    l.keep = 2;
    e.apply(e.repo.plan_create("b", l, false).unwrap());
    let files = [
        e.deb("kern", "1.0~noble", "arm64", "a"),
        e.deb("kern", "1.1~noble", "arm64", "a"),
        e.deb("kern", "1.2~noble", "arm64", "a"),
    ];
    e.apply(
        e.repo
            .plan_add("b", "main", &files, &Dists::Detect, false)
            .unwrap(),
    );
    assert_eq!(
        e.versions("b", "noble", "main", "kern"),
        ["1.2~noble", "1.1~noble"],
        "keep = 2 applied on add"
    );
    e.apply(
        e.repo
            .plan_add(
                "b",
                "main",
                &[e.deb("tool", "1.0", "all", "a")],
                &Dists::All,
                false,
            )
            .unwrap(),
    );

    // find
    assert_eq!(
        e.repo.find(None, &f(&["kern", "t*"]), false).unwrap().len(),
        4
    );
    assert_eq!(e.repo.find(None, &f(&["kern"]), true).unwrap().len(), 1);
    let found = e
        .repo
        .find(
            None,
            &Filter::new(&["kern"], Some(">=1.2~noble"), None).unwrap(),
            false,
        )
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].package.version, "1.2~noble");

    e.repo
        .snapshot_create("b", "before-promote", "", &RevRef::Head)
        .unwrap();

    // promote copies the newest version; again is a no-op
    let plan = e
        .repo
        .plan_promote(
            ("b", "main"),
            ("b", "testing"),
            &f(&["kern"]),
            &Promote::default(),
        )
        .unwrap();
    assert_eq!(plan.lines(), vec!["+ noble/testing kern 1.2~noble arm64"]);
    e.apply(plan);
    assert!(
        e.repo
            .plan_promote(
                ("b", "main"),
                ("b", "testing"),
                &f(&["kern"]),
                &Promote::default()
            )
            .unwrap()
            .is_empty()
    );
    // --to-dist
    let to = Promote {
        dists: Some(s(&["noble"])),
        to_dist: Some("jammy".into()),
        ..Default::default()
    };
    e.apply(
        e.repo
            .plan_promote(("b", "main"), ("b", "testing"), &f(&["tool"]), &to)
            .unwrap(),
    );
    assert!(
        e.packages("b", "jammy", "testing", "arm64")
            .contains("Package: tool")
    );
    // --move back out of testing
    let mv = Promote {
        move_: true,
        ..Default::default()
    };
    e.apply(
        e.repo
            .plan_promote(("b", "testing"), ("b", "main"), &f(&["kern"]), &mv)
            .unwrap(),
    );
    assert!(e.versions("b", "noble", "testing", "kern").is_empty());
    assert!(matches!(
        e.repo.plan_promote(
            ("b", "main"),
            ("b", "main"),
            &f(&["tool"]),
            &Promote::default()
        ),
        Err(Error::Invalid(_))
    ));

    // prune-versions
    let plan = e
        .repo
        .plan_prune_versions("b", Some("main"), None, Some(1), &Filter::all())
        .unwrap();
    assert_eq!(plan.lines(), vec!["- noble/main kern 1.1~noble arm64"]);
    e.apply(plan);
    assert!(
        e.repo
            .plan_prune_versions("b", None, None, Some(1), &Filter::all())
            .unwrap()
            .is_empty()
    );

    // diff
    let d = e
        .repo
        .diff("b", &RevRef::parse("before-promote"), &RevRef::Head)
        .unwrap();
    let shown: Vec<String> = d
        .iter()
        .map(|x| {
            format!(
                "{} {}/{} {} {}",
                x.change, x.dist, x.component, x.package.name, x.package.version
            )
        })
        .collect();
    assert_eq!(
        shown,
        ["+ jammy/testing tool 1.0", "- noble/main kern 1.1~noble"]
    );
    assert!(
        e.repo
            .diff("b", &RevRef::Head, &RevRef::Head)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn history_expiry_and_gc() {
    let Some(mut e) = Env::new() else { return };
    let now = Arc::new(AtomicI64::new(1_000_000));
    let clock = now.clone();
    e.repo
        .set_clock(Box::new(move || clock.load(Ordering::SeqCst)));
    let mut l = e.layout("b", &["noble"], &["main"], &["arm64"]);
    l.history_max_age = Some(100);
    e.apply(e.repo.plan_create("b", l, false).unwrap());
    let old = e.deb("kern", "1.0~noble", "arm64", "a");
    e.apply(
        e.repo
            .plan_add(
                "b",
                "main",
                std::slice::from_ref(&old),
                &Dists::Detect,
                false,
            )
            .unwrap(),
    );
    e.repo
        .snapshot_create("b", "keep-me", "", &RevRef::Head)
        .unwrap();
    e.apply(
        e.repo
            .plan_rm("b", "main", &Filter::all(), None, None)
            .unwrap(),
    );
    let dbg = e.deb("kern-dbg", "1.0~noble", "arm64", "a");
    e.apply(
        e.repo
            .plan_add(
                "b",
                "main",
                std::slice::from_ref(&dbg),
                &Dists::Detect,
                false,
            )
            .unwrap(),
    );
    e.apply(
        e.repo
            .plan_rm("b", "main", &Filter::all(), None, None)
            .unwrap(),
    );
    // create, add, rm, add, rm (a label is not a revision)
    assert_eq!(e.repo.history("b", 100).unwrap().len(), 5);

    now.fetch_add(1_000, Ordering::SeqCst);
    let expired = e.repo.expire_history("b").unwrap();
    let left: Vec<_> = e
        .repo
        .history("b", 100)
        .unwrap()
        .into_iter()
        .map(|r| (r.message, r.labels))
        .collect();
    assert_eq!(expired, 3, "{left:?}");
    assert_eq!(
        left.len(),
        2,
        "head and the labelled revision stay: {left:?}"
    );

    // kern-dbg is in no remaining revision: gc removes its row and file
    // (once it is older than the grace period); kern is pinned by the label
    assert_eq!(e.repo.gc().unwrap().packages, 0, "within the grace period");
    now.fetch_add(aptpod_repo::history::GC_GRACE, Ordering::SeqCst);
    let gc = e.repo.gc().unwrap();
    assert_eq!(gc.packages, 1);
    let store = |p: &Path| aptpod_core::deb::read_deb(p).unwrap().hashes.sha256;
    let sp = |sha: &str| {
        e.dir
            .path()
            .join("data/store")
            .join(&sha[..2])
            .join(format!("{sha}.deb"))
    };
    assert!(!sp(&store(&dbg.path)).exists());
    assert!(sp(&store(&old.path)).exists());
    e.apply(
        e.repo
            .plan_rollback("b", &RevRef::parse("keep-me"))
            .unwrap(),
    );
    assert!(
        e.packages("b", "noble", "main", "arm64")
            .contains("Package: kern")
    );

    // snapshot prune by age
    now.fetch_add(1_000, Ordering::SeqCst);
    assert_eq!(
        e.repo
            .snapshot_prune("b", Some(500), 0, true)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(e.repo.snapshots("b").unwrap().len(), 1, "dry run");
    e.repo.snapshot_prune("b", Some(500), 0, false).unwrap();
    assert!(e.repo.snapshots("b").unwrap().is_empty());
}

#[test]
fn cross_publish_promote_and_delete() {
    let Some(mut e) = Env::new() else { return };
    e.apply(
        e.repo
            .plan_create(
                "b",
                e.layout("b", &["jammy", "noble"], &["main"], &["arm64"]),
                false,
            )
            .unwrap(),
    );
    e.apply(
        e.repo
            .plan_create(
                "pub",
                e.layout("pub", &["noble"], &["main"], &["amd64"]),
                false,
            )
            .unwrap(),
    );
    e.apply(
        e.repo
            .plan_add(
                "b",
                "main",
                &[e.deb("kern", "1.0~noble", "arm64", "a")],
                &Dists::Detect,
                false,
            )
            .unwrap(),
    );
    e.apply(
        e.repo
            .plan_add(
                "b",
                "main",
                &[e.deb("tool", "1.0", "all", "a")],
                &Dists::All,
                false,
            )
            .unwrap(),
    );

    let err = e
        .repo
        .plan_promote(
            ("b", "main"),
            ("pub", "main"),
            &f(&["kern"]),
            &Promote::default(),
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("architecture arm64 is not published by pub"),
        "{err}"
    );
    let plan = e
        .repo
        .plan_promote(
            ("b", "main"),
            ("pub", "main"),
            &f(&["tool"]),
            &Promote {
                move_: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(plan.parts.len(), 2);
    e.apply(plan);
    assert!(
        e.packages("pub", "noble", "main", "amd64")
            .contains("Package: tool")
    );
    assert!(
        !e.packages("b", "noble", "main", "arm64")
            .contains("Package: tool")
    );
    assert!(
        e.packages("b", "jammy", "main", "arm64")
            .contains("Package: tool"),
        "only dists both sides have"
    );

    // paths may not overlap
    let nested = Layout {
        path: e.root("b").join("sub"),
        ..e.layout("x", &["noble"], &["main"], &["arm64"])
    };
    assert!(
        e.repo
            .plan_create("x", nested, false)
            .unwrap_err()
            .to_string()
            .contains("overlaps")
    );

    // delete: blocked while packages remain; removes only what aptpod wrote
    fs::write(e.root("pub").join("README"), "keep me").unwrap();
    assert!(matches!(
        e.repo.plan_delete("pub", false),
        Err(Error::Blocked(_))
    ));
    e.apply(e.repo.plan_delete("pub", true).unwrap());
    assert!(!e.root("pub").join("dists").exists() && !e.root("pub").join("pool").exists());
    assert_eq!(
        fs::read_to_string(e.root("pub").join("README")).unwrap(),
        "keep me"
    );
    assert!(e.repo.status("pub").is_err());
}

#[test]
fn stale_plans_are_refused() {
    let Some(mut e) = Env::new() else { return };
    e.apply(
        e.repo
            .plan_create("t", e.layout("t", &["noble"], &["main"], &["arm64"]), false)
            .unwrap(),
    );
    let a = e
        .repo
        .plan_add(
            "t",
            "main",
            &[e.deb("a1", "1.0~noble", "arm64", "a")],
            &Dists::Detect,
            false,
        )
        .unwrap();
    let b = e
        .repo
        .plan_add(
            "t",
            "main",
            &[e.deb("b1", "1.0~noble", "arm64", "a")],
            &Dists::Detect,
            false,
        )
        .unwrap();
    e.apply(a);
    assert!(matches!(e.repo.apply(&b, "test"), Err(Error::Stale(_))));
}

#[test]
fn replace_applies_to_every_dist() {
    let Some(mut e) = Env::new() else { return };
    e.apply(
        e.repo
            .plan_create(
                "t",
                e.layout("t", &["jammy", "noble"], &["main"], &["arm64"]),
                false,
            )
            .unwrap(),
    );
    e.apply(
        e.repo
            .plan_add(
                "t",
                "main",
                &[e.deb("tool", "1.0", "all", "a")],
                &Dists::All,
                false,
            )
            .unwrap(),
    );
    let changed = e.deb("tool", "1.0", "all", "b");
    // targeting one dist still replaces the shared pool file in both
    let plan = e
        .repo
        .plan_add(
            "t",
            "main",
            std::slice::from_ref(&changed),
            &Dists::List(s(&["noble"])),
            true,
        )
        .unwrap();
    e.apply(plan);
    let sha = aptpod_core::deb::read_deb(&changed.path)
        .unwrap()
        .hashes
        .sha256;
    for d in ["jammy", "noble"] {
        assert!(
            e.packages("t", d, "main", "arm64").contains(&sha),
            "{d} lists the new content"
        );
    }
}

/// A signer that fails after `ok` signatures.
struct FlakySigner {
    inner: aptpod_core::sign::GpgSigner,
    ok: Arc<AtomicUsize>,
}

impl Signer for FlakySigner {
    fn clearsign(&self, data: &[u8]) -> Result<Vec<u8>, aptpod_core::Error> {
        if self.ok.load(Ordering::SeqCst) == 0 {
            return Err(aptpod_core::Error::Sign("injected failure".into()));
        }
        self.ok.fetch_sub(1, Ordering::SeqCst);
        self.inner.clearsign(data)
    }
    fn detach_sign(&self, data: &[u8]) -> Result<Vec<u8>, aptpod_core::Error> {
        self.inner.detach_sign(data)
    }
}

#[test]
fn signed_publish_and_repair_after_failure() {
    let Some(mut e) = Env::new() else { return };
    let Some(key) = TestKey::new() else { return };
    let mut l = e.layout("s", &["jammy", "noble"], &["main"], &["arm64"]);
    l.sign = true;
    assert!(
        e.repo
            .plan_create("s", l.clone(), false)
            .unwrap_err()
            .to_string()
            .contains("no signing key")
    );
    e.repo.set_signer(Some(Box::new(key.signer())));
    e.apply(e.repo.plan_create("s", l, false).unwrap());
    let ring = aptpod_core::verify::Keyring::from_bytes(&key.public).unwrap();
    let inrelease = fs::read(e.root("s").join("dists/noble/InRelease")).unwrap();
    assert!(aptpod_core::verify::verify_inrelease(&ring, &inrelease).is_ok());

    // the first dist signs, the second fails: DB moved, tree half-updated
    let ok = Arc::new(AtomicUsize::new(1));
    e.repo.set_signer(Some(Box::new(FlakySigner {
        inner: key.signer(),
        ok: ok.clone(),
    })));
    let plan = e
        .repo
        .plan_add(
            "s",
            "main",
            &[e.deb("tool", "1.0", "all", "a")],
            &Dists::All,
            false,
        )
        .unwrap();
    assert!(e.repo.apply(&plan, "test").is_err());
    assert!(
        e.packages("s", "jammy", "main", "arm64")
            .contains("Package: tool")
    );
    assert!(
        !e.packages("s", "noble", "main", "arm64")
            .contains("Package: tool")
    );

    // repair brings the tree to the head
    e.repo.set_signer(Some(Box::new(key.signer())));
    let fixed = e.repo.repair().unwrap();
    assert_eq!(
        fixed.published,
        vec![("s".to_string(), "noble".to_string())]
    );
    assert!(
        e.packages("s", "noble", "main", "arm64")
            .contains("Package: tool")
    );
    assert!(
        e.repo.repair().unwrap().published.is_empty(),
        "nothing left to repair"
    );

    // a --replace interrupted after the first dist: repair replaces the
    // rest even though no plan carries the replacement any more
    let changed = e.deb("tool", "1.0", "all", "b");
    let ok = Arc::new(AtomicUsize::new(1));
    e.repo.set_signer(Some(Box::new(FlakySigner {
        inner: key.signer(),
        ok,
    })));
    let plan = e
        .repo
        .plan_add(
            "s",
            "main",
            std::slice::from_ref(&changed),
            &Dists::All,
            true,
        )
        .unwrap();
    assert!(matches!(
        e.repo.apply(&plan, "test"),
        Err(Error::Unsynced(_))
    ));
    e.repo.set_signer(Some(Box::new(key.signer())));
    e.repo.repair().unwrap();
    let sha = aptpod_core::deb::read_deb(&changed.path)
        .unwrap()
        .hashes
        .sha256;
    for d in ["jammy", "noble"] {
        assert!(e.packages("s", d, "main", "arm64").contains(&sha), "{d}");
    }
    assert_eq!(
        fs::read(e.root("s").join("pool/main/t/tool/tool_1.0_all.deb")).unwrap(),
        fs::read(&changed.path).unwrap()
    );
}

#[test]
fn replace_rollback_and_input_checks() {
    let Some(mut e) = Env::new() else { return };
    e.apply(
        e.repo
            .plan_create(
                "t",
                e.layout("t", &["jammy", "noble"], &["main"], &["arm64"]),
                false,
            )
            .unwrap(),
    );
    let a = e.deb("tool", "1.0", "all", "a");
    e.apply(
        e.repo
            .plan_add("t", "main", std::slice::from_ref(&a), &Dists::All, false)
            .unwrap(),
    );
    let before = e.repo.history("t", 1).unwrap()[0].id;
    let b = e.deb("tool", "1.0", "all", "b");
    e.apply(
        e.repo
            .plan_add("t", "main", std::slice::from_ref(&b), &Dists::All, true)
            .unwrap(),
    );
    let pool = e.root("t").join("pool/main/t/tool/tool_1.0_all.deb");
    assert_eq!(fs::read(&pool).unwrap(), fs::read(&b.path).unwrap());
    // rolling back over a replace puts the old content back
    e.apply(
        e.repo
            .plan_rollback("t", &RevRef::Revision(before))
            .unwrap(),
    );
    assert_eq!(fs::read(&pool).unwrap(), fs::read(&a.path).unwrap());
    assert!(e.repo.repair().unwrap().published.is_empty());

    // two different files with the same name/version/arch in one add
    let c = e.deb("pair", "1.0", "all", "c");
    let d = e.deb("pair", "1.0", "all", "d");
    let err = e
        .repo
        .plan_add("t", "main", &[c, d], &Dists::All, true)
        .unwrap_err();
    assert!(
        matches!(err, Error::Conflict(_)) && err.to_string().contains("two files"),
        "{err}"
    );

    // expected sha256
    let mut x = e.deb("checked", "1.0", "all", "a");
    x.sha256 = Some("0".repeat(64));
    assert!(
        e.repo
            .plan_add("t", "main", &[x], &Dists::All, false)
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );

    // Source newer than the binary: allowed, with a warning
    let dir = e.dir.path().join("srcwarn");
    fs::create_dir_all(&dir).unwrap();
    let w: AddFile = build_deb(
        &dir,
        "libbar1",
        "2.0",
        "all",
        "xz",
        "Source: libbar (2.0-1)\n",
    )
    .into();
    let plan = e
        .repo
        .plan_add("t", "main", &[w], &Dists::All, false)
        .unwrap();
    assert!(
        plan.warnings.iter().any(|x| x.contains("apt 2.6")),
        "{:?}",
        plan.warnings
    );

    // a fresh, unreferenced package survives gc (a plan may still use it)
    let shas_before = fs::read_dir(e.dir.path().join("data/store"))
        .unwrap()
        .count();
    e.repo.gc().unwrap();
    assert_eq!(
        fs::read_dir(e.dir.path().join("data/store"))
            .unwrap()
            .count(),
        shas_before
    );
}

#[test]
fn populated_paths_need_adopt_and_moves_are_precise() {
    let Some(mut e) = Env::new() else { return };
    let root = e.root("t");
    fs::create_dir_all(root.join("dists/noble")).unwrap();
    fs::create_dir_all(root.join("pool/main/o/old")).unwrap();
    fs::write(root.join("pool/main/o/old/old_1_all.deb"), "foreign").unwrap();
    fs::write(root.join("README"), "not ours").unwrap();
    let l = e.layout("t", &["noble"], &["main"], &["arm64"]);
    let err = e.repo.plan_create("t", l.clone(), false).unwrap_err();
    assert!(
        matches!(err, Error::Blocked(_)) && err.to_string().contains("adopt"),
        "{err}"
    );
    e.apply(e.repo.plan_create("t", l.clone(), true).unwrap());
    assert!(root.join("dists/noble/Release").is_file());
    assert!(
        !root.join("pool/main/o/old/old_1_all.deb").exists(),
        "adopting deletes unlisted pool files"
    );
    assert_eq!(fs::read_to_string(root.join("README")).unwrap(), "not ours");

    e.apply(
        e.repo
            .plan_add(
                "t",
                "main",
                &[e.deb("tool", "1.0", "all", "a")],
                &Dists::All,
                false,
            )
            .unwrap(),
    );
    // moving to a populated path is refused too; to an empty one is precise
    let busy = e.dir.path().join("busy");
    fs::create_dir_all(busy.join("pool")).unwrap();
    let to_busy = Layout {
        path: busy,
        ..l.clone()
    };
    assert!(matches!(
        e.repo.plan_layout("t", to_busy, false, false),
        Err(Error::Blocked(_))
    ));
    fs::create_dir_all(root.join("dists/other-tool")).unwrap();
    let moved = Layout {
        path: e.dir.path().join("moved"),
        ..l
    };
    e.apply(
        e.repo
            .plan_layout("t", moved.clone(), false, false)
            .unwrap(),
    );
    assert!(moved.path.join("dists/noble/Release").is_file());
    assert!(
        moved
            .path
            .join("pool/main/t/tool/tool_1.0_all.deb")
            .is_file()
    );
    assert!(!root.join("dists/noble").exists() && !root.join("pool").exists());
    assert!(
        root.join("dists/other-tool").is_dir(),
        "dists aptpod did not record stay"
    );
    assert_eq!(fs::read_to_string(root.join("README")).unwrap(), "not ours");
}
