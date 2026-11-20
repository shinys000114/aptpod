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
