// SPDX-License-Identifier: GPL-3.0-only

//! Re-signed mirrors against a local signed upstream: the publish follows
//! the upstream (layout, contents, our signature), reruns fetch nothing,
//! changes are snapshots, filters and index stanzas are checked against
//! the files, failed dists keep their contents, the brake, interruption.

mod common;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use aptpod_core::testutil::TestKey;
use aptpod_core::verify::{Keyring, verify_inrelease};
use aptpod_mirror::Error;
use aptpod_mirror::fetch::Http;
use aptpod_mirror::resign::{ResignCtx, ResignMirror, ResignReport, Target, sync};
use aptpod_repo::mirror::{MirrorReport, MirrorSync};
use aptpod_repo::ops::RevRef;
use aptpod_repo::store::Store;
use aptpod_repo::{Filter, Repo};
use common::{Server, Upstream};

struct RepoTarget(Mutex<Repo>);

impl Target for RepoTarget {
    fn known(&self, sha256: &[String]) -> Result<HashSet<String>, Error> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .known_packages(sha256)?
            .into_iter()
            .collect())
    }
    fn apply(&self, s: MirrorSync) -> Result<MirrorReport, Error> {
        Ok(self.0.lock().unwrap().sync_mirror("rs", &s, "test")?)
    }
}

struct Env {
    up: Upstream,
    srv: Server,
    ours: TestKey,
    data: tempfile::TempDir,
    target: RepoTarget,
    http: Http,
}

impl Env {
    fn new() -> Option<Self> {
        let up = Upstream::new()?;
        let ours = TestKey::new()?;
        let srv = Server::start(&up.root);
        let data = tempfile::tempdir().unwrap();
        let mut repo = Repo::open(&data.path().join("data")).unwrap();
        repo.set_signer(Some(Box::new(ours.signer())));
        Some(Self {
            up,
            srv,
            ours,
            data,
            target: RepoTarget(Mutex::new(repo)),
            http: Http::new(Duration::from_secs(10))
                .unwrap()
                .with_backoff(Duration::from_millis(10)),
        })
    }

    fn path(&self) -> PathBuf {
        self.data.path().join("pub")
    }

    fn mirror(&self, dists: &[&str]) -> ResignMirror {
        let mut m = ResignMirror::new(&self.srv.url, dists, self.path());
        m.source.keyring = Some(Keyring::from_bytes(&self.up.key.public).unwrap());
        m.source.retries = 1;
        m
    }

    fn run_with(&self, m: &ResignMirror, cancel: &AtomicBool) -> Result<ResignReport, Error> {
        let log = |l: &str| eprintln!("resign: {l}");
        let store = Store::new(self.data.path().join("data/store"));
        let ctx = ResignCtx {
            http: &self.http,
            state_dir: &self.data.path().join("data/mirrors/rs"),
            store: &store,
            cancel,
            log: &log,
        };
        sync(m, &ctx, &self.target)
    }

    fn run(&self, m: &ResignMirror) -> ResignReport {
        self.run_with(m, &AtomicBool::new(false)).unwrap()
    }

    /// dist/component name=version arch, sorted.
    fn contents(&self) -> Vec<String> {
        let repo = self.target.0.lock().unwrap();
        let mut v: Vec<String> = repo
            .list("rs", None, None, &Filter::all())
            .unwrap()
            .into_iter()
            .map(|r| {
                format!(
                    "{}/{} {}={} {}",
                    r.dist, r.component, r.package.name, r.package.version, r.package.arch
                )
            })
            .collect();
        v.sort();
        v
    }

    fn sync_labels(&self) -> Vec<String> {
        let repo = self.target.0.lock().unwrap();
        repo.snapshots("rs")
            .unwrap()
            .into_iter()
            .map(|s| s.label)
            .filter(|l| l.starts_with("sync-"))
            .collect()
    }

    fn pool_requests(&self) -> usize {
        self.srv.requests_for("pool/").len()
    }
}

#[test]
fn follows_upstream_signed_with_our_key() {
    let Some(env) = Env::new() else { return };
    let up = &env.up;
    let a = up.deb("alpha", "1.0", "amd64", "a1");
    let b = up.deb("beta", "1.0", "all", "b1");
    let c = up.deb("gamma", "2.0", "amd64", "c1");
    up.publish(
        "one",
        &["amd64"],
        &[("main", vec![a.clone(), b.clone()]), ("extra", vec![c])],
    );
    up.publish("two", &["amd64"], &[("main", vec![a])]);

    let m = env.mirror(&["one", "two"]);
    let r = env.run(&m);
    assert!(r.ok(), "{r:?}");
    assert_eq!((r.packages, r.downloaded), (3, 3));
    let p = r.publish.as_ref().unwrap();
    assert!(
        p.created && p.revision.is_some() && p.snapshot.is_some(),
        "{p:?}"
    );
    assert_eq!(
        env.contents(),
        [
            "one/extra gamma=2.0 amd64",
            "one/main alpha=1.0 amd64",
            "one/main beta=1.0 all",
            "two/main alpha=1.0 amd64",
        ]
    );
    {
        let repo = env.target.0.lock().unwrap();
        let l = repo.layout("rs").unwrap();
        assert_eq!(l.dists, ["one", "two"]);
        assert_eq!(l.components, ["main", "extra"]);
        assert_eq!(l.only.get("extra").unwrap(), &["one".to_string()]);
        assert_eq!((l.origin.as_str(), l.label.as_str()), ("Test", "test"));
        assert!(l.sign);
    }
    // Signed with our key, not the upstream's.
    let ir = std::fs::read(env.path().join("dists/one/InRelease")).unwrap();
    verify_inrelease(&Keyring::from_bytes(&env.ours.public).unwrap(), &ir).unwrap();
    assert!(verify_inrelease(&Keyring::from_bytes(&env.up.key.public).unwrap(), &ir).is_err());
    assert!(
        std::fs::read_to_string(env.path().join("dists/one/main/binary-amd64/Packages"))
            .unwrap()
            .contains("Package: beta\n")
    );

    // A rerun reads the indices only and changes nothing.
    let before = env.pool_requests();
    let r = env.run(&m);
    assert!(r.ok(), "{r:?}");
    assert_eq!(r.downloaded, 0);
    assert_eq!(env.pool_requests(), before, "no pool file fetched again");
    let p = r.publish.unwrap();
    assert!(p.revision.is_none() && p.snapshot.is_none(), "{p:?}");
    assert_eq!(env.sync_labels().len(), 1);
}

#[test]
fn changes_are_snapshots_and_a_sync_undoes_a_rollback() {
    let Some(env) = Env::new() else { return };
    let up = &env.up;
    let a1 = up.deb("alpha", "1.0", "amd64", "a1");
    let b1 = up.deb("beta", "1.0", "amd64", "b1");
    up.publish("one", &["amd64"], &[("main", vec![a1.clone(), b1])]);
    let mut m = env.mirror(&["one"]);
    m.keep_snapshots = 2;
    env.run(&m);
    let first = env.sync_labels()[0].clone();

    let a2 = up.deb("alpha", "2.0", "amd64", "a2");
    up.publish("one", &["amd64"], &[("main", vec![a1.clone(), a2.clone()])]);
    let r = env.run(&m);
    let p = r.publish.unwrap();
    assert_eq!((p.added, p.removed), (1, 1), "{p:?}");
    assert_eq!(
        env.contents(),
        ["one/main alpha=1.0 amd64", "one/main alpha=2.0 amd64"]
    );
    assert_eq!(env.sync_labels().len(), 2);

    // Rolled back by hand: the next sync says so and restores upstream.
    {
        let mut repo = env.target.0.lock().unwrap();
        let plan = repo
            .plan_rollback("rs", &RevRef::Label(first.clone()))
            .unwrap();
        repo.apply(&plan, "test").unwrap();
    }
    assert!(
        env.contents()
            .contains(&"one/main beta=1.0 amd64".to_string())
    );
    let r = env.run(&m);
    let p = r.publish.unwrap();
    assert!(
        p.warnings.iter().any(|w| w.contains("rolled back")),
        "{p:?}"
    );
    assert_eq!(
        env.contents(),
        ["one/main alpha=1.0 amd64", "one/main alpha=2.0 amd64"]
    );
    // Only the newest two sync snapshots stay.
    let labels = env.sync_labels();
    assert_eq!(labels.len(), 2, "{labels:?}");
    assert!(!labels.contains(&first));
    assert!(!p.dropped_snapshots.is_empty());
}

#[test]
fn filters_and_lying_indices() {
    let Some(env) = Env::new() else { return };
    let up = &env.up;
    let a = up.deb("alpha", "1.0", "amd64", "a");
    let d = up.deb("alpha-dbg", "1.0", "amd64", "d");
    let b = up.deb("beta", "1.0", "amd64", "b");
    up.publish("one", &["amd64"], &[("main", vec![a.clone(), d, b])]);
    let mut m = env.mirror(&["one"]);
    m.include = Some(Filter::new(&["alpha*"], None, None).unwrap());
    m.exclude = Some(Filter::new(&["*-dbg"], None, None).unwrap());
    let r = env.run(&m);
    assert!(r.ok(), "{r:?}");
    assert_eq!((r.packages, r.filtered, r.downloaded), (1, 2, 1));
    assert_eq!(env.contents(), ["one/main alpha=1.0 amd64"]);
    // `glob=version` selects one version.
    let mut exact = env.mirror(&["one"]);
    exact.include = Some(Filter::new(&["beta=2.0"], None, None).unwrap());
    let r = env.run(&exact);
    assert!(
        r.publish.unwrap().removed == 1 && env.contents().is_empty(),
        "{:?}",
        env.contents()
    );
    env.run(&m);

    // An index stanza that names another package than its file is: the
    // dist fails and keeps what it had.
    let x = up.deb("alpine", "9.0", "amd64", "x");
    up.publish("one", &["amd64"], &[("main", vec![a, x])]);
    let idx = up.dist_dir("one").join("main/binary-amd64/Packages");
    let text = std::fs::read_to_string(&idx)
        .unwrap()
        .replace("Package: alpine\n", "Package: alpha-tools\n");
    up.write_index("one", "main/binary-amd64/Packages", &text);
    up.resign("one", &["amd64"], &["main"], &Default::default());
    let r = env.run(&m);
    assert!(!r.ok());
    let err = r.dists[0].error.as_deref().unwrap();
    assert!(err.contains("the index says alpha-tools"), "{err}");
    assert_eq!(env.contents(), ["one/main alpha=1.0 amd64"]);
}

#[test]
fn failed_dists_keep_contents_missing_files_can_be_ignored() {
    let Some(env) = Env::new() else { return };
    let up = &env.up;
    let a = up.deb("alpha", "1.0", "amd64", "a");
    let b = up.deb("beta", "1.0", "amd64", "b");
    up.publish("one", &["amd64"], &[("main", vec![a.clone()])]);
    up.publish("two", &["amd64"], &[("main", vec![a.clone()])]);
    let mut m = env.mirror(&["one", "two"]);
    env.run(&m);

    up.publish("one", &["amd64"], &[("main", vec![a.clone(), b.clone()])]);
    let c = up.deb("gamma", "1.0", "amd64", "c");
    up.publish("two", &["amd64"], &[("main", vec![a.clone(), c.clone()])]);
    let gone = up.pool_of("main", &c);
    env.srv.faults().missing.insert(gone.clone());
    let r = env.run(&m);
    assert!(r.dists[0].ok && !r.dists[1].ok, "{r:?}");
    assert_eq!(
        env.contents(),
        [
            "one/main alpha=1.0 amd64",
            "one/main beta=1.0 amd64",
            "two/main alpha=1.0 amd64",
        ]
    );

    m.source.ignore_missing.insert(gone.clone());
    let r = env.run(&m);
    assert!(r.ok(), "{r:?}");
    assert_eq!(r.ignored_missing, [gone]);
    assert_eq!(env.contents().len(), 3);

    // A dist upstream drops entirely (Release gone) also keeps its contents.
    std::fs::remove_dir_all(up.dist_dir("two")).unwrap();
    let r = env.run(&m);
    assert!(!r.dists[1].ok);
    assert!(
        env.contents()
            .contains(&"two/main alpha=1.0 amd64".to_string())
    );
}

#[test]
fn brake_on_mass_removal_and_interruption() {
    let Some(env) = Env::new() else { return };
    let up = &env.up;
    let debs: Vec<PathBuf> = (0..4)
        .map(|i| up.deb(&format!("p{i}"), "1.0", "amd64", "x"))
        .collect();
    up.publish("one", &["amd64"], &[("main", debs.clone())]);
    let mut m = env.mirror(&["one"]);
    m.max_delete = 2;
    env.run(&m);

    up.publish("one", &["amd64"], &[("main", debs[..1].to_vec())]);
    let e = env.run_with(&m, &AtomicBool::new(false)).unwrap_err();
    assert!(e.to_string().contains("more than max_delete"), "{e}");
    assert_eq!(env.contents().len(), 4);
    m.force = true;
    env.run(&m);
    assert_eq!(env.contents(), ["one/main p0=1.0 amd64"]);

    // Interrupted before anything is applied: nothing changes.
    let n = up.deb("new", "1.0", "amd64", "n");
    up.publish("one", &["amd64"], &[("main", vec![debs[0].clone(), n])]);
    let e = env.run_with(&m, &AtomicBool::new(true)).unwrap_err();
    assert!(matches!(e, Error::Interrupted), "{e}");
    assert_eq!(env.contents(), ["one/main p0=1.0 amd64"]);
}

#[test]
fn pool_clash_between_dists_fails_the_later_one() {
    let Some(env) = Env::new() else { return };
    let up = &env.up;
    let a = up.deb("alpha", "1.0", "amd64", "first");
    let other = up.deb("alpha", "1.0", "amd64", "second");
    up.publish("one", &["amd64"], &[("main", vec![a])]);
    // `two` lives in its own upstream pool, so the upstream itself does not
    // clash; in our publish both would be the same pool file.
    up.publish("two", &["amd64"], &[("main", vec![])]);
    let p = up
        .pool_of("main", &other)
        .replace("pool/main/", "pool/two/");
    let full = up.root.join(&p);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::copy(&other, &full).unwrap();
    let mut stanza = String::from_utf8(
        std::process::Command::new("dpkg-deb")
            .args(["-f"])
            .arg(&other)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let h = aptpod_core::hash::hash_file(&other).unwrap();
    stanza.push_str(&format!(
        "Filename: {p}\nSize: {}\nSHA256: {}\n\n",
        h.size, h.sha256
    ));
    up.write_index("two", "main/binary-amd64/Packages", &stanza);
    up.resign("two", &["amd64"], &["main"], &Default::default());
    let r = env.run(&env.mirror(&["one", "two"]));
    assert!(r.dists[0].ok && !r.dists[1].ok, "{r:?}");
    assert!(
        r.dists[1]
            .error
            .as_deref()
            .unwrap()
            .contains("other content")
    );
    // `two` was never published: the layout only has `one`.
    assert_eq!(env.contents(), ["one/main alpha=1.0 amd64"]);
}
