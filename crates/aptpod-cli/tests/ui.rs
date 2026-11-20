// SPDX-License-Identifier: GPL-3.0-only

//! The web UI against the real server: sessions, CSRF, preview/apply
//! through hidden fields, roles, stale previews, escaping, headers.

mod common;

use common::*;
use reqwest::StatusCode;
use reqwest::blocking::Client;

/// A browser: keeps cookies, does not follow redirects.
fn browser() -> Client {
    Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> &'a str {
    let i = s
        .find(start)
        .unwrap_or_else(|| panic!("{start:?} not in page:\n{s}"))
        + start.len();
    let j = s[i..].find(end).unwrap() + i;
    &s[i..j]
}

/// Logs in through the form; returns the CSRF token of the session.
fn ui_login(b: &Client, url: &str, user: &str) -> String {
    let page = b.get(format!("{url}login")).send().unwrap().text().unwrap();
    let csrf = between(&page, "name=\"csrf\" value=\"", "\"").to_string();
    let r = b
        .post(format!("{url}login"))
        .form(&[
            ("user", user),
            ("password", &format!("{user}-password")),
            ("csrf", &csrf),
        ])
        .send()
        .unwrap();
    assert_eq!(r.status(), StatusCode::SEE_OTHER, "login as {user}");
    let set = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        set.iter().any(|c| c.starts_with("aptpod_session=")
            && c.contains("HttpOnly")
            && c.contains("SameSite=Lax")),
        "{set:?}"
    );
    let home = b.get(url).send().unwrap().text().unwrap();
    between(&home, "\"X-CSRF-Token\": \"", "\"").to_string()
}

fn htmx_post(
    b: &Client,
    url: &str,
    csrf: Option<&str>,
    form: &[(&str, &str)],
) -> (StatusCode, String) {
    let mut r = b.post(url).header("hx-request", "true").form(form);
    if let Some(c) = csrf {
        r = r.header("x-csrf-token", c);
    }
    let r = r.send().unwrap();
    (r.status(), r.text().unwrap())
}

/// The hidden fields of a preview, as a form to post back.
fn hidden_fields(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("<input type=\"hidden\" name=\"") {
        rest = &rest[i + 27..];
        let name = &rest[..rest.find('"').unwrap()];
        let v = between(rest, "value=\"", "\"");
        // Undo the HTML escaping of attribute values.
        let v = v
            .replace("&#34;", "\"")
            .replace("&#39;", "'")
            .replace("&#60;", "<")
            .replace("&#62;", ">")
            .replace("&#38;", "&");
        out.push((name.to_string(), v));
    }
    out
}

#[test]
fn web_ui() {
    let Some((dir, bin, cfg)) = prepare() else {
        return;
    };
    let d = dir.path();
    let srv = Server::start(&bin, &cfg);
    let url = srv.url.clone() + "/";
    let admin = Cli {
        creds: d.join("admin.toml"),
        url: srv.url.clone(),
    };
    login(&admin, "admin");
    let tp = d.join("trees/t").display().to_string();
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
    let debs = d.join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    for (n, v) in [("ui-a", "1.0~noble"), ("ui-b", "1.0~noble")] {
        let deb = build_deb(&debs, n, v).display().to_string();
        admin.ok(&["add", "t", "main", &deb, "-y"]);
    }

    // Not logged in: to the login page; the session never opens the API.
    let anon = browser();
    let r = anon.get(&url).send().unwrap();
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    assert_eq!(r.headers()["location"], "/login");
    let page = anon
        .get(format!("{url}login"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    let csrf = between(&page, "name=\"csrf\" value=\"", "\"").to_string();
    let r = anon
        .post(format!("{url}login"))
        .form(&[
            ("user", "admin"),
            ("password", "wrong-password"),
            ("csrf", &csrf),
        ])
        .send()
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // A login form without its cookie token is refused.
    let r = browser()
        .post(format!("{url}login"))
        .form(&[
            ("user", "admin"),
            ("password", "admin-password"),
            ("csrf", "forged"),
        ])
        .send()
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    let b = browser();
    let token = ui_login(&b, &url, "ci");
    let home = b.get(&url).send().unwrap();
    let csp = home.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("script-src 'self'"), "{csp}");
    let home = home.text().unwrap();
    assert!(home.contains("href=\"/p/t\""), "{home}");
    assert!(home.contains("\"allowEval\":false") && home.contains("\"allowScriptTags\":false"));
    assert_eq!(
        b.get(format!("{}api/v1/whoami", url))
            .send()
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let js = b.get(format!("{url}ui/htmx-2.0.11.min.js")).send().unwrap();
    assert_eq!(js.headers()["content-type"], "text/javascript");
    use sha2::Digest;
    let digest = sha2::Sha256::digest(js.bytes().unwrap());
    assert_eq!(
        digest
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect::<String>(),
        "d6fdc75f204e6bdefa99b69bf1e6d4ac69b8a364f77929f45c13476b4000f717"
    );

    let page = b.get(format!("{url}p/t")).send().unwrap().text().unwrap();
    assert!(page.contains("Remove packages"), "publisher sees the form");
    let list = b
        .get(format!("{url}p/t/packages?q=ui-*"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(list.contains("ui-a") && list.contains("ui-b"), "{list}");

    // Without the CSRF token nothing happens.
    let rm = format!("{url}p/t/rm");
    let (st, _) = htmx_post(
        &b,
        &rm,
        None,
        &[("component", "main"), ("packages", "ui-a"), ("apply", "1")],
    );
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(admin.ok(&["ls", "t"]).contains("ui-a"));

    // Preview, then apply with the hidden fields.
    let (st, preview) = htmx_post(
        &b,
        &rm,
        Some(&token),
        &[("component", "main"), ("packages", "ui-a")],
    );
    assert_eq!(st, StatusCode::OK);
    assert!(
        preview.contains("ui-a") && preview.contains("Apply"),
        "{preview}"
    );
    let fields = hidden_fields(&preview);
    let form: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (_, done) = htmx_post(&b, &rm, Some(&token), &form);
    assert!(done.contains("Applied"), "{done}");
    assert!(!admin.ok(&["ls", "t"]).contains("ui-a"));

    // A change between preview and apply: refused as stale.
    let (_, preview) = htmx_post(
        &b,
        &rm,
        Some(&token),
        &[("component", "main"), ("packages", "ui-b")],
    );
    let fields = hidden_fields(&preview);
    let deb = build_deb(&debs, "ui-c", "1.0~noble").display().to_string();
    admin.ok(&["add", "t", "main", &deb, "-y"]);
    let form: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (_, stale) = htmx_post(&b, &rm, Some(&token), &form);
    assert!(stale.contains("changed since the preview"), "{stale}");
    assert!(admin.ok(&["ls", "t"]).contains("ui-b"));

    // Text from requests is escaped.
    let (_, err) = htmx_post(
        &b,
        &rm,
        Some(&token),
        &[("component", "<script>x</script>"), ("packages", "ui-b")],
    );
    assert!(
        err.contains("&#60;script&#62;") && !err.contains("<script>x"),
        "{err}"
    );

    // A reader sees no write form and cannot write anyway.
    let viewer = browser();
    let vt = ui_login(&viewer, &url, "viewer");
    let page = viewer
        .get(format!("{url}p/t"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(!page.contains("Remove packages"));
    let (_, err) = htmx_post(
        &viewer,
        &rm,
        Some(&vt),
        &[("component", "main"), ("packages", "ui-b"), ("apply", "1")],
    );
    assert!(err.contains("publisher role"), "{err}");
    assert!(admin.ok(&["ls", "t"]).contains("ui-b"));

    // Jobs and a job page.
    let jobs = b.get(format!("{url}jobs")).send().unwrap().text().unwrap();
    assert!(jobs.contains("rm") && jobs.contains("publish:t"), "{jobs}");

    // Logout ends the session (also for a copied cookie).
    let (st, _) = htmx_post(&b, &format!("{url}logout"), Some(&token), &[]);
    assert_eq!(st, StatusCode::OK);
    assert_eq!(b.get(&url).send().unwrap().status(), StatusCode::SEE_OTHER);
    assert!(srv.stop().success());
}

/// Add (multipart upload), promote, snapshots, history, diff and rollback
/// through the pages.
#[test]
fn web_ui_repo_actions() {
    let Some((dir, bin, cfg)) = prepare() else {
        return;
    };
    let d = dir.path();
    let srv = Server::start(&bin, &cfg);
    let url = srv.url.clone() + "/";
    let admin = Cli {
        creds: d.join("admin.toml"),
        url: srv.url.clone(),
    };
    login(&admin, "admin");
    let tp = d.join("trees/t").display().to_string();
    admin.ok(&[
        "publish",
        "create",
        "t",
        "--path",
        &tp,
        "--dist",
        "noble",
        "--component",
        "main,extra",
        "--arch",
        "amd64",
        "--no-sign",
        "-y",
    ]);
    let b = browser();
    let token = ui_login(&b, &url, "ci");
    let base = format!("{url}p/t");

    // Add: the files go up as multipart, the preview lists them by name.
    let debs = d.join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let deb = build_deb(&debs, "web-pkg", "1.0~noble");
    let form = reqwest::blocking::multipart::Form::new()
        .text("component", "main")
        .text("dists", "")
        .file("file", &deb)
        .unwrap();
    let preview = b
        .post(format!("{base}/add"))
        .header("hx-request", "true")
        .header("x-csrf-token", &token)
        .multipart(form)
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        preview.contains("web-pkg 1.0~noble") && preview.contains("Apply"),
        "{preview}"
    );
    let fields = hidden_fields(&preview);
    let form: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (_, done) = htmx_post(&b, &format!("{base}/add/apply"), Some(&token), &form);
    assert!(done.contains("Applied"), "{done}");
    assert!(admin.ok(&["ls", "t"]).contains("web-pkg"));

    // A label with markup in its description: shown escaped.
    let (_, snaps) = htmx_post(
        &b,
        &format!("{base}/snapshots"),
        Some(&token),
        &[
            ("label", "one-pkg"),
            ("description", "<script>alert(1)</script>"),
        ],
    );
    assert!(
        snaps.contains("one-pkg")
            && snaps.contains("&#60;script&#62;")
            && !snaps.contains("<script>alert"),
        "{snaps}"
    );

    // Promote to the other component.
    let (_, preview) = htmx_post(
        &b,
        &format!("{base}/promote"),
        Some(&token),
        &[
            ("component", "main"),
            ("packages", "web-pkg"),
            ("to_publish", "t"),
            ("to_component", "extra"),
        ],
    );
    assert!(preview.contains("noble/extra"), "{preview}");
    let fields = hidden_fields(&preview);
    let form: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (_, done) = htmx_post(&b, &format!("{base}/promote"), Some(&token), &form);
    assert!(done.contains("Applied"), "{done}");
    assert!(admin.ok(&["ls", "t", "-c", "extra"]).contains("web-pkg"));

    // History and diff against the label, then roll back to it.
    let hist = b
        .get(format!("{base}/history"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(hist.contains("HEAD") && hist.contains("one-pkg"), "{hist}");
    let diff = b
        .get(format!("{base}/diff?from=one-pkg"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        diff.contains("+ noble/extra web-pkg 1.0~noble amd64"),
        "{diff}"
    );
    let (_, preview) = htmx_post(
        &b,
        &format!("{base}/rollback"),
        Some(&token),
        &[("to", "one-pkg")],
    );
    let fields = hidden_fields(&preview);
    let form: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (_, done) = htmx_post(&b, &format!("{base}/rollback"), Some(&token), &form);
    assert!(done.contains("Applied"), "{done}");
    assert!(!admin.ok(&["ls", "t", "-c", "extra"]).contains("web-pkg"));

    // Drop the label.
    let (_, snaps) = htmx_post(
        &b,
        &format!("{base}/snapshots/one-pkg/drop"),
        Some(&token),
        &[],
    );
    assert!(snaps.contains("No snapshot labels"), "{snaps}");
    assert!(srv.stop().success());
}

/// Layout editing (and the config file's read-only items), mirrors, job
/// logs, tokens, password and users through the pages.
#[test]
fn web_ui_admin() {
    let Some((dir, bin, cfg)) = prepare() else {
        return;
    };
    let d = dir.path();
    let (trees, mroot, file) = (d.join("trees"), d.join("mirrors"), d.join("aptpod.toml"));
    let fp = trees.join("fp").display().to_string();
    std::fs::write(
        &file,
        format!("[publishes.fp]\npath = {fp:?}\ndists = [\"noble\"]\ncomponents = [\"main\"]\narchitectures = [\"amd64\"]\nsign = false\n"),
    )
    .unwrap();
    let base_cfg = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!(
            "{base_cfg}mirror_roots = [{:?}]\nconfig_file = {:?}\n",
            mroot.display().to_string(),
            file.display().to_string()
        ),
    )
    .unwrap();
    let srv = Server::start(&bin, &cfg);
    let url = srv.url.clone() + "/";
    let admin = Cli {
        creds: d.join("admin.toml"),
        url: srv.url.clone(),
    };
    login(&admin, "admin");
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
    let a = browser();
    let at = ui_login(&a, &url, "admin");

    // A new publish from the overview.
    let np = trees.join("webnew").display().to_string();
    let layout = format!(
        "path = {np:?}\ndists = [\"noble\"]\ncomponents = [\"main\"]\narchitectures = [\"amd64\"]\nsign = false\n"
    );
    let (_, preview) = htmx_post(
        &a,
        &format!("{url}publishes/new"),
        Some(&at),
        &[("alias", "webnew"), ("layout", &layout)],
    );
    assert!(preview.contains("Apply"), "{preview}");
    let fields = hidden_fields(&preview);
    let form: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (_, done) = htmx_post(&a, &format!("{url}publishes/new"), Some(&at), &form);
    assert!(done.contains("Applied"), "{done}");
    assert!(admin.ok(&["publish", "ls"]).contains("webnew\t"));

    // Layout: edit through the textarea, preview, apply.
    let page = a.get(format!("{url}p/t")).send().unwrap().text().unwrap();
    let toml_text = between(
        &page,
        "<textarea name=\"layout\" rows=\"14\" class=\"mono\">",
        "</textarea>",
    )
    .replace("&#34;", "\"")
    .replace("&#39;", "'");
    let edited = toml_text.replace("\"noble\"", "\"noble\", \"resolute\"");
    assert_ne!(edited, toml_text, "{toml_text}");
    let (_, preview) = htmx_post(
        &a,
        &format!("{url}p/t/layout"),
        Some(&at),
        &[("layout", &edited)],
    );
    assert!(preview.contains("resolute"), "{preview}");
    let fields = hidden_fields(&preview);
    let form: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (_, done) = htmx_post(&a, &format!("{url}p/t/layout"), Some(&at), &form);
    assert!(done.contains("Applied"), "{done}");
    assert!(trees.join("t/dists/resolute").is_dir());
    // A publish from the config file: no editor, and editing is refused.
    let page = a.get(format!("{url}p/fp")).send().unwrap().text().unwrap();
    assert!(
        page.contains("Managed by the server's config file") && !page.contains("name=\"layout\"")
    );
    let (_, err) = htmx_post(
        &a,
        &format!("{url}p/fp/layout"),
        Some(&at),
        &[("layout", &edited)],
    );
    assert!(err.contains("managed by"), "{err}");

    // Mirrors: list, page, check, sync, edit.
    let mp = mroot.join("m").display().to_string();
    admin.ok(&[
        "mirror",
        "create",
        "m",
        "--url",
        "http://127.0.0.1:9/",
        "--path",
        &mp,
        "--dist",
        "stable",
        "--insecure",
    ]);
    let list = a
        .get(format!("{url}mirrors"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        list.contains("href=\"/mirrors/m\"") && list.contains("never synced"),
        "{list}"
    );
    let page = a
        .get(format!("{url}mirrors/m"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        page.contains("max_delete = 10000") && page.contains("Sync now"),
        "{page}"
    );
    let (_, check) = htmx_post(&a, &format!("{url}mirrors/m/check"), Some(&at), &[]);
    assert!(check.contains("FAILED"), "{check}");
    let (_, started) = htmx_post(&a, &format!("{url}mirrors/m/sync"), Some(&at), &[]);
    let job = between(&started, "Follow job ", "<").to_string();
    for _ in 0..50 {
        let log = a
            .get(format!("{url}jobs/{job}/log"))
            .send()
            .unwrap()
            .text()
            .unwrap();
        if !log.contains("hx-trigger=\"every 2s\"") {
            assert!(log.contains("mirror m:"), "{log}");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let job_page = a
        .get(format!("{url}jobs/{job}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(job_page.contains("failed"), "{job_page}");
    let def = between(
        &page,
        "<textarea name=\"def\" rows=\"16\" class=\"mono\">",
        "</textarea>",
    )
    .replace("&#34;", "\"")
    .replace("max_delete = 10000", "max_delete = 5");
    let (_, saved) = htmx_post(
        &a,
        &format!("{url}mirrors/m/edit"),
        Some(&at),
        &[("def", &def)],
    );
    assert!(saved.contains("Saved"), "{saved}");
    assert!(admin.ok(&["mirror", "get", "m"]).contains("max_delete = 5"));

    // Account: a token made here works on the API until revoked.
    let (_, tokens) = htmx_post(
        &a,
        &format!("{url}account/tokens"),
        Some(&at),
        &[("name", "from-web")],
    );
    let token = between(&tokens, "<code class=\"mono\">", "</code>").to_string();
    let who = |t: &str| {
        reqwest::blocking::Client::new()
            .get(format!("{url}api/v1/whoami"))
            .bearer_auth(t)
            .send()
            .unwrap()
            .status()
    };
    assert_eq!(who(&token), StatusCode::OK);
    let rows: Vec<&str> = tokens
        .split("<tr>")
        .filter(|r| r.contains("from-web"))
        .collect();
    let id = between(rows[0], "<td class=\"num\">", "</td>").to_string();
    htmx_post(
        &a,
        &format!("{url}account/tokens/{id}/revoke"),
        Some(&at),
        &[],
    );
    assert_eq!(who(&token), StatusCode::UNAUTHORIZED);
    let (_, err) = htmx_post(
        &a,
        &format!("{url}account/password"),
        Some(&at),
        &[("current", "nope-nope"), ("new", "admin-password-2")],
    );
    assert!(err.contains("current password is wrong"), "{err}");
    // A page that does not exist: an error page with its status.
    let r = a.get(format!("{url}p/nope")).send().unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert!(r.text().unwrap().contains("<h1>Error</h1>"));

    // Users: an admin adds and changes them; a publisher may not.
    let (_, users) = htmx_post(
        &a,
        &format!("{url}admin/users"),
        Some(&at),
        &[
            ("name", "web-made"),
            ("role", "reader"),
            ("password", "web-made-password"),
        ],
    );
    assert!(users.contains("web-made"), "{users}");
    let (_, users) = htmx_post(
        &a,
        &format!("{url}admin/users/web-made"),
        Some(&at),
        &[("role", "publisher"), ("password", "")],
    );
    assert!(
        admin.ok(&["user", "ls"]).contains("web-made\tpublisher"),
        "{users}"
    );
    let (_, users) = htmx_post(
        &a,
        &format!("{url}admin/users/web-made/delete"),
        Some(&at),
        &[],
    );
    assert!(!users.contains("web-made"));
    let c = browser();
    ui_login(&c, &url, "ci");
    let page = c
        .get(format!("{url}admin/users"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(page.contains("admin role"), "{page}");
    assert!(
        !c.get(format!("{url}p/t"))
            .send()
            .unwrap()
            .text()
            .unwrap()
            .contains("name=\"layout\"")
    );
    // A password change ends every web session of that user.
    let other = browser();
    ui_login(&other, &url, "ci");
    let ct = ui_login(&c, &url, "ci");
    let (st, _) = htmx_post(
        &c,
        &format!("{url}account/password"),
        Some(&ct),
        &[("current", "ci-password"), ("new", "ci-password-2")],
    );
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        other.get(&url).send().unwrap().status(),
        StatusCode::SEE_OTHER
    );
    assert!(srv.stop().success());
}
