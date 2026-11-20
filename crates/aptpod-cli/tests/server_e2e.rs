// SPDX-License-Identifier: GPL-3.0-only

//! The real server and the real CLI: login, publish create, upload + add
//! with preview/apply, listing, roles, stale plans, limits, shutdown and
//! restart. Needs dpkg-deb and the `aptpod-server` binary next to `aptpod`
//! (`cargo test --workspace` builds both).

mod common;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::*;
use serde_json::{Value, json};

#[test]
fn server_and_cli() {
    let Some((dir, server_bin, cfg)) = prepare() else {
        return;
    };
    let d = dir.path();
    let roots = d.join("trees");

    let srv = Server::start(&server_bin, &cfg);
    // One server per data directory.
    let second = Command::new(&server_bin)
        .arg("-c")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("another aptpod-server"));

    let cli = |name: &str| Cli {
        creds: d.join(format!("{name}.toml")),
        url: srv.url.clone(),
    };
    let (admin, ci, viewer) = (cli("admin"), cli("ci"), cli("viewer"));

    // Login: wrong password, then right; the credentials file is private.
    let out = admin.run(&["login", "--user", "admin", "--password-stdin"], "nope\n");
    assert_eq!(out.status.code(), Some(7), "{}", text(&out));
    for (c, u) in [(&admin, "admin"), (&ci, "ci"), (&viewer, "viewer")] {
        let out = c.run(
            &["login", "--user", u, "--password-stdin"],
            &format!("{u}-password\n"),
        );
        assert!(out.status.success(), "{}", text(&out));
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&c.creds).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
    }
    assert!(ci.ok(&["whoami"]).starts_with("ci (publisher)"));
    // The saved token goes only to the server it came from.
    assert!(
        ci.ok(&["--server", &format!("{}/", srv.url), "whoami"])
            .starts_with("ci ")
    );
    let other = srv.url.replace("127.0.0.1", "localhost");
    assert!(
        ci.fails(&["--server", &other, "whoami"], 7)
            .contains("aptpod login")
    );

    // Publishes: admin only, under a publish root.
    let tree = roots.join("t");
    let create = |c: &Cli, path: &Path, code: i32| {
        let p = path.display().to_string();
        let args = [
            "publish",
            "create",
            "t",
            "--path",
            &p,
            "--dist",
            "jammy,noble",
            "--component",
            "main",
            "--arch",
            "amd64",
            "--no-sign",
            "--yes",
        ];
        if code == 0 {
            c.ok(&args)
        } else {
            c.fails(&args, code)
        }
    };
    assert!(create(&ci, &tree, 7).contains("admin role"));
    assert!(
        create(&admin, &d.join("elsewhere"), 1).contains("not under a configured publish root")
    );
    let out = create(&admin, &tree, 0);
    assert!(out.contains("t is now at revision #"), "{out}");
    assert!(admin.ok(&["publish", "ls"]).starts_with("t\t"));

    // add: preview + apply, dists detected from the version suffix.
    let debs = d.join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let hj = build_deb(&debs, "hello", "1.0-1~jammy");
    let hn = build_deb(&debs, "hello", "1.0-1~noble");
    let tool = build_deb(&debs, "tool", "2.0");
    let (hj, hn, tool) = (
        hj.display().to_string(),
        hn.display().to_string(),
        tool.display().to_string(),
    );

    // Readers cannot change anything; a dry run changes nothing; without a
    // terminal, applying needs --yes.
    assert!(
        viewer
            .fails(&["add", "t", "main", &hj, "-y"], 7)
            .contains("publisher role")
    );
    let out = ci.ok(&["add", "t", "main", &hj, &hn, "-n"]);
    assert!(
        out.contains("hello 1.0-1~jammy") && out.contains("jammy/main"),
        "{out}"
    );
    assert!(ci.fails(&["add", "t", "main", &hj], 2).contains("--yes"));
    assert_eq!(ci.ok(&["ls", "t"]), "");

    let out = ci.ok(&["add", "t", "main", &hj, &hn, "-y"]);
    assert!(
        out.contains("published t jammy") && out.contains("published t noble"),
        "{out}"
    );
    let packages =
        std::fs::read_to_string(tree.join("dists/noble/main/binary-amd64/Packages")).unwrap();
    assert!(packages.contains("Version: 1.0-1~noble"));
    assert!(
        tree.join("pool/main/h/hello/hello_1.0-1~noble_amd64.deb")
            .is_file()
    );

    // No suffix: the dist must be given; the warning names the client file.
    let err = ci.fails(&["add", "t", "main", &tool, "-y"], 1);
    assert!(err.contains("tool_2.0_amd64.deb"), "{err}");
    assert!(!err.contains("/uploads/"), "{err}");
    ci.ok(&["add", "t", "main", &tool, "--dist", "noble", "-y"]);

    let rows: Vec<Value> = serde_json::from_str(&viewer.ok(&["ls", "t", "--json"])).unwrap();
    assert_eq!(rows.len(), 3);
    let table = viewer.ok(&["ls", "t", "hello", "-d", "noble"]);
    assert_eq!(table.trim(), "noble  main  hello  1.0-1~noble  amd64");
    assert!(
        viewer
            .ok(&["publish", "show", "t"])
            .contains("noble/main: 2 package(s)")
    );

    // Same request again: nothing to do.
    assert!(
        ci.ok(&["add", "t", "main", &hj, "-y"])
            .contains("nothing to do")
    );

    // Same name and version, other content: conflict, unless --replace.
    let other_dir = d.join("other");
    std::fs::create_dir_all(&other_dir).unwrap();
    let hn2 = build_deb_marked(&other_dir, "hello", "1.0-1~noble", "rebuilt")
        .display()
        .to_string();
    ci.fails(&["add", "t", "main", &hn2, "-y"], 4);
    let out = ci.ok(&["add", "t", "main", &hn2, "--replace", "-y"]);
    assert!(out.contains("published t noble"), "{out}");

    // Stale: a preview's bases no longer match after another change.
    let token = ci.token();
    let up = |p: &str| {
        let (st, v) = upload(&srv.url, &token, std::fs::read(p).unwrap());
        assert_eq!(st, 200, "{v}");
        v["id"].as_str().unwrap().to_string()
    };
    let h2 = build_deb(&debs, "hello", "1.1-1~jammy")
        .display()
        .to_string();
    let h3 = build_deb(&debs, "hello", "1.2-1~jammy")
        .display()
        .to_string();
    let body = |id: &str| json!({ "component": "main", "uploads": [id] });
    let (id2, id3) = (up(&h2), up(&h3));
    let (st, preview) = api(
        &srv.url,
        &token,
        "POST",
        "/api/v1/publishes/t/add",
        Some(body(&id2)),
    );
    assert_eq!(st, 200, "{preview}");
    let mut b3 = body(&id3);
    b3["apply"] = json!(true);
    assert_eq!(
        api(
            &srv.url,
            &token,
            "POST",
            "/api/v1/publishes/t/add",
            Some(b3)
        )
        .0,
        200
    );
    let mut b2 = body(&id2);
    b2["apply"] = json!(true);
    b2["expect"] = preview["plan"]["bases"].clone();
    let (st, err) = api(
        &srv.url,
        &token,
        "POST",
        "/api/v1/publishes/t/add",
        Some(b2.clone()),
    );
    assert_eq!((st, err["kind"].as_str()), (409, Some("stale")), "{err}");
    // Applying the fresh preview works; the upload is gone afterwards.
    let (_, fresh) = api(
        &srv.url,
        &token,
        "POST",
        "/api/v1/publishes/t/add",
        Some(body(&id2)),
    );
    b2["expect"] = fresh["plan"]["bases"].clone();
    let (st, v) = api(
        &srv.url,
        &token,
        "POST",
        "/api/v1/publishes/t/add",
        Some(b2.clone()),
    );
    assert_eq!(st, 200, "{v}");
    let (st, v) = api(
        &srv.url,
        &token,
        "POST",
        "/api/v1/publishes/t/add",
        Some(b2),
    );
    assert_eq!((st, v["kind"].as_str()), (404, Some("not_found")), "{v}");

    // A misspelt field is an error, not a silent preview.
    let id4 = up(&h3);
    let (st, _) = api(
        &srv.url,
        &token,
        "POST",
        "/api/v1/publishes/t/add",
        Some(json!({ "component": "main", "uploads": [id4], "aply": true })),
    );
    assert!((400..500).contains(&st), "{st}");

    // Uploads: size limit, garbage, bad ids, no token.
    assert_eq!(upload(&srv.url, &token, vec![0u8; 1_000_001]).0, 413);
    let (_, junk) = upload(&srv.url, &token, b"not a deb".to_vec());
    let (st, v) = api(
        &srv.url,
        &token,
        "POST",
        "/api/v1/publishes/t/add",
        Some(json!({ "component": "main", "uploads": [junk["id"]], "dists": ["noble"] })),
    );
    assert_eq!((st, v["kind"].as_str()), (400, Some("invalid")), "{v}");
    let (st, _) = api(
        &srv.url,
        &token,
        "POST",
        "/api/v1/publishes/t/add",
        Some(json!({ "component": "main", "uploads": ["../../etc/passwd"] })),
    );
    assert_eq!(st, 400);
    assert_eq!(
        api(&srv.url, "aptpod_bogus", "GET", "/api/v1/whoami", None).0,
        401
    );
    assert_eq!(api(&srv.url, &token, "GET", "/api/v1/nope", None).0, 404);

    let jobs = viewer.ok(&["jobs"]);
    assert!(
        jobs.contains("done add publish:t") && jobs.contains("done create publish:t"),
        "{jobs}"
    );

    // Graceful stop, restart on the same data; logout revokes the token.
    assert!(srv.stop().success());
    let srv = Server::start(&server_bin, &cfg);
    let viewer = Cli {
        url: srv.url.clone(),
        ..viewer
    };
    // The new port is another server URL, so the saved token is not sent;
    // passed explicitly, it still works (tokens live in the database).
    let old = viewer.token();
    viewer.fails(&["ls", "t"], 7);
    let rows: Vec<Value> =
        serde_json::from_str(&viewer.ok(&["--token", &old, "ls", "t", "--json"])).unwrap();
    assert_eq!(rows.len(), 5);
    viewer.ok(&["--token", &old, "logout"]);
    assert!(!viewer.creds.exists());
    assert_eq!(api(&srv.url, &old, "GET", "/api/v1/whoami", None).0, 401);
    assert!(srv.stop().success());
}

/// rm, prune, promote (across components and publishes), find, snapshots,
/// history, diff, rollback, layout get/set/edit, delete, gc and repair.
#[test]
fn operations() {
    let Some((dir, bin, cfg)) = prepare() else {
        return;
    };
    let d = dir.path();
    let roots = d.join("trees");
    let srv = Server::start(&bin, &cfg);
    let cli = |name: &str| Cli {
        creds: d.join(format!("{name}.toml")),
        url: srv.url.clone(),
    };
    let (admin, ci, viewer) = (cli("admin"), cli("ci"), cli("viewer"));
    for (c, u) in [(&admin, "admin"), (&ci, "ci"), (&viewer, "viewer")] {
        login(c, u);
    }
    let (t, u) = (roots.join("t"), roots.join("u"));
    let (tp, up) = (t.display().to_string(), u.display().to_string());
    admin.ok(&[
        "publish",
        "create",
        "t",
        "--path",
        &tp,
        "--dist",
        "jammy,noble",
        "--component",
        "main,extra",
        "--arch",
        "amd64",
        "--label",
        "lbl1",
        "--no-sign",
        "-y",
    ]);
    admin.ok(&[
        "publish",
        "create",
        "u",
        "--path",
        &up,
        "--dist",
        "noble",
        "--component",
        "main",
        "--arch",
        "amd64",
        "--no-sign",
        "-y",
    ]);

    let debs = d.join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let files: Vec<String> = [
        ("aa", "1.0"),
        ("aa", "1.1"),
        ("aa", "1.2"),
        ("bb", "1.0"),
        ("aa-dbg", "1.0"),
    ]
    .iter()
    .map(|(n, v)| build_deb(&debs, n, v).display().to_string())
    .collect();
    let mut args = vec!["add", "t", "main", "--dist", "noble", "-y"];
    args.extend(files.iter().map(String::as_str));
    ci.ok(&args);

    let names = |c: &Cli, args: &[&str]| -> Vec<String> {
        let rows: Vec<Value> = serde_json::from_str(&c.ok(args)).unwrap();
        rows.iter()
            .map(|r| {
                format!(
                    "{}/{} {}={}",
                    r["dist"].as_str().unwrap(),
                    r["component"].as_str().unwrap(),
                    r["name"].as_str().unwrap(),
                    r["version"].as_str().unwrap()
                )
            })
            .collect()
    };

    assert!(
        ci.ok(&[
            "snapshot",
            "create",
            "t",
            "before-rm",
            "-m",
            "five packages"
        ])
        .starts_with("before-rm -> #")
    );
    // rm needs a selection; a glob removes only what it matches.
    assert!(ci.fails(&["rm", "t", "main", "-y"], 1).contains("--all"));
    assert!(
        viewer
            .fails(&["rm", "t", "main", "aa-*", "-y"], 7)
            .contains("publisher")
    );
    let out = ci.ok(&["rm", "t", "main", "aa-*", "-y"]);
    assert!(out.contains("aa-dbg"), "{out}");
    ci.ok(&["prune", "t", "--keep", "2", "-y"]);
    assert_eq!(
        names(&viewer, &["ls", "t", "--json"]),
        [
            "noble/main aa=1.2",
            "noble/main aa=1.1",
            "noble/main bb=1.0"
        ]
    );

    // promote: newest only, to another component; to another publish.
    ci.ok(&[
        "promote",
        "t",
        "main",
        "aa",
        "--to-component",
        "extra",
        "-y",
    ]);
    ci.ok(&["promote", "t", "main", "bb", "--to-publish", "u", "-y"]);
    assert!(
        ci.fails(&["promote", "t", "main", "-y"], 1)
            .contains("--all")
    );
    assert_eq!(
        names(&viewer, &["ls", "t", "-c", "extra", "--json"]),
        ["noble/extra aa=1.2"]
    );
    assert_eq!(
        names(&viewer, &["ls", "u", "--json"]),
        ["noble/main bb=1.0"]
    );
    let found = viewer.ok(&["find", "bb", "aa", "--newest"]);
    assert_eq!(found.lines().count(), 4, "{found}");
    assert!(found.lines().any(|l| l.starts_with("u ")), "{found}");

    // history, diff and rollback to the label.
    let hist = viewer.ok(&["history", "t"]);
    assert!(
        hist.contains("(HEAD)") && hist.contains("[before-rm]"),
        "{hist}"
    );
    let diff = viewer.ok(&["diff", "t", "before-rm"]);
    assert!(diff.contains("- noble/main aa-dbg 1.0 amd64"), "{diff}");
    assert!(diff.contains("- noble/main aa 1.0 amd64"), "{diff}");
    assert!(diff.contains("+ noble/extra aa 1.2 amd64"), "{diff}");
    ci.ok(&["rollback", "t", "before-rm", "-y"]);
    assert_eq!(names(&viewer, &["ls", "t", "--json"]).len(), 5);
    assert!(t.join("pool/main/a/aa-dbg/aa-dbg_1.0_amd64.deb").is_file());
    assert_eq!(viewer.ok(&["diff", "t", "before-rm"]), "");

    // Snapshot labels.
    assert!(
        viewer
            .ok(&["snapshot", "ls", "t"])
            .starts_with("before-rm\t#")
    );
    let dry = ci.ok(&["snapshot", "prune", "t", "--older-than", "0", "-n"]);
    assert!(
        dry.contains("would drop") && dry.contains("before-rm"),
        "{dry}"
    );
    ci.ok(&["snapshot", "drop", "t", "before-rm"]);
    assert_eq!(viewer.ok(&["snapshot", "ls", "t"]), "");
    ci.fails(&["snapshot", "drop", "t", "before-rm"], 1);

    // Layout as TOML: set from stdin (dropping a dist with packages is
    // blocked without --force), edit through $EDITOR.
    let layout = viewer.ok(&["publish", "get", "t"]);
    let with_dists = |text: &str, dists: &[&str]| {
        let mut t: toml::Table = toml::from_str(text).unwrap();
        t.insert(
            "dists".into(),
            toml::Value::Array(dists.iter().map(|d| toml::Value::from(*d)).collect()),
        );
        toml::to_string(&t).unwrap()
    };
    let parsed: toml::Table = toml::from_str(&layout).unwrap();
    assert_eq!(parsed["dists"].as_array().unwrap().len(), 2, "{layout}");
    // Fields come in the server's order, not sorted.
    assert!(layout.starts_with("path = "), "{layout}");
    let no_noble = with_dists(&layout, &["jammy"]);
    assert_eq!(
        ci.run(&["publish", "set", "t", "-", "-y"], &no_noble)
            .status
            .code(),
        Some(7)
    );
    let out = admin.run(&["publish", "set", "t", "-", "-y"], &no_noble);
    assert_eq!(out.status.code(), Some(5), "{}", text(&out));
    let no_jammy = with_dists(&layout, &["noble"]);
    let out = admin.run(&["publish", "set", "t", "-", "-y"], &no_jammy);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!t.join("dists/jammy").exists());
    let out = admin.run_env(
        &["publish", "edit", "t", "-y"],
        "",
        &[("VISUAL", "sed -i s/lbl1/lbl2/")],
    );
    assert!(out.status.success(), "{}", text(&out));
    let release = std::fs::read_to_string(t.join("dists/noble/Release")).unwrap();
    assert!(release.contains("Label: lbl2"), "{release}");
    let out = admin.run_env(&["publish", "edit", "t"], "", &[("VISUAL", "true")]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("no changes"),
        "{}",
        text(&out)
    );

    // Delete: blocked while packages are left.
    assert_eq!(
        admin
            .run(&["publish", "delete", "u", "-y"], "")
            .status
            .code(),
        Some(5)
    );
    admin.ok(&["publish", "delete", "u", "--force", "-y"]);
    assert!(!u.join("dists/noble").exists() && !u.join("pool").exists());
    assert!(!admin.ok(&["publish", "ls"]).contains("u\t"));

    // Maintenance (admin only).
    ci.fails(&["gc"], 7);
    assert!(admin.ok(&["gc"]).contains("revision(s) expired"));
    assert!(admin.ok(&["repair"]).contains("up to date"));
    let jobs = viewer.ok(&["jobs", "-n", "100"]);
    for kind in [
        "rm", "prune", "promote", "rollback", "snapshot", "layout", "delete", "gc", "repair",
    ] {
        assert!(jobs.contains(&format!("done {kind} ")), "{kind}: {jobs}");
    }
    assert!(srv.stop().success());
}

/// Tokens, users and passwords through the API; add --checksums; config
/// export/import; the job event stream and stopping with a follower.
#[test]
fn accounts_config_events() {
    let Some((dir, bin, cfg)) = prepare() else {
        return;
    };
    let d = dir.path();
    let roots = d.join("trees");
    let srv = Server::start(&bin, &cfg);
    let cli = |name: &str| Cli {
        creds: d.join(format!("{name}.toml")),
        url: srv.url.clone(),
    };
    let (admin, ci) = (cli("admin"), cli("ci"));
    login(&admin, "admin");
    login(&ci, "ci");

    // Follow events from the start.
    let mut follower = Command::new(env!("CARGO_BIN_EXE_aptpod"))
        .args(["jobs", "--follow", "-n", "0"])
        .env("APTPOD_CREDENTIALS", &admin.creds)
        .env("APTPOD_SERVER", &srv.url)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let (tx, events) = mpsc::channel::<String>();
    let out = follower.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    std::thread::sleep(Duration::from_millis(300));

    // Tokens: a CI token works on its own and can be revoked.
    let t = ci.ok(&["token", "create", "jenkins"]).trim().to_string();
    assert!(t.starts_with("aptpod_"));
    assert!(ci.ok(&["--token", &t, "whoami"]).starts_with("ci "));
    let list = ci.ok(&["token", "ls"]);
    let id = list
        .lines()
        .find(|l| l.contains("\tjenkins\t"))
        .and_then(|l| l.split('\t').next())
        .unwrap()
        .to_string();
    ci.ok(&["token", "revoke", &id]);
    ci.fails(&["--token", &t, "whoami"], 7);

    // Users (admin); the last admin stays.
    ci.fails(&["user", "ls"], 7);
    let out = admin.run(
        &["user", "add", "bot", "--role", "reader", "--password-stdin"],
        "bot-password\n",
    );
    assert!(out.status.success(), "{}", text(&out));
    let out = admin.run(
        &["user", "add", "bot", "--password-stdin"],
        "bot-password\n",
    );
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    admin.ok(&["user", "role", "bot", "publisher"]);
    assert!(admin.ok(&["user", "ls"]).contains("bot\tpublisher"));
    assert!(
        admin
            .fails(&["user", "role", "admin", "reader"], 4)
            .contains("last admin")
    );
    assert!(
        admin
            .fails(&["user", "del", "admin"], 4)
            .contains("last admin")
    );
    let bot = cli("bot");
    login(&bot, "bot");
    let out = admin.run(
        &["user", "passwd", "bot", "--password-stdin"],
        "bot-password-2\n",
    );
    assert!(out.status.success(), "{}", text(&out));
    let out = bot.run(
        &["login", "--user", "bot", "--password-stdin"],
        "bot-password\n",
    );
    assert_eq!(out.status.code(), Some(7));
    // Own password: the current one is checked.
    let out = bot.run(
        &["passwd", "--password-stdin"],
        "wrong-password\nbot-password-3\n",
    );
    assert_eq!(out.status.code(), Some(7), "{}", text(&out));
    let out = bot.run(
        &["passwd", "--password-stdin"],
        "bot-password-2\nbot-password-3\n",
    );
    assert!(out.status.success(), "{}", text(&out));
    admin.ok(&["user", "del", "bot"]);
    bot.fails(&["whoami"], 7);

    // Config import: create two publishes from TOML.
    let (a, b) = (roots.join("a"), roots.join("b"));
    let config = format!(
        "[publishes.a]\npath = {:?}\ndists = [\"noble\"]\ncomponents = [\"main\"]\narchitectures = [\"amd64\"]\nsign = false\n\n\
         [publishes.b]\npath = {:?}\ndists = [\"jammy\", \"noble\"]\ncomponents = [\"main\"]\narchitectures = [\"amd64\"]\nsign = false\n",
        a.display().to_string(),
        b.display().to_string()
    );
    assert_eq!(
        ci.run(&["config", "import", "-", "-y"], &config)
            .status
            .code(),
        Some(7)
    );
    let overlap = config.replace(
        &b.display().to_string(),
        &a.join("sub").display().to_string(),
    );
    let out = admin.run(&["config", "import", "-", "-y"], &overlap);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("overlapping"),
        "{}",
        text(&out)
    );
    let out = admin.run(&["config", "import", "-", "-n"], &config);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("[a]"),
        "{}",
        text(&out)
    );
    assert!(admin.ok(&["publish", "ls"]).is_empty());
    let out = admin.run(&["config", "import", "-", "-y"], &config);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(admin.ok(&["publish", "ls"]).lines().count(), 2);
    let again = admin.run(&["config", "import", "-", "-y"], &config);
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("nothing to do"),
        "{}",
        text(&again)
    );

    // Export round-trips; an edit of one publish only touches that one.
    let exported = admin.ok(&["config", "export"]);
    assert!(
        exported.contains("[publishes.a]") && exported.contains("[publishes.b]"),
        "{exported}"
    );
    let mut cfg: toml::Table = toml::from_str(&exported).unwrap();
    cfg["publishes"]["b"]
        .as_table_mut()
        .unwrap()
        .insert("dists".into(), toml::Value::Array(vec!["noble".into()]));
    let edited = toml::to_string(&cfg).unwrap();
    let out = admin.run(&["config", "import", "-", "-y"], &edited);
    assert!(out.status.success(), "{}", text(&out));
    let hist_a = admin.ok(&["history", "a"]);
    assert_eq!(hist_a.lines().count(), 1, "{hist_a}");
    assert_eq!(admin.ok(&["history", "b"]).lines().count(), 2);

    // add --checksums.
    let debs = d.join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let deb = build_deb(&debs, "pkg", "1.0");
    let sha = {
        let out = Command::new("sha256sum").arg(&deb).output().unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    let deb = deb.display().to_string();
    let sums = d.join("SHA256SUMS");
    std::fs::write(&sums, format!("{}  other.deb\n", "0".repeat(64))).unwrap();
    let s = sums.display().to_string();
    assert!(
        ci.fails(
            &[
                "add",
                "a",
                "main",
                &deb,
                "-d",
                "noble",
                "--checksums",
                &s,
                "-y"
            ],
            2
        )
        .contains("not in the checksums")
    );
    std::fs::write(&sums, format!("{}  pkg_1.0_amd64.deb\n", "0".repeat(64))).unwrap();
    assert!(
        ci.fails(
            &[
                "add",
                "a",
                "main",
                &deb,
                "-d",
                "noble",
                "--checksums",
                &s,
                "-y"
            ],
            4
        )
        .contains("does not match")
    );
    std::fs::write(&sums, format!("{sha}  pkg_1.0_amd64.deb\n")).unwrap();
    ci.ok(&[
        "add",
        "a",
        "main",
        &deb,
        "-d",
        "noble",
        "--checksums",
        &s,
        "-y",
    ]);

    // The follower saw the jobs as they ran.
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !seen.iter().any(|l: &String| l.contains("done add"))
        && std::time::Instant::now() < deadline
    {
        if let Ok(l) = events.recv_timeout(Duration::from_millis(200)) {
            seen.push(l);
        }
    }
    let all = seen.join("\n");
    assert!(
        all.contains("running import") && all.contains("done import"),
        "{all}"
    );
    assert!(all.contains("done add publish:a ci"), "{all}");

    // Stopping does not wait for the follower, which then ends.
    assert!(srv.stop().success());
    let mut ended = false;
    for _ in 0..50 {
        if follower.try_wait().unwrap().is_some() {
            ended = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = follower.kill();
    assert!(ended, "the follower did not end with the server");
}

/// Release signing with the key from server.toml; a broken key leaves the
/// change saved but the tree behind (exit 6) until a restart with a working
/// key repairs it.
#[test]
fn signing() {
    let Some(key) = GpgKey::new() else {
        return;
    };
    let Some((dir, bin, cfg)) = prepare_extra(&key.signing_toml(&key.fpr)) else {
        return;
    };
    let d = dir.path();
    let roots = d.join("trees");
    let keyring = aptpod_core::verify::Keyring::from_bytes(&key.public).unwrap();
    let verify = |tree: &Path, dist: &str| -> String {
        let data = std::fs::read(tree.join(format!("dists/{dist}/InRelease"))).unwrap();
        aptpod_core::verify::verify_inrelease(&keyring, &data).expect("valid InRelease")
    };

    let srv = Server::start(&bin, &cfg);
    let admin = Cli {
        creds: d.join("admin.toml"),
        url: srv.url.clone(),
    };
    login(&admin, "admin");
    let s = roots.join("s");
    let sp = s.display().to_string();
    admin.ok(&[
        "publish",
        "create",
        "s",
        "--path",
        &sp,
        "--dist",
        "noble",
        "--component",
        "main",
        "--arch",
        "amd64",
        "-y",
    ]);
    let debs = d.join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let deb = build_deb(&debs, "signed-pkg", "1.0").display().to_string();
    admin.ok(&["add", "s", "main", &deb, "-d", "noble", "-y"]);
    let signed = verify(&s, "noble");
    assert!(signed.contains("main/binary-amd64/Packages"), "{signed}");
    assert!(s.join("dists/noble/Release.gpg").is_file());
    assert!(srv.stop().success());

    // A key the keyring does not have: committed, but the tree is behind.
    let good = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, good.replace(&key.fpr, &"0".repeat(40))).unwrap();
    let srv = Server::start(&bin, &cfg);
    let admin = Cli {
        url: srv.url.clone(),
        ..admin
    };
    let token = admin.token();
    let deb2 = build_deb(&debs, "signed-pkg", "1.1").display().to_string();
    let err = admin.fails(
        &[
            "--token", &token, "add", "s", "main", &deb2, "-d", "noble", "-y",
        ],
        6,
    );
    assert!(err.contains("repair"), "{err}");
    let rows: Vec<Value> =
        serde_json::from_str(&admin.ok(&["--token", &token, "ls", "s", "--json"])).unwrap();
    assert_eq!(rows.len(), 2, "the change is saved");
    // The old tree is still served intact: old Packages, valid signature.
    let old = std::fs::read_to_string(s.join("dists/noble/main/binary-amd64/Packages")).unwrap();
    assert!(!old.contains("Version: 1.1"), "{old}");
    assert_eq!(verify(&s, "noble"), signed);
    assert!(srv.stop().success());

    // Fixed key: startup repair brings the tree up to date, signed.
    std::fs::write(&cfg, &good).unwrap();
    let srv = Server::start(&bin, &cfg);
    let packages =
        std::fs::read_to_string(s.join("dists/noble/main/binary-amd64/Packages")).unwrap();
    assert!(packages.contains("Version: 1.1"), "{packages}");
    verify(&s, "noble");
    assert!(srv.stop().success());
}

/// Mirrors through the server: definitions and their rules, syncs as jobs
/// with logs, adoption without deleting, a failing upstream key, config
/// export/import, edit and delete.
#[test]
fn mirrors() {
    use aptpod_core::deb::read_deb;
    use aptpod_core::index::ReleaseMeta;
    use aptpod_core::publish::{ComponentSpec, DistSpec, PublishPackage, publish_dist};

    let Some(key) = aptpod_core::testutil::TestKey::new() else {
        return;
    };
    let up = tempfile::tempdir().unwrap();
    let archive = up.path().join("archive");
    let pkgs: Vec<PathBuf> = [
        ("up-hello", "1.0", "amd64"),
        ("up-tool", "2.0", "all"),
        ("up-arm", "1.0", "arm64"),
    ]
    .iter()
    .map(|(n, v, a)| aptpod_core::testutil::build_deb(up.path(), n, v, a, "xz", ""))
    .collect();
    let spec = DistSpec {
        name: "noble".into(),
        meta: ReleaseMeta::new("noble", "Up", "up"),
        architectures: vec!["amd64".into(), "arm64".into()],
        components: vec![ComponentSpec {
            name: "main".into(),
            packages: pkgs
                .iter()
                .map(|f| PublishPackage {
                    info: read_deb(f).unwrap(),
                    file: f.clone(),
                    mode: Default::default(),
                })
                .collect(),
        }],
    };
    publish_dist(&archive, &spec, Some(&key.signer())).unwrap();
    let url = static_server(archive.clone(), Duration::ZERO);
    let slow_url = static_server(archive.clone(), Duration::from_secs(3));

    let Some((dir, bin, cfg)) = prepare_extra("") else {
        return;
    };
    let d = dir.path();
    let (trees, mroot) = (d.join("trees"), d.join("mirrors"));
    let base = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!(
            "{base}mirror_roots = [{:?}, {:?}]\nmirror_workers = 2\n",
            mroot.display().to_string(),
            trees.display().to_string()
        ),
    )
    .unwrap();
    let srv = Server::start(&bin, &cfg);
    let cli = |name: &str| Cli {
        creds: d.join(format!("{name}.toml")),
        url: srv.url.clone(),
    };
    let (admin, ci) = (cli("admin"), cli("ci"));
    login(&admin, "admin");
    login(&ci, "ci");
    let keyfile = d.join("upstream.gpg");
    std::fs::write(&keyfile, &key.public).unwrap();
    let kf = keyfile.display().to_string();
    let dest = mroot.join("up");
    let dp = dest.display().to_string();
    let create = |c: &Cli, name: &str, path: &str, extra: &[&str], code: i32| {
        let mut args = vec![
            "mirror",
            "create",
            name,
            "--url",
            &url,
            "--path",
            path,
            "--dist",
            "noble",
            "--component",
            "main",
            "--arch",
            "amd64",
        ];
        args.extend_from_slice(extra);
        if code == 0 {
            c.ok(&args)
        } else {
            c.fails(&args, code)
        }
    };

    // Definitions: admin only, a key (or --insecure), under mirror_roots.
    assert!(create(&ci, "up", &dp, &["--keyring", &kf], 7).contains("admin"));
    assert!(create(&admin, "up", &dp, &[], 1).contains("keyring"));
    let outside = d.join("elsewhere").display().to_string();
    assert!(
        create(&admin, "up", &outside, &["--keyring", &kf], 1)
            .contains("not under a configured mirror root")
    );
    create(&admin, "up", &dp, &["--keyring", &kf], 0);
    // Mirrors and publishes never share a tree.
    let tp = trees.join("t").display().to_string();
    admin.ok(&[
        "publish",
        "create",
        "t",
        "--path",
        &tp,
        "--dist",
        "noble",
        "--component",
        "main",
        "--arch",
        "amd64",
        "--no-sign",
        "-y",
    ]);
    assert!(create(&admin, "m2", &tp, &["--insecure"], 1).contains("overlaps publish"));
    // trees/ is both a publish and a mirror root: a publish inside a
    // mirror's tree is refused as well.
    let m3 = trees.join("m3").display().to_string();
    create(&admin, "m3", &m3, &["--insecure"], 0);
    let inside = trees.join("m3/sub").display().to_string();
    let err = admin.fails(
        &[
            "publish",
            "create",
            "p2",
            "--path",
            &inside,
            "--dist",
            "noble",
            "--component",
            "main",
            "--arch",
            "amd64",
            "--no-sign",
            "-y",
        ],
        1,
    );
    assert!(err.contains("overlaps the tree of mirror \"m3\""), "{err}");
    admin.ok(&["mirror", "delete", "m3"]);

    // Sync as a job, following its log.
    let out = ci.ok(&["mirror", "sync", "up", "--wait"]);
    assert!(
        out.contains("switched to the new indices") && out.contains("done:"),
        "{out}"
    );
    for f in [
        "dists/noble/InRelease",
        "dists/noble/main/binary-amd64/Packages.xz",
    ] {
        assert_eq!(
            std::fs::read(archive.join(f)).unwrap(),
            std::fs::read(dest.join(f)).unwrap(),
            "{f}"
        );
    }
    assert!(!dest.join("dists/noble/main/binary-arm64").exists());
    let ls = ci.ok(&["mirror", "ls"]);
    assert!(ls.starts_with("up\t") && ls.contains("\tdone\t"), "{ls}");
    let out = ci.ok(&["mirror", "sync", "up", "--wait"]);
    assert!(out.contains("0 file(s) downloaded"), "{out}");
    let jobs = ci.ok(&["jobs"]);
    let job: String = jobs
        .lines()
        .find(|l| l.contains("mirror-sync mirror:up"))
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .trim_start_matches('#')
        .to_string();
    assert!(ci.ok(&["log", &job]).contains("mirror up:"));
    assert!(
        ci.fails(&["mirror", "stop", "up"], 1)
            .contains("not syncing")
    );

    // Adopting a directory that already holds files: nothing is deleted
    // until asked once.
    let adopted = mroot.join("adopted");
    let stray = adopted.join("pool/main/s/stray/stray_1_all.deb");
    std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
    std::fs::write(&stray, "old tool's file").unwrap();
    let ap = adopted.display().to_string();
    assert!(create(&admin, "adopted", &ap, &["--keyring", &kf], 1).contains("already holds files"));
    let out = admin.run(
        &[
            "mirror",
            "create",
            "adopted",
            "--url",
            &url,
            "--path",
            &ap,
            "--dist",
            "noble",
            "--keyring",
            &kf,
            "--adopt",
        ],
        "",
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("prune off"),
        "{}",
        text(&out)
    );
    let out = ci.ok(&["mirror", "sync", "adopted", "--wait"]);
    assert!(out.contains("1 unreferenced left"), "{out}");
    assert!(stray.exists());
    // Deleting what was adopted is the admin's decision.
    assert!(
        ci.fails(&["mirror", "sync", "adopted", "--prune", "--wait"], 7)
            .contains("admin")
    );
    assert!(stray.exists());
    admin.ok(&["mirror", "sync", "adopted", "--prune", "--wait"]);
    assert!(!stray.exists());

    // A key upstream did not sign with: the job fails, the mirror says so.
    let other = aptpod_core::testutil::TestKey::new().unwrap();
    let otherfile = d.join("other.gpg");
    std::fs::write(&otherfile, &other.public).unwrap();
    let wp = mroot.join("wrong").display().to_string();
    create(
        &admin,
        "wrong",
        &wp,
        &["--keyring", &otherfile.display().to_string()],
        0,
    );
    let err = ci.fails(&["mirror", "sync", "wrong", "--wait"], 1);
    assert!(err.contains("failed"), "{err}");
    assert!(
        ci.ok(&["mirror", "show", "wrong"])
            .contains("# last sync: failed")
    );

    // Stopping a running sync; the job ends interrupted.
    let sp = mroot.join("slow").display().to_string();
    admin.ok(&[
        "mirror",
        "create",
        "slow",
        "--url",
        &slow_url,
        "--path",
        &sp,
        "--dist",
        "noble",
        "--keyring",
        &kf,
    ]);
    let started = ci.ok(&["mirror", "sync", "slow"]);
    // "slow: job 12 started; ..."
    let job = started.split_whitespace().nth(2).unwrap().to_string();
    assert!(
        ci.fails(&["mirror", "sync", "slow"], 4)
            .contains("already syncing")
    );
    std::thread::sleep(Duration::from_millis(800));
    ci.ok(&["mirror", "stop", "slow"]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while ci.ok(&["mirror", "show", "slow"]).contains("syncing now")
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        ci.ok(&["mirror", "show", "slow"])
            .contains("# last sync: interrupted")
    );
    let jobs = ci.ok(&["jobs"]);
    assert!(jobs.contains(&format!("#{job} ")), "job {job}: {jobs}");
    assert!(!mroot.join("slow/dists/noble").exists(), "nothing switched");

    // Config export/import carries mirrors; edit through $VISUAL.
    let exported = admin.ok(&["config", "export"]);
    assert!(
        exported.contains("[mirrors.up]") && exported.contains("BEGIN PGP PUBLIC KEY BLOCK"),
        "{exported}"
    );
    let changed = exported.replacen("parallel = 8", "parallel = 2", 1);
    let out = admin.run(&["config", "import", "-", "-y"], &changed);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("mirror adopted: update")
            || String::from_utf8_lossy(&out.stdout).contains("mirror up: update"),
        "{}",
        text(&out)
    );
    let again = admin.run(&["config", "import", "-", "-y"], &changed);
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("nothing to do"),
        "{}",
        text(&again)
    );
    let out = admin.run_env(
        &["mirror", "edit", "up"],
        "",
        &[(
            "VISUAL",
            "sed -i s/max_delete\\ =\\ 10000/max_delete\\ =\\ 7/",
        )],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        admin
            .ok(&["mirror", "get", "up"])
            .contains("max_delete = 7")
    );

    // Forgetting a mirror leaves its tree.
    assert!(
        admin
            .ok(&["mirror", "delete", "wrong"])
            .contains("still at")
    );
    assert!(!ci.ok(&["mirror", "ls"]).contains("wrong\t"));
    assert!(dest.join("dists/noble/InRelease").is_file());

    // Shutting down during a sync: the server stops promptly, the sync is
    // interrupted and says so after the restart.
    ci.ok(&["mirror", "sync", "slow"]);
    std::thread::sleep(Duration::from_millis(800));
    let t = std::time::Instant::now();
    assert!(srv.stop().success());
    assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
    let srv = Server::start(&bin, &cfg);
    let ci = Cli {
        url: srv.url.clone(),
        ..ci
    };
    let token = ci.token();
    assert!(
        ci.ok(&["--token", &token, "mirror", "show", "slow"])
            .contains("# last sync: interrupted")
    );
    assert!(srv.stop().success());
}

/// rsync-apt mirrors through the server: definition rules, a two-pass
/// sync as a job, the trace file.
#[test]
fn rsync_mirrors() {
    let Some((dir, bin, cfg)) = prepare_extra("") else {
        return;
    };
    let d = dir.path();
    let archive = d.join("upstream");
    std::fs::create_dir_all(archive.join("dists/stable")).unwrap();
    std::fs::create_dir_all(archive.join("pool/main/a")).unwrap();
    std::fs::write(archive.join("dists/stable/Release"), "Suite: stable\n").unwrap();
    std::fs::write(archive.join("pool/main/a/a_1_all.deb"), "payload").unwrap();
    let Some((mut daemon, url)) = rsync_daemon(&archive, d) else {
        return;
    };
    let mroot = d.join("mirrors");
    let base = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!("{base}mirror_roots = [{:?}]\n", mroot.display().to_string()),
    )
    .unwrap();
    let srv = Server::start(&bin, &cfg);
    let admin = Cli {
        creds: d.join("admin.toml"),
        url: srv.url.clone(),
    };
    login(&admin, "admin");
    let dest = mroot.join("deb");
    let dp = dest.display().to_string();
    let create = |extra: &[&str], code: i32| {
        let mut args = vec![
            "mirror",
            "create",
            "deb",
            "--kind",
            "rsync-apt",
            "--url",
            &url,
            "--path",
            &dp,
        ];
        args.extend_from_slice(extra);
        if code == 0 {
            admin.ok(&args)
        } else {
            admin.fails(&args, code)
        }
    };
    assert!(create(&["--dist", "stable"], 1).contains("for http mirrors"));
    let bad = admin.fails(
        &[
            "mirror",
            "create",
            "deb",
            "--kind",
            "rsync-apt",
            "--url",
            "host:/srv",
            "--path",
            &dp,
        ],
        1,
    );
    assert!(bad.contains("not an rsync daemon source"), "{bad}");
    create(&["--trace-host", "mirror.example"], 0);
    assert!(
        admin.ok(&["mirror", "check", "deb"]).contains("ok"),
        "rsync probe"
    );
    let out = admin.ok(&["mirror", "sync", "deb", "--wait"]);
    assert!(
        out.contains("rsync payloads") && out.contains("rsync indexes") && out.contains("done:"),
        "{out}"
    );
    assert_eq!(
        std::fs::read(dest.join("pool/main/a/a_1_all.deb")).unwrap(),
        b"payload"
    );
    assert!(dest.join("project/trace/mirror.example").is_file());
    assert!(
        admin
            .ok(&["mirror", "get", "deb"])
            .contains("kind = \"rsync-apt\"")
    );
    assert!(srv.stop().success());
    let _ = daemon.kill();
    let _ = daemon.wait();
}

/// Schedules (in the configured time zone), public status files, upstream
/// checks and `sync --all`.
#[test]
fn schedules_status_and_checks() {
    let up = tempfile::tempdir().unwrap();
    let Some((archive, public, _key)) = signed_upstream(up.path()) else {
        return;
    };
    let url = static_server(archive.clone(), Duration::ZERO);
    let Some((dir, bin, cfg)) = prepare_extra("") else {
        return;
    };
    let d = dir.path();
    let (mroot, status) = (d.join("mirrors"), d.join("status"));
    let base = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!(
            "{base}mirror_roots = [{:?}]\nmirror_workers = 2\ntimezone = \"Asia/Seoul\"\nstatus_dir = {:?}\n",
            mroot.display().to_string(),
            status.display().to_string()
        ),
    )
    .unwrap();
    let srv = Server::start(&bin, &cfg);
    let admin = Cli {
        creds: d.join("admin.toml"),
        url: srv.url.clone(),
    };
    login(&admin, "admin");
    let keyfile = d.join("up.gpg");
    std::fs::write(&keyfile, &public).unwrap();
    let kf = keyfile.display().to_string();
    let mk = |name: &str, extra: &[&str]| {
        let p = mroot.join(name).display().to_string();
        let mut args = vec![
            "mirror",
            "create",
            name,
            "--url",
            &url,
            "--path",
            &p,
            "--dist",
            "noble",
            "--keyring",
            &kf,
        ];
        args.extend_from_slice(extra);
        admin.ok(&args);
    };
    assert!(
        admin
            .fails(
                &[
                    "mirror",
                    "create",
                    "bad",
                    "--url",
                    &url,
                    "--path",
                    &mroot.join("bad").display().to_string(),
                    "--dist",
                    "noble",
                    "--keyring",
                    &kf,
                    "--schedule",
                    "61 * * * *"
                ],
                1
            )
            .contains("minute")
    );
    mk("tick", &["--schedule", "* * * * *"]);
    mk("manual", &[]);

    // Status files exist before any sync; next run is in +09:00.
    let ls = admin.ok(&["mirror", "ls"]);
    assert!(
        ls.contains("+09:00") && ls.contains("not scheduled"),
        "{ls}"
    );
    let st: Value =
        serde_json::from_str(&std::fs::read_to_string(status.join("manual.json")).unwrap())
            .unwrap();
    assert_eq!(st["state"], "never synced", "{st}");
    assert!(st["next_run"].is_null());
    // (tick may already be syncing: its schedule matches every minute.)
    let st: Value =
        serde_json::from_str(&std::fs::read_to_string(status.join("tick.json")).unwrap()).unwrap();
    assert!(st["next_run"].as_str().unwrap().ends_with("+09:00"), "{st}");

    // Checks: a good upstream, then one whose key does not match.
    let out = admin.ok(&["mirror", "check", "manual"]);
    assert!(out.contains("ok noble: InRelease signature valid"), "{out}");
    let other = aptpod_core::testutil::TestKey::new().unwrap();
    let otherfile = d.join("other.gpg");
    std::fs::write(&otherfile, &other.public).unwrap();
    let wp = mroot.join("wrong").display().to_string();
    admin.ok(&[
        "mirror",
        "create",
        "wrong",
        "--url",
        &url,
        "--path",
        &wp,
        "--dist",
        "noble",
        "--keyring",
        &otherfile.display().to_string(),
        "--disabled",
    ]);
    assert!(
        admin
            .fails(&["mirror", "check", "wrong"], 1)
            .contains("check failed: wrong")
    );

    // sync --all: every enabled mirror (not the disabled one).
    let out = admin.ok(&["mirror", "sync", "--all", "--wait"]);
    assert!(
        out.contains("== manual") && out.contains("== tick") && !out.contains("== wrong"),
        "{out}"
    );
    let st: Value =
        serde_json::from_str(&std::fs::read_to_string(status.join("manual.json")).unwrap())
            .unwrap();
    assert_eq!(st["state"], "synced");
    assert!(st["last_success"].as_str().unwrap().ends_with("+09:00"));
    let svg = std::fs::read_to_string(status.join("manual.svg")).unwrap();
    assert!(svg.contains("synced 20"), "{svg}");
    let index: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(status.join("index.json")).unwrap()).unwrap();
    assert_eq!(index.len(), 3);

    // The every-minute schedule fires within a minute, as the scheduler.
    let deadline = std::time::Instant::now() + Duration::from_secs(75);
    loop {
        let jobs = admin.ok(&["jobs", "--json"]);
        let jobs: Vec<Value> = serde_json::from_str(&jobs).unwrap();
        if jobs
            .iter()
            .any(|j| j["user"] == "scheduler" && j["resource"] == "mirror:tick")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no scheduled sync: {jobs:?}"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
    // A deleted mirror's status files go.
    admin.ok(&["mirror", "delete", "wrong"]);
    assert!(!status.join("wrong.json").exists() && !status.join("wrong.svg").exists());
    assert!(srv.stop().success());
}

/// config_file: checked offline, applied at start and on SIGHUP, what it
/// declares is read-only through the API, data operations still work,
/// destructive changes need `force` in the file, a broken file keeps the
/// previous state.
#[test]
fn config_file_workflow() {
    let Some((dir, bin, cfg)) = prepare_extra("") else {
        return;
    };
    let d = dir.path();
    let (trees, mroot, file) = (d.join("trees"), d.join("mirrors"), d.join("aptpod.toml"));
    let base = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!(
            "{base}mirror_roots = [{:?}]\nconfig_file = {:?}\n",
            mroot.display().to_string(),
            file.display().to_string()
        ),
    )
    .unwrap();
    let fp = trees.join("fp").display().to_string();
    let fm = mroot.join("fm").display().to_string();
    let write_file = |dists: &str, extra: &str| {
        std::fs::write(
            &file,
            format!(
                "{extra}[publishes.fp]\npath = {fp:?}\ndists = [{dists}]\ncomponents = [\"main\"]\narchitectures = [\"amd64\"]\nsign = false\n\n\
                 [mirrors.fm]\nurl = \"http://127.0.0.1:9/\"\npath = {fm:?}\ndists = [\"stable\"]\ninsecure = true\n"
            ),
        )
        .unwrap()
    };
    let check = |code: bool| {
        let o = Command::new(&bin)
            .arg("-c")
            .arg(&cfg)
            .arg("config-check")
            .output()
            .unwrap();
        assert_eq!(o.status.success(), code, "{}", text(&o));
        text(&o)
    };
    // Checked without a server.
    std::fs::write(&file, "[publishes.bad]\npath = \"/elsewhere/x\"\ndists = [\"a\"]\ncomponents = [\"main\"]\narchitectures = [\"amd64\"]\n").unwrap();
    assert!(check(false).contains("not under a configured publish root"));
    write_file("\"jammy\", \"noble\"", "");
    assert!(check(true).contains("ok (1 publish(es), 1 mirror(s))"));

    let srv = Server::start(&bin, &cfg);
    let admin = Cli {
        creds: d.join("admin.toml"),
        url: srv.url.clone(),
    };
    login(&admin, "admin");
    assert!(
        admin.ok(&["publish", "ls"]).contains("fp\t")
            && admin.ok(&["publish", "ls"]).contains("(config file)")
    );
    assert!(admin.ok(&["mirror", "ls"]).contains("(config file)"));
    let status = admin.ok(&["config", "status"]);
    assert!(
        status.contains(": ok")
            && status.contains("managed publishes: fp")
            && status.contains("managed mirrors: fm"),
        "{status}"
    );

    // Read-only through the API; packages still go in.
    let err = admin.fails(&["publish", "delete", "fp", "--force", "-y"], 4);
    assert!(
        err.contains("managed by") && err.contains("aptpod.toml"),
        "{err}"
    );
    assert!(
        admin
            .fails(&["mirror", "delete", "fm"], 4)
            .contains("managed by")
    );
    let debs = d.join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let deb = build_deb(&debs, "filepkg", "1.0~jammy")
        .display()
        .to_string();
    admin.ok(&["add", "fp", "main", &deb, "-y"]);
    // An export imports back as it is: managed items are skipped.
    let exported = admin.ok(&["config", "export"]);
    let out = admin.run(&["config", "import", "-", "-y"], &exported);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("managed by the config file; skipped"),
        "{}",
        text(&out)
    );

    let reload = |srv: &Server| {
        let before = admin.ok(&["config", "status", "--json"]);
        Command::new("kill")
            .args(["-HUP", &srv.child.id().to_string()])
            .status()
            .unwrap();
        for _ in 0..100 {
            if admin.ok(&["config", "status", "--json"]) != before {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("reload not applied");
    };
    // Adding a dist: previewed, then applied on SIGHUP.
    write_file("\"jammy\", \"noble\", \"resolute\"", "");
    let plan = admin.ok(&["config", "plan"]);
    assert!(plan.contains("resolute"), "{plan}");
    reload(&srv);
    assert!(trees.join("fp/dists/resolute").is_dir());
    // Dropping jammy (it holds a package) is refused until the file says force.
    write_file("\"noble\", \"resolute\"", "");
    reload(&srv);
    let status = admin.ok(&["config", "status"]);
    assert!(
        status.contains("FAILED") && status.contains("force"),
        "{status}"
    );
    assert!(trees.join("fp/dists/jammy").is_dir());
    write_file("\"noble\", \"resolute\"", "force = true\n");
    reload(&srv);
    assert!(admin.ok(&["config", "status"]).contains(": ok"));
    assert!(!trees.join("fp/dists/jammy").exists());

    // A broken file keeps what was there, managed marks included.
    std::fs::write(&file, "[publishes.fp\n").unwrap();
    reload(&srv);
    assert!(admin.ok(&["config", "status"]).contains("FAILED"));
    admin.fails(&["publish", "delete", "fp", "--force", "-y"], 4);
    // Leaving the file makes an item editable again (it is not deleted).
    std::fs::write(
        &file,
        format!("[publishes.fp]\npath = {fp:?}\ndists = [\"noble\", \"resolute\"]\ncomponents = [\"main\"]\narchitectures = [\"amd64\"]\nsign = false\n"),
    )
    .unwrap();
    reload(&srv);
    assert!(admin.ok(&["mirror", "ls"]).contains("fm\t"), "kept");
    admin.ok(&["mirror", "delete", "fm"]);
    assert!(srv.stop().success());
}

/// A re-signed mirror through the server: definition rules, a sync that
/// creates the publish signed with the server's key and a snapshot, the
/// publish refusing manual changes, export, and delete leaving the publish.
#[test]
fn resign_mirrors() {
    let Some(ours) = GpgKey::new() else {
        return;
    };
    let up = tempfile::tempdir().unwrap();
    let Some((archive, up_public, _up_key)) = signed_upstream(up.path()) else {
        return;
    };
    let url = static_server(archive, Duration::ZERO);
    let Some((dir, bin, cfg)) = prepare_extra(&ours.signing_toml(&ours.fpr)) else {
        return;
    };
    let d = dir.path();
    let srv = Server::start(&bin, &cfg);
    let cli = |name: &str| Cli {
        creds: d.join(format!("{name}.toml")),
        url: srv.url.clone(),
    };
    let (admin, ci) = (cli("admin"), cli("ci"));
    login(&admin, "admin");
    login(&ci, "ci");
    let keyfile = d.join("upstream.gpg");
    std::fs::write(&keyfile, &up_public).unwrap();
    let kf = keyfile.display().to_string();
    let tree = d.join("trees/rs");
    let tp = tree.display().to_string();
    let base = [
        "mirror", "create", "rs", "--kind", "resign", "--url", &url, "--path", &tp, "--dist",
        "noble",
    ];
    let with = |extra: &[&str]| -> Vec<String> {
        base.iter().chain(extra).map(|s| s.to_string()).collect()
    };
    let run = |c: &Cli, args: &[String], code: Option<i32>| -> String {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        match code {
            None => c.ok(&a),
            Some(n) => c.fails(&a, n),
        }
    };
    // Unverified content is never signed with our key.
    let err = run(&admin, &with(&["--insecure"]), Some(1));
    assert!(err.contains("no insecure"), "{err}");
    // The tree is a publish: publish_roots, not mirror_roots.
    let outside = d.join("elsewhere").display().to_string();
    let mut args = with(&["--keyring", &kf]);
    args[8] = outside;
    let err = run(&admin, &args, Some(1));
    assert!(err.contains("publish root"), "{err}");
    run(
        &admin,
        &with(&["--keyring", &kf, "--keep-snapshots", "3"]),
        None,
    );

    let out = ci.ok(&["mirror", "sync", "rs", "--wait"]);
    assert!(out.contains("revision #") && out.contains("done:"), "{out}");
    let rows: Vec<Value> = serde_json::from_str(&ci.ok(&["ls", "rs", "--json"])).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "sched-pkg");
    let ir = std::fs::read(tree.join("dists/noble/InRelease")).unwrap();
    let k = |b: &[u8]| aptpod_core::verify::Keyring::from_bytes(b).unwrap();
    aptpod_core::verify::verify_inrelease(&k(&ours.public), &ir).expect("our signature");
    assert!(aptpod_core::verify::verify_inrelease(&k(&up_public), &ir).is_err());
    let (code, snaps) = api(
        &srv.url,
        &ci.token(),
        "GET",
        "/api/v1/publishes/rs/snapshots",
        None,
    );
    assert_eq!(code, 200);
    assert!(
        snaps[0]["label"].as_str().unwrap().starts_with("sync-"),
        "{snaps}"
    );
    let out = ci.ok(&["mirror", "sync", "rs", "--wait"]);
    assert!(out.contains("publish unchanged"), "{out}");
    // Overriding the removal brake is an admin call.
    ci.fails(&["mirror", "sync", "rs", "--prune"], 7);

    // Filled by the mirror: manual changes are refused (managed, exit 4).
    let debs = d.join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let deb = build_deb(&debs, "local-pkg", "1.0").display().to_string();
    let err = ci.fails(&["add", "rs", "main", &deb, "-d", "noble", "-y"], 4);
    assert!(err.contains("filled by mirror"), "{err}");
    let err = ci.fails(&["rm", "rs", "main", "sched-pkg", "-y"], 4);
    assert!(err.contains("filled by mirror"), "{err}");
    // Export leaves the publish out (the mirror makes it).
    let exported = admin.ok(&["config", "export"]);
    assert!(exported.contains("[mirrors.rs]") && exported.contains("kind = \"resign\""));
    assert!(!exported.contains("[publishes.rs]"), "{exported}");

    // Deleting the mirror leaves an ordinary publish.
    let out = admin.ok(&["mirror", "delete", "rs"]);
    assert!(out.contains("publish rs stays"), "{out}");
    ci.ok(&["add", "rs", "main", &deb, "-d", "noble", "-y"]);
    assert!(srv.stop().success());
}
