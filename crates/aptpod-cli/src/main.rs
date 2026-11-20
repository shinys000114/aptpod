// SPDX-License-Identifier: GPL-3.0-only

//! `aptpod`: command line client for aptpod-server.

mod account;
mod client;
mod mirror;
mod ops;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};

use crate::client::{CliError, Client, Credentials};

#[derive(Parser)]
#[command(name = "aptpod", version = aptpod_core::version(), about = "Command line client for aptpod-server")]
struct Cli {
    /// Server URL (default: the one `aptpod login` saved).
    #[arg(long, env = "APTPOD_SERVER", global = true)]
    server: Option<String>,
    /// API token (default: the one `aptpod login` saved).
    #[arg(long, env = "APTPOD_TOKEN", global = true, hide_env_values = true)]
    token: Option<String>,
    /// Print JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Logs in and saves a token in ~/.config/aptpod/credentials.toml.
    Login {
        /// User name (default: $USER).
        #[arg(short, long)]
        user: Option<String>,
        /// Read the password from the first line of stdin.
        #[arg(long)]
        password_stdin: bool,
        /// Name of the new token (default: cli@<hostname>).
        #[arg(long)]
        token_name: Option<String>,
    },
    /// Revokes the saved token and forgets it.
    Logout,
    /// Shows who the token belongs to.
    Whoami,
    /// Publishes (repository trees).
    #[command(subcommand)]
    Publish(PublishCmd),
    /// Adds packages (files or http(s) URLs) to a component.
    Add {
        alias: String,
        component: String,
        #[arg(required = true)]
        files: Vec<String>,
        /// Target dists (repeatable or comma-separated; default: from the
        /// `~<dist>` version suffix).
        #[arg(short, long, value_delimiter = ',')]
        dist: Vec<String>,
        /// Every dist that has the component.
        #[arg(long, conflicts_with = "dist")]
        all_dists: bool,
        /// Replace a pool file that has different content under the same name.
        #[arg(long)]
        replace: bool,
        /// sha256sum-style file (or URL) listing every file by its base
        /// name; uploads that do not match are refused before any change.
        #[arg(long)]
        checksums: Option<String>,
        #[command(flatten)]
        commit: CommitArgs,
    },
    /// Lists packages of a publish.
    Ls {
        alias: String,
        /// Package patterns: `name`, `name_version`, shell globs.
        patterns: Vec<String>,
        #[arg(short, long, value_delimiter = ',')]
        component: Vec<String>,
        #[arg(short, long, value_delimiter = ',')]
        dist: Vec<String>,
        /// Version condition, e.g. `>= 1.2`.
        #[arg(long)]
        versions: Option<String>,
        #[arg(long)]
        arch: Option<String>,
    },
    /// Recent jobs (applied changes).
    Jobs {
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Then print job events as they happen (until interrupted).
        #[arg(short, long)]
        follow: bool,
    },
    /// Mirrors of remote repositories.
    #[command(subcommand)]
    Mirror(mirror::MirrorCmd),
    /// Prints a job's log (e.g. a mirror sync).
    Log {
        job: i64,
        /// Keep printing while the job runs.
        #[arg(short, long)]
        follow: bool,
    },
    #[command(flatten)]
    Ops(ops::OpsCmd),
    #[command(flatten)]
    Account(account::AccountCmd),
}

#[derive(Subcommand)]
enum PublishCmd {
    /// Lists publishes.
    Ls,
    /// Shows a publish's layout and package counts.
    Show { alias: String },
    /// Prints a publish's layout as TOML.
    Get { alias: String },
    /// Replaces a publish's layout from a TOML file, `-` for stdin (admin).
    Set {
        alias: String,
        file: PathBuf,
        /// Allow dropping cells or architectures that hold packages.
        #[arg(long)]
        force: bool,
        /// Take over a new path that already holds a repository tree.
        #[arg(long)]
        adopt: bool,
        #[command(flatten)]
        commit: CommitArgs,
    },
    /// Edits a publish's layout in $VISUAL / $EDITOR (admin).
    Edit {
        alias: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        adopt: bool,
        #[command(flatten)]
        commit: CommitArgs,
    },
    /// Deletes a publish and its published tree (admin).
    Delete {
        alias: String,
        /// Delete although it still has packages.
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        commit: CommitArgs,
    },
    /// Creates a publish (admin).
    Create {
        alias: String,
        /// Root of the tree (`dists/`, `pool/`); must be under a
        /// publish root of the server.
        #[arg(long)]
        path: PathBuf,
        #[arg(short, long, value_delimiter = ',', required = true)]
        dist: Vec<String>,
        #[arg(short, long, value_delimiter = ',', required = true)]
        component: Vec<String>,
        #[arg(short, long, value_delimiter = ',', required = true)]
        arch: Vec<String>,
        #[arg(long, default_value = "")]
        origin: String,
        #[arg(long, default_value = "")]
        label: String,
        #[arg(long)]
        description: Option<String>,
        /// Do not sign Release files.
        #[arg(long)]
        no_sign: bool,
        /// Keep the newest N versions per package (0: all).
        #[arg(long, default_value_t = 0)]
        keep: u32,
        /// Take over a path that already holds a repository tree.
        #[arg(long)]
        adopt: bool,
        #[command(flatten)]
        commit: CommitArgs,
    },
}

#[derive(Args, Clone, Copy)]
struct CommitArgs {
    /// Apply without asking.
    #[arg(short, long)]
    yes: bool,
    /// Only show the plan.
    #[arg(short = 'n', long, conflicts_with = "yes")]
    dry_run: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("aptpod: {}", e.message);
            ExitCode::from(e.exit_code())
        }
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    let json = cli.json;
    let saved = Credentials::load()?;
    let server = cli
        .server
        .clone()
        .or_else(|| saved.as_ref().map(|c| c.server.clone()));
    // The saved token is only ever sent to the server that issued it.
    let same = |a: &str, b: &str| a.trim_end_matches('/') == b.trim_end_matches('/');
    let token = cli.token.clone().or_else(|| {
        saved
            .as_ref()
            .filter(|c| server.as_deref().is_some_and(|s| same(s, &c.server)))
            .map(|c| c.token.clone())
    });
    let need_server = || {
        server.clone().ok_or_else(|| {
            CliError::usage("no server: pass --server URL or run `aptpod login --server URL`")
        })
    };

    match cli.command {
        Cmd::Login {
            user,
            password_stdin,
            token_name,
        } => {
            let server = need_server()?;
            let user = user
                .or_else(|| std::env::var("USER").ok())
                .ok_or_else(|| CliError::usage("pass --user"))?;
            let password = if password_stdin {
                aptpod_core::term::read_stdin_line()
            } else {
                aptpod_core::term::read_password(&format!("Password for {user}: "))
            }
            .map_err(|e| CliError::other(format!("reading the password: {e}")))?;
            let token_name = token_name.unwrap_or_else(|| format!("cli@{}", hostname()));
            let c = Client::new(&server, None)?;
            let r: Value = c.post(
                "/api/v1/login",
                &json!({ "user": user, "password": password, "token_name": token_name }),
            )?;
            let token = r["token"].as_str().unwrap_or_default().to_string();
            let path = Credentials {
                server: server.clone(),
                token,
            }
            .save()?;
            println!(
                "logged in to {server} as {user} ({}); token saved in {}",
                r["user"]["role"].as_str().unwrap_or("?"),
                path.display()
            );
        }
        Cmd::Logout => {
            let c = Client::new(&need_server()?, token)?;
            c.delete("/api/v1/token")?;
            if let Some(p) = Credentials::remove()? {
                println!("removed {}", p.display());
            }
        }
        Cmd::Whoami => {
            let c = Client::new(&need_server()?, token)?;
            let u: Value = c.get("/api/v1/whoami", &[])?;
            if json {
                print_json(&u);
            } else {
                println!(
                    "{} ({}) at {}",
                    u["name"].as_str().unwrap_or("?"),
                    u["role"].as_str().unwrap_or("?"),
                    c.base()
                );
            }
        }
        Cmd::Publish(cmd) => publish(Client::new(&need_server()?, token)?, cmd, json)?,
        Cmd::Add {
            alias,
            component,
            files,
            dist,
            all_dists,
            replace,
            checksums,
            commit,
        } => {
            let c = Client::new(&need_server()?, token)?;
            let sums = checksums.map(|s| load_checksums(&c, &s)).transpose()?;
            let dists = if all_dists {
                json!("all")
            } else if dist.is_empty() {
                json!("detect")
            } else {
                json!(dist)
            };
            let opts = AddOpts {
                dists,
                replace,
                sums,
            };
            add(&c, &alias, &component, &files, opts, commit, json)?;
        }
        Cmd::Ls {
            alias,
            patterns,
            component,
            dist,
            versions,
            arch,
        } => {
            let c = Client::new(&need_server()?, token)?;
            let mut q = Vec::new();
            if !patterns.is_empty() {
                q.push(("q", patterns.join(",")));
            }
            if !component.is_empty() {
                q.push(("component", component.join(",")));
            }
            if !dist.is_empty() {
                q.push(("dist", dist.join(",")));
            }
            if let Some(v) = versions {
                q.push(("versions", v));
            }
            if let Some(a) = arch {
                q.push(("arch", a));
            }
            let rows: Vec<Value> = c.get(&format!("/api/v1/publishes/{alias}/packages"), &q)?;
            if json {
                print_json(&Value::Array(rows));
            } else {
                let cols = ["dist", "component", "name", "version", "arch"];
                print_table(&cols, &rows);
            }
        }
        Cmd::Ops(cmd) => ops::run(&Client::new(&need_server()?, token)?, cmd, json)?,
        Cmd::Mirror(cmd) => mirror::run(&Client::new(&need_server()?, token)?, cmd, json)?,
        Cmd::Log { job, follow } => {
            mirror::follow(&Client::new(&need_server()?, token)?, job, follow)?;
        }
        Cmd::Account(cmd) => account::run(&Client::new(&need_server()?, token)?, cmd, json)?,
        Cmd::Jobs { limit, follow } => {
            let c = Client::new(&need_server()?, token)?;
            // Subscribe first, so nothing between the listing and the
            // stream is missed.
            let events = follow.then(|| c.events()).transpose()?;
            let jobs: Vec<Value> = c.get("/api/v1/jobs", &[("limit", limit.to_string())])?;
            if json {
                print_json(&Value::Array(jobs));
            } else {
                for j in &jobs {
                    let text = j["error"]
                        .as_str()
                        .map(|e| format!("error: {e}"))
                        .unwrap_or_else(|| j["summary"].as_str().unwrap_or("").to_string());
                    println!(
                        "#{} {} {} {} {} {}",
                        j["id"],
                        fmt_time(j["created"].as_i64()),
                        j["state"].as_str().unwrap_or(""),
                        j["kind"].as_str().unwrap_or(""),
                        j["resource"].as_str().unwrap_or(""),
                        text
                    );
                }
            }
            if let Some(events) = events {
                for ev in events {
                    let ev = ev?;
                    if json {
                        println!("{ev}");
                        continue;
                    }
                    let text = ev["error"]
                        .as_str()
                        .map(|e| format!("error: {e}"))
                        .or_else(|| ev["summary"].as_str().map(String::from))
                        .unwrap_or_default();
                    println!(
                        "#{} {} {} {} {} {}",
                        ev["id"],
                        ev["state"].as_str().unwrap_or(""),
                        ev["kind"].as_str().unwrap_or(""),
                        ev["resource"].as_str().unwrap_or(""),
                        ev["user"].as_str().unwrap_or(""),
                        text
                    );
                }
            }
        }
    }
    Ok(())
}

fn publish(c: Client, cmd: PublishCmd, json: bool) -> Result<(), CliError> {
    match cmd {
        PublishCmd::Ls => {
            let list: Vec<Value> = c.get("/api/v1/publishes", &[])?;
            if json {
                print_json(&Value::Array(list));
            } else {
                for p in &list {
                    let l = &p["layout"];
                    println!(
                        "{}\t{}\t{}{}",
                        p["alias"].as_str().unwrap_or(""),
                        l["path"].as_str().unwrap_or(""),
                        join(&l["dists"]),
                        if p["managed"].as_bool() == Some(true) {
                            "\t(config file)"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
        PublishCmd::Show { alias } => {
            let p: Value = c.get(&format!("/api/v1/publishes/{alias}"), &[])?;
            if json {
                print_json(&p);
            } else {
                let l = &p["layout"];
                println!("path:          {}", l["path"].as_str().unwrap_or(""));
                println!("dists:         {}", join(&l["dists"]));
                println!("components:    {}", join(&l["components"]));
                println!("architectures: {}", join(&l["architectures"]));
                println!("signed:        {}", l["sign"].as_bool().unwrap_or(true));
                println!("cells:");
                for cell in p["cells"].as_array().into_iter().flatten() {
                    println!(
                        "  {}/{}: {} package(s)",
                        cell["dist"].as_str().unwrap_or(""),
                        cell["component"].as_str().unwrap_or(""),
                        cell["packages"]
                    );
                }
            }
        }
        PublishCmd::Get { alias } => {
            let p: Value = c.get(&format!("/api/v1/publishes/{alias}"), &[])?;
            print!("{}", layout_toml(&p["layout"])?);
        }
        PublishCmd::Set {
            alias,
            file,
            force,
            adopt,
            commit,
        } => {
            let text = if file.as_os_str() == "-" {
                std::io::read_to_string(std::io::stdin())
            } else {
                std::fs::read_to_string(&file)
            }
            .map_err(|e| CliError::other(format!("{}: {e}", file.display())))?;
            let layout = parse_layout(&text, &file.display().to_string())?;
            let body = json!({ "layout": layout, "force": force, "adopt": adopt });
            preview_apply(
                &c,
                &format!("/api/v1/publishes/{alias}/layout"),
                body,
                commit,
                json,
            )?;
        }
        PublishCmd::Edit {
            alias,
            force,
            adopt,
            commit,
        } => {
            let p: Value = c.get(&format!("/api/v1/publishes/{alias}"), &[])?;
            let before = layout_toml(&p["layout"])?;
            let after = edit_text(&before)?;
            if after == before {
                println!("no changes");
                return Ok(());
            }
            let layout = parse_layout(&after, "the edited layout")?;
            let body = json!({ "layout": layout, "force": force, "adopt": adopt });
            preview_apply(
                &c,
                &format!("/api/v1/publishes/{alias}/layout"),
                body,
                commit,
                json,
            )?;
        }
        PublishCmd::Delete {
            alias,
            force,
            commit,
        } => {
            let body = json!({ "force": force });
            preview_apply(
                &c,
                &format!("/api/v1/publishes/{alias}/delete"),
                body,
                commit,
                json,
            )?;
        }
        PublishCmd::Create {
            alias,
            path,
            dist,
            component,
            arch,
            origin,
            label,
            description,
            no_sign,
            keep,
            adopt,
            commit,
        } => {
            let mut layout = json!({
                "path": path,
                "dists": dist,
                "components": component,
                "architectures": arch,
                "origin": origin,
                "label": label,
                "sign": !no_sign,
                "keep": keep,
            });
            if let Some(d) = description {
                layout["description"] = json!(d);
            }
            let body = json!({ "alias": alias, "layout": layout, "adopt": adopt });
            preview_apply(&c, "/api/v1/publishes", body, commit, json)?;
        }
    }
    Ok(())
}

struct AddOpts {
    dists: Value,
    replace: bool,
    /// base name -> sha256
    sums: Option<BTreeMap<String, String>>,
}

/// Reads `sha256sum` output: `<hex>  <name>` or `<hex> *<name>`.
fn parse_checksums(text: &str, what: &str) -> Result<BTreeMap<String, String>, CliError> {
    let mut out = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || CliError::usage(format!("{what}:{}: not a sha256sum line", i + 1));
        let (hash, name) = line.split_once(char::is_whitespace).ok_or_else(bad)?;
        let name = name.trim_start().trim_start_matches('*');
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || name.is_empty() {
            return Err(bad());
        }
        let base = name.rsplit('/').next().unwrap_or(name);
        out.insert(base.to_string(), hash.to_ascii_lowercase());
    }
    Ok(out)
}

fn load_checksums(c: &Client, src: &str) -> Result<BTreeMap<String, String>, CliError> {
    let text = if src.starts_with("http://") || src.starts_with("https://") {
        let dir = tempfile::tempdir().map_err(|e| CliError::other(e.to_string()))?;
        let p = c.download(src, dir.path())?;
        std::fs::read_to_string(p)
    } else {
        std::fs::read_to_string(src)
    }
    .map_err(|e| CliError::other(format!("{src}: {e}")))?;
    parse_checksums(&text, src)
}

fn base_name(s: &str) -> &str {
    let s = s.split(['?', '#']).next().unwrap_or(s);
    s.rsplit('/').next().unwrap_or(s)
}

fn add(
    c: &Client,
    alias: &str,
    component: &str,
    files: &[String],
    opts: AddOpts,
    commit: CommitArgs,
    json: bool,
) -> Result<(), CliError> {
    if let Some(sums) = &opts.sums {
        let missing: Vec<&str> = files
            .iter()
            .map(|f| base_name(f))
            .filter(|b| !sums.contains_key(*b))
            .collect();
        if !missing.is_empty() {
            return Err(CliError::usage(format!(
                "not in the checksums: {}",
                missing.join(", ")
            )));
        }
    }
    // URLs are fetched here, not by the server: it never makes requests on
    // a client's behalf.
    let tmp = tempfile::tempdir().map_err(|e| CliError::other(format!("temporary dir: {e}")))?;
    let mut local: Vec<(PathBuf, String)> = Vec::new();
    let mut seen = BTreeSet::new();
    for f in files {
        let (path, name) = if f.starts_with("http://") || f.starts_with("https://") {
            let p = c.download(f, tmp.path())?;
            (p, f.clone())
        } else {
            let p = PathBuf::from(f);
            if !p.is_file() {
                return Err(CliError::usage(format!("{f}: no such file")));
            }
            (p, f.clone())
        };
        if !seen.insert(name.clone()) {
            continue;
        }
        local.push((path, name));
    }
    let mut ids = Vec::new();
    let mut names = Vec::new();
    for (path, name) in &local {
        let up: Value = c.upload(path)?;
        if let Some(sums) = &opts.sums {
            let want = &sums[base_name(name)];
            let got = up["sha256"].as_str().unwrap_or_default();
            if got != want {
                return Err(CliError {
                    kind: "conflict".into(),
                    message: format!("{name}: sha256 {got} does not match the checksums ({want})"),
                });
            }
        }
        ids.push(up["id"].as_str().unwrap_or_default().to_string());
        names.push(name.clone());
    }
    let body = json!({
        "component": component,
        "uploads": ids,
        "names": names,
        "dists": opts.dists,
        "replace": opts.replace,
    });
    preview_apply(
        c,
        &format!("/api/v1/publishes/{alias}/add"),
        body,
        commit,
        json,
    )
}

/// Previews a change, asks, then applies it bound to the previewed heads.
fn preview_apply(
    c: &Client,
    path: &str,
    mut body: Value,
    commit: CommitArgs,
    json: bool,
) -> Result<(), CliError> {
    let preview: Value = c.post(path, &body)?;
    let plan = &preview["plan"];
    if !json || commit.dry_run {
        if json {
            print_json(&preview);
        } else {
            print_plan(plan);
        }
    }
    if plan["empty"].as_bool() == Some(true) {
        if !json {
            println!("nothing to do");
        }
        return Ok(());
    }
    if commit.dry_run {
        return Ok(());
    }
    if !commit.yes {
        let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
        if !interactive {
            return Err(CliError::usage(
                "not applied: no terminal to confirm on; pass --yes (or --dry-run)",
            ));
        }
        if !aptpod_core::term::confirm("Apply?") {
            return Err(CliError::other("not applied"));
        }
    }
    body["apply"] = json!(true);
    body["expect"] = plan["bases"].clone();
    let out: Value = c.post(path, &body)?;
    if json {
        print_json(&out);
    } else {
        print_applied(&out["applied"]);
    }
    Ok(())
}

fn layout_toml(layout: &Value) -> Result<String, CliError> {
    toml::to_string_pretty(layout).map_err(|e| CliError::other(format!("layout as TOML: {e}")))
}

fn parse_layout(text: &str, what: &str) -> Result<Value, CliError> {
    toml::from_str(text).map_err(|e| CliError::usage(format!("{what}: {e}")))
}

/// Opens `text` in the user's editor and returns the result.
fn edit_text(text: &str) -> Result<String, CliError> {
    let fail = |e: std::io::Error| CliError::other(format!("editing: {e}"));
    let file = tempfile::Builder::new()
        .prefix("aptpod-layout-")
        .suffix(".toml")
        .tempfile()
        .map_err(fail)?;
    std::fs::write(file.path(), text).map_err(fail)?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    // Through the shell, so EDITOR may carry arguments ("code -w").
    let st = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(file.path())
        .status()
        .map_err(fail)?;
    if !st.success() {
        return Err(CliError::other(format!(
            "{editor} failed ({st}); nothing changed"
        )));
    }
    std::fs::read_to_string(file.path()).map_err(fail)
}

fn print_plan(plan: &Value) {
    for w in plan["warnings"].as_array().into_iter().flatten() {
        eprintln!("warning: {}", w.as_str().unwrap_or(""));
    }
    for l in plan["lines"].as_array().into_iter().flatten() {
        println!("  {}", l.as_str().unwrap_or(""));
    }
}

fn print_applied(a: &Value) {
    for r in a["revisions"].as_array().into_iter().flatten() {
        println!(
            "{} is now at revision #{}",
            r[0].as_str().unwrap_or(""),
            r[1]
        );
    }
    for p in a["published"].as_array().into_iter().flatten() {
        println!(
            "published {} {}",
            p[0].as_str().unwrap_or(""),
            p[1].as_str().unwrap_or("")
        );
    }
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn join(v: &Value) -> String {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(",")
}

fn print_table(cols: &[&str], rows: &[Value]) {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            cols.iter()
                .map(|c| {
                    r[*c]
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| r[*c].to_string())
                })
                .collect()
        })
        .collect();
    let widths: Vec<usize> = (0..cols.len())
        .map(|i| cells.iter().map(|r| r[i].len()).max().unwrap_or(0))
        .collect();
    for r in &cells {
        let line: Vec<String> = r
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
}

fn fmt_time(t: Option<i64>) -> String {
    t.and_then(|t| time::OffsetDateTime::from_unix_timestamp(t).ok())
        .and_then(|t| {
            t.format(time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]:[second]Z"
            ))
            .ok()
        })
        .unwrap_or_default()
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[test]
    fn checksums() {
        let h = "a".repeat(64);
        let sums = super::parse_checksums(
            &format!(
                "# comment\n{h}  dir/x_1_amd64.deb\n{}  *y.deb\n",
                "B".repeat(64)
            ),
            "SUMS",
        )
        .unwrap();
        assert_eq!(sums["x_1_amd64.deb"], h);
        assert_eq!(sums["y.deb"], "b".repeat(64));
        assert!(super::parse_checksums("abc  x.deb", "SUMS").is_err());
        assert_eq!(super::base_name("https://h/p/x.deb?sig=1"), "x.deb");
    }

    #[test]
    fn cli_definition_is_valid() {
        super::Cli::command().debug_assert();
    }
}
