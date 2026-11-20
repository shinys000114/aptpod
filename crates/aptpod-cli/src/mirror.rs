// SPDX-License-Identifier: GPL-3.0-only

//! `aptpod mirror ...` and `aptpod log`.

use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;
use serde_json::{Value, json};

use crate::client::{CliError, Client};
use crate::{edit_text, fmt_time, print_json};

// Parsed once per run; the size of the variants does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum MirrorCmd {
    /// Mirrors and their last sync.
    Ls,
    /// A mirror's definition and status.
    Show { name: String },
    /// Prints a mirror's definition as TOML.
    Get { name: String },
    /// Defines a mirror (admin): from flags, or a TOML file with --from.
    Create {
        name: String,
        /// TOML definition (as `mirror get` prints it), `-` for stdin.
        #[arg(long, conflicts_with_all = ["url", "path", "dist"])]
        from: Option<PathBuf>,
        /// Archive root (holding `dists/` and `pool/`).
        #[arg(long, required_unless_present = "from")]
        url: Option<String>,
        /// Destination, under one of the server's mirror_roots.
        #[arg(long, required_unless_present = "from")]
        path: Option<PathBuf>,
        #[arg(short, long, value_delimiter = ',', required_unless_present = "from")]
        dist: Vec<String>,
        /// Default: every component the Release lists.
        #[arg(short, long, value_delimiter = ',')]
        component: Vec<String>,
        /// Default: every architecture the Release lists.
        #[arg(short, long, value_delimiter = ',')]
        arch: Vec<String>,
        /// The upstream's public key (armored or binary file).
        #[arg(long, conflicts_with = "insecure")]
        keyring: Option<PathBuf>,
        /// Do not check upstream signatures.
        #[arg(long)]
        insecure: bool,
        #[arg(long)]
        udebs: bool,
        #[arg(long)]
        sources: bool,
        /// More index files to keep, e.g. '*/i18n/*' (repeatable).
        #[arg(long)]
        extras: Vec<String>,
        /// File listing pool paths upstream lists but does not serve.
        #[arg(long)]
        ignore_missing: Option<PathBuf>,
        #[arg(long)]
        parallel: Option<usize>,
        /// Keep files upstream dropped.
        #[arg(long)]
        no_prune: bool,
        #[arg(long)]
        max_delete: Option<usize>,
        /// Mirror into a directory that already holds files (prune off).
        #[arg(long)]
        adopt: bool,
    },
    /// Replaces a mirror's definition from TOML, `-` for stdin (admin).
    Set {
        name: String,
        file: PathBuf,
        #[arg(long)]
        adopt: bool,
    },
    /// Edits a mirror's definition in $VISUAL / $EDITOR (admin).
    Edit {
        name: String,
        #[arg(long)]
        adopt: bool,
    },
    /// Forgets a mirror; its tree stays on disk (admin).
    Delete { name: String },
    /// Starts a sync.
    Sync {
        name: String,
        /// Delete unreferenced pool files this once, even with prune off.
        #[arg(long)]
        prune: bool,
        /// Re-hash every file already present.
        #[arg(long)]
        verify_all: bool,
        /// Follow the log until the sync ends; exit 1 unless it succeeded.
        #[arg(short, long)]
        wait: bool,
    },
    /// Stops a running sync.
    Stop { name: String },
}

fn read_input(file: &PathBuf) -> Result<String, CliError> {
    if file.as_os_str() == "-" {
        std::io::read_to_string(std::io::stdin())
    } else {
        std::fs::read_to_string(file)
    }
    .map_err(|e| CliError::other(format!("{}: {e}", file.display())))
}

fn def_toml(view: &Value) -> Result<String, CliError> {
    let mut def = view.clone();
    if let Some(o) = def.as_object_mut() {
        for k in ["name", "status", "running"] {
            o.remove(k);
        }
        o.retain(|_, v| !v.is_null());
    }
    toml::to_string(&def).map_err(|e| CliError::other(format!("definition as TOML: {e}")))
}

fn parse_def(text: &str, what: &str) -> Result<Value, CliError> {
    toml::from_str(text).map_err(|e| CliError::usage(format!("{what}: {e}")))
}

fn note(r: &Value) {
    if let Some(n) = r["note"].as_str() {
        eprintln!("note: {n}");
    }
    if let Some(p) = r["old_path_left"].as_str() {
        eprintln!("note: the previous tree at {p} was left in place");
    }
}

pub fn run(c: &Client, cmd: MirrorCmd, json: bool) -> Result<(), CliError> {
    match cmd {
        MirrorCmd::Ls => {
            let list: Vec<Value> = c.get("/api/v1/mirrors", &[])?;
            if json {
                print_json(&Value::Array(list));
                return Ok(());
            }
            for m in &list {
                let st = &m["status"];
                let state = if m["running"].is_i64() {
                    format!("syncing (job {})", m["running"])
                } else {
                    st["state"].as_str().unwrap_or("never synced").to_string()
                };
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    m["name"].as_str().unwrap_or(""),
                    m["path"].as_str().unwrap_or(""),
                    state,
                    fmt_time(st["finished"].as_i64()),
                    st["summary"].as_str().unwrap_or("")
                );
            }
        }
        MirrorCmd::Show { name } => {
            let m: Value = c.get(&format!("/api/v1/mirrors/{name}"), &[])?;
            if json {
                print_json(&m);
                return Ok(());
            }
            print!("{}", def_toml(&m)?);
            let st = &m["status"];
            println!("# last sync: {}", st["state"].as_str().unwrap_or("never"));
            if let Some(j) = st["job"].as_i64() {
                println!(
                    "#   job {j}, {} .. {}: {}",
                    fmt_time(st["started"].as_i64()),
                    fmt_time(st["finished"].as_i64()),
                    st["summary"].as_str().unwrap_or("")
                );
            }
            if m["running"].is_i64() {
                println!("# syncing now: job {}", m["running"]);
            }
        }
        MirrorCmd::Get { name } => {
            let m: Value = c.get(&format!("/api/v1/mirrors/{name}"), &[])?;
            print!("{}", def_toml(&m)?);
        }
        MirrorCmd::Create {
            name,
            from,
            url,
            path,
            dist,
            component,
            arch,
            keyring,
            insecure,
            udebs,
            sources,
            extras,
            ignore_missing,
            parallel,
            no_prune,
            max_delete,
            adopt,
        } => {
            let def = if let Some(f) = from {
                parse_def(&read_input(&f)?, &f.display().to_string())?
            } else {
                let key = keyring
                    .map(|k| {
                        let data = std::fs::read(&k)
                            .map_err(|e| CliError::other(format!("{}: {e}", k.display())))?;
                        armor(&data).map_err(|e| CliError::usage(format!("{}: {e}", k.display())))
                    })
                    .transpose()?;
                let ignore: Vec<String> = match ignore_missing {
                    Some(f) => read_input(&f)?
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty() && !l.starts_with('#'))
                        .map(String::from)
                        .collect(),
                    None => vec![],
                };
                let mut d = json!({
                    "url": url, "path": path, "dists": dist,
                    "components": component, "architectures": arch,
                    "udebs": udebs, "sources": sources, "extras": extras,
                    "keyring": key, "insecure": insecure, "ignore_missing": ignore,
                    "prune": !no_prune,
                });
                if let Some(p) = parallel {
                    d["parallel"] = json!(p);
                }
                if let Some(m) = max_delete {
                    d["max_delete"] = json!(m);
                }
                d.as_object_mut().unwrap().retain(|_, v| !v.is_null());
                d
            };
            let r: Value = c.post(
                "/api/v1/mirrors",
                &json!({ "name": name, "def": def, "adopt": adopt }),
            )?;
            note(&r);
            println!("mirror {name} defined; start it with `aptpod mirror sync {name}`");
        }
        MirrorCmd::Set { name, file, adopt } => {
            let def = parse_def(&read_input(&file)?, &file.display().to_string())?;
            let r: Value = c.post(
                &format!("/api/v1/mirrors/{name}"),
                &json!({ "def": def, "adopt": adopt }),
            )?;
            note(&r);
        }
        MirrorCmd::Edit { name, adopt } => {
            let m: Value = c.get(&format!("/api/v1/mirrors/{name}"), &[])?;
            let before = def_toml(&m)?;
            let after = edit_text(&before)?;
            if after == before {
                println!("no changes");
                return Ok(());
            }
            let def = parse_def(&after, "the edited definition")?;
            let r: Value = c.post(
                &format!("/api/v1/mirrors/{name}"),
                &json!({ "def": def, "adopt": adopt }),
            )?;
            note(&r);
        }
        MirrorCmd::Delete { name } => {
            let r: Value = c.delete_json(&format!("/api/v1/mirrors/{name}"))?;
            println!(
                "mirror {name} forgotten; its tree is still at {}",
                r["path_left"].as_str().unwrap_or("?")
            );
        }
        MirrorCmd::Sync {
            name,
            prune,
            verify_all,
            wait,
        } => {
            let r: Value = c.post(
                &format!("/api/v1/mirrors/{name}/sync"),
                &json!({ "prune": prune, "verify_all": verify_all }),
            )?;
            let job = r["job"].as_i64().unwrap_or_default();
            if !wait {
                println!("job {job} started; follow it with `aptpod log {job} -f`");
                return Ok(());
            }
            let state = follow(c, job, true)?;
            if state != "done" {
                return Err(CliError::other(format!("job {job} {state}")));
            }
        }
        MirrorCmd::Stop { name } => {
            let r: Value = c.post(&format!("/api/v1/mirrors/{name}/stop"), &json!({}))?;
            println!("asked job {} to stop", r["job"]);
        }
    }
    Ok(())
}

/// Prints a job's log, following it while it runs; returns its final state.
pub fn follow(c: &Client, job: i64, keep_following: bool) -> Result<String, CliError> {
    let mut offset = 0u64;
    loop {
        let r: Value = c.get(
            &format!("/api/v1/jobs/{job}/log"),
            &[("offset", offset.to_string())],
        )?;
        let text = r["text"].as_str().unwrap_or("");
        print!("{text}");
        offset = r["offset"].as_u64().unwrap_or(offset);
        let running = r["running"].as_bool() == Some(true);
        if (!running || !keep_following) && text.is_empty() {
            return Ok(r["state"].as_str().unwrap_or("?").to_string());
        }
        if text.is_empty() {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// Keys as given if armored, else armored from binary.
fn armor(data: &[u8]) -> Result<String, String> {
    if let Ok(s) = std::str::from_utf8(data)
        && s.contains("-----BEGIN PGP PUBLIC KEY BLOCK-----")
    {
        return Ok(s.to_string());
    }
    // Binary keyrings (e.g. /usr/share/keyrings/*.gpg) as base64 armor.
    Ok(format!(
        "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\n{}\n-----END PGP PUBLIC KEY BLOCK-----\n",
        base64(data)
    ))
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for (i, chunk) in data.chunks(3).enumerate() {
        if i > 0 && i % 16 == 0 {
            out.push('\n');
        }
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(T[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
