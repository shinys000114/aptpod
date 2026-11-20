// SPDX-License-Identifier: GPL-3.0-only

//! Commands beyond add/ls: rm, promote, prune, rollback, history, diff,
//! snapshots, find, gc, repair.

use clap::Subcommand;
use serde_json::{Value, json};

use crate::client::{CliError, Client};
use crate::{CommitArgs, fmt_time, preview_apply, print_json, print_table};

#[derive(Subcommand)]
pub enum OpsCmd {
    /// Removes packages from a component (all dists unless --dist).
    Rm {
        alias: String,
        component: String,
        /// `name`, `name=version`, shell globs.
        patterns: Vec<String>,
        /// Every package (when no pattern is given).
        #[arg(long)]
        all: bool,
        #[arg(short, long, value_delimiter = ',')]
        dist: Vec<String>,
        /// Only versions beyond the newest N per name and arch.
        #[arg(long)]
        keep: Option<u32>,
        /// Version condition, e.g. `<< 2.0`.
        #[arg(long)]
        versions: Option<String>,
        #[arg(long)]
        arch: Option<String>,
        #[command(flatten)]
        commit: CommitArgs,
    },
    /// Copies (or moves) packages to another dist, component or publish.
    Promote {
        alias: String,
        component: String,
        patterns: Vec<String>,
        #[arg(long)]
        all: bool,
        /// Source dists (default: all).
        #[arg(short, long, value_delimiter = ',')]
        dist: Vec<String>,
        #[arg(long)]
        to_publish: Option<String>,
        #[arg(long)]
        to_component: Option<String>,
        /// Target dist (with one source dist).
        #[arg(long)]
        to_dist: Option<String>,
        /// Every matching version, not only the newest.
        #[arg(long)]
        all_versions: bool,
        /// Remove from the source.
        #[arg(long = "move")]
        move_: bool,
        #[arg(long)]
        versions: Option<String>,
        #[arg(long)]
        arch: Option<String>,
        #[command(flatten)]
        commit: CommitArgs,
    },
    /// Keeps only the newest N versions of each package.
    Prune {
        alias: String,
        patterns: Vec<String>,
        #[arg(short, long)]
        component: Option<String>,
        #[arg(short, long, value_delimiter = ',')]
        dist: Vec<String>,
        /// Default: the publish's `keep`.
        #[arg(long)]
        keep: Option<u32>,
        #[command(flatten)]
        commit: CommitArgs,
    },
    /// Restores the packages of a revision (`#N`) or snapshot label.
    Rollback {
        alias: String,
        to: String,
        #[command(flatten)]
        commit: CommitArgs,
    },
    /// Revisions of a publish, newest first.
    History {
        alias: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// Package differences between two revisions or labels.
    Diff {
        alias: String,
        from: String,
        /// Default: HEAD.
        to: Option<String>,
    },
    /// Snapshot labels.
    #[command(subcommand)]
    Snapshot(SnapshotCmd),
    /// Searches packages across publishes.
    Find {
        patterns: Vec<String>,
        #[arg(short, long, value_delimiter = ',')]
        publish: Vec<String>,
        /// Only the newest version per cell, name and arch.
        #[arg(long)]
        newest: bool,
        #[arg(long)]
        versions: Option<String>,
        #[arg(long)]
        arch: Option<String>,
    },
    /// All publish layouts as one TOML file.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Forgets expired revisions and removes unreferenced packages (admin).
    Gc,
    /// Rewrites published trees that are behind the database (admin).
    Repair,
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Prints every publish layout as TOML (`[publishes.<alias>]`).
    Export,
    /// Creates and updates publishes from a TOML file, `-` for stdin
    /// (admin). Publishes missing from the file are left alone.
    Import {
        file: std::path::PathBuf,
        /// Allow dropping cells or architectures that hold packages.
        #[arg(long)]
        force: bool,
        /// Take over paths that already hold repository trees.
        #[arg(long)]
        adopt: bool,
        #[command(flatten)]
        commit: CommitArgs,
    },
}

#[derive(Subcommand)]
pub enum SnapshotCmd {
    Ls {
        alias: String,
    },
    /// Labels a revision (default: HEAD).
    Create {
        alias: String,
        label: String,
        #[arg(short, long, default_value = "")]
        message: String,
        /// `#N`, another label or HEAD.
        #[arg(long)]
        at: Option<String>,
    },
    Drop {
        alias: String,
        label: String,
    },
    /// Drops old labels.
    Prune {
        alias: String,
        /// Only labels older than this (e.g. 90d, 12h, 3600).
        #[arg(long, value_parser = parse_age)]
        older_than: Option<i64>,
        /// Always keep the newest N.
        #[arg(long, default_value_t = 0)]
        keep_last: usize,
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
}

/// `90d`, `12h`, `30m`, `45s` or plain seconds.
pub fn parse_age(s: &str) -> Result<i64, String> {
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: i64 = num.parse().map_err(|_| format!("bad age {s:?}"))?;
    let mul = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 7 * 86400,
        _ => return Err(format!("bad age {s:?} (use s, m, h, d or w)")),
    };
    Ok(n * mul)
}

fn opt_list(v: &[String]) -> Value {
    if v.is_empty() { Value::Null } else { json!(v) }
}

/// Drops nulls so the server's defaults apply.
fn clean(mut v: Value) -> Value {
    if let Some(m) = v.as_object_mut() {
        m.retain(|_, x| !x.is_null());
    }
    v
}

pub fn run(c: &Client, cmd: OpsCmd, json: bool) -> Result<(), CliError> {
    match cmd {
        OpsCmd::Rm {
            alias,
            component,
            patterns,
            all,
            dist,
            keep,
            versions,
            arch,
            commit,
        } => {
            let body = clean(json!({
                "component": component, "packages": patterns, "all": all,
                "dists": opt_list(&dist), "keep": keep, "versions": versions, "arch": arch,
            }));
            preview_apply(
                c,
                &format!("/api/v1/publishes/{alias}/rm"),
                body,
                commit,
                json,
            )
        }
        OpsCmd::Promote {
            alias,
            component,
            patterns,
            all,
            dist,
            to_publish,
            to_component,
            to_dist,
            all_versions,
            move_,
            versions,
            arch,
            commit,
        } => {
            let body = clean(json!({
                "component": component, "packages": patterns, "all": all,
                "dists": opt_list(&dist), "to_publish": to_publish,
                "to_component": to_component, "to_dist": to_dist,
                "all_versions": all_versions, "move": move_,
                "versions": versions, "arch": arch,
            }));
            preview_apply(
                c,
                &format!("/api/v1/publishes/{alias}/promote"),
                body,
                commit,
                json,
            )
        }
        OpsCmd::Prune {
            alias,
            patterns,
            component,
            dist,
            keep,
            commit,
        } => {
            let body = clean(json!({
                "component": component, "packages": patterns,
                "dists": opt_list(&dist), "keep": keep,
            }));
            preview_apply(
                c,
                &format!("/api/v1/publishes/{alias}/prune"),
                body,
                commit,
                json,
            )
        }
        OpsCmd::Rollback { alias, to, commit } => preview_apply(
            c,
            &format!("/api/v1/publishes/{alias}/rollback"),
            json!({ "to": to }),
            commit,
            json,
        ),
        OpsCmd::History { alias, limit } => {
            let revs: Vec<Value> = c.get(
                &format!("/api/v1/publishes/{alias}/history"),
                &[("limit", limit.to_string())],
            )?;
            if json {
                {
                    print_json(&Value::Array(revs));
                    return Ok(());
                }
            }
            for r in &revs {
                let labels: Vec<&str> = r["labels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                println!(
                    "#{:<5} {} {:<10} {}{}{}",
                    r["id"],
                    fmt_time(r["created"].as_i64()),
                    r["author"].as_str().unwrap_or(""),
                    r["message"].as_str().unwrap_or(""),
                    if labels.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", labels.join(", "))
                    },
                    if r["head"].as_bool() == Some(true) {
                        " (HEAD)"
                    } else {
                        ""
                    },
                );
            }
            Ok(())
        }
        OpsCmd::Diff { alias, from, to } => {
            let mut q = vec![("from", from)];
            if let Some(t) = to {
                q.push(("to", t));
            }
            let entries: Vec<Value> = c.get(&format!("/api/v1/publishes/{alias}/diff"), &q)?;
            if json {
                {
                    print_json(&Value::Array(entries));
                    return Ok(());
                }
            }
            for e in &entries {
                println!(
                    "{} {}/{} {} {} {}",
                    e["change"].as_str().unwrap_or("?"),
                    e["dist"].as_str().unwrap_or(""),
                    e["component"].as_str().unwrap_or(""),
                    e["name"].as_str().unwrap_or(""),
                    e["version"].as_str().unwrap_or(""),
                    e["arch"].as_str().unwrap_or(""),
                );
            }
            Ok(())
        }
        OpsCmd::Snapshot(cmd) => snapshot(c, cmd, json),
        OpsCmd::Find {
            patterns,
            publish,
            newest,
            versions,
            arch,
        } => {
            let mut q = vec![("newest", newest.to_string())];
            if !patterns.is_empty() {
                q.push(("q", patterns.join(",")));
            }
            if !publish.is_empty() {
                q.push(("publish", publish.join(",")));
            }
            if let Some(v) = versions {
                q.push(("versions", v));
            }
            if let Some(a) = arch {
                q.push(("arch", a));
            }
            let rows: Vec<Value> = c.get("/api/v1/packages", &q)?;
            if json {
                print_json(&Value::Array(rows));
            } else {
                print_table(
                    &["alias", "dist", "component", "name", "version", "arch"],
                    &rows,
                );
            }
            Ok(())
        }
        OpsCmd::Config(ConfigCmd::Export) => {
            let cfg: Value = c.get("/api/v1/config", &[])?;
            if json {
                print_json(&cfg);
            } else {
                let text = toml::to_string_pretty(&cfg)
                    .map_err(|e| CliError::other(format!("config as TOML: {e}")))?;
                print!("{text}");
            }
            Ok(())
        }
        OpsCmd::Config(ConfigCmd::Import {
            file,
            force,
            adopt,
            commit,
        }) => {
            let text = if file.as_os_str() == "-" {
                std::io::read_to_string(std::io::stdin())
            } else {
                std::fs::read_to_string(&file)
            }
            .map_err(|e| CliError::other(format!("{}: {e}", file.display())))?;
            let config: Value = toml::from_str(&text)
                .map_err(|e| CliError::usage(format!("{}: {e}", file.display())))?;
            let body = json!({ "config": config, "force": force, "adopt": adopt });
            preview_apply(c, "/api/v1/config", body, commit, json)
        }
        OpsCmd::Gc => {
            let r: Value = c.post("/api/v1/gc", &json!({}))?;
            if json {
                print_json(&r);
            } else {
                let expired: u64 = r["expired"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(_, n)| n.as_u64())
                    .sum();
                println!(
                    "{expired} revision(s) expired, {} cell set(s) and {} package(s) removed",
                    r["cellsets"], r["packages"]
                );
            }
            Ok(())
        }
        OpsCmd::Repair => {
            let r: Value = c.post("/api/v1/repair", &json!({}))?;
            if json {
                print_json(&r);
            } else {
                let dists = r["published"].as_array().map_or(0, Vec::len);
                if dists == 0 {
                    println!("all published trees are up to date");
                }
                for p in r["published"].as_array().into_iter().flatten() {
                    println!(
                        "rewrote {} {}",
                        p[0].as_str().unwrap_or(""),
                        p[1].as_str().unwrap_or("")
                    );
                }
            }
            Ok(())
        }
    }
}

fn snapshot(c: &Client, cmd: SnapshotCmd, json: bool) -> Result<(), CliError> {
    let print_labels = |list: &[Value]| {
        for s in list {
            println!(
                "{}\t#{}\t{}\t{}",
                s["label"].as_str().unwrap_or(""),
                s["revision"],
                fmt_time(s["created"].as_i64()),
                s["description"].as_str().unwrap_or("")
            );
        }
    };
    match cmd {
        SnapshotCmd::Ls { alias } => {
            let list: Vec<Value> = c.get(&format!("/api/v1/publishes/{alias}/snapshots"), &[])?;
            if json {
                print_json(&Value::Array(list));
            } else {
                print_labels(&list);
            }
        }
        SnapshotCmd::Create {
            alias,
            label,
            message,
            at,
        } => {
            let r: Value = c.post(
                &format!("/api/v1/publishes/{alias}/snapshots"),
                &clean(json!({ "label": label, "description": message, "at": at })),
            )?;
            if json {
                print_json(&r);
            } else {
                println!("{label} -> #{}", r["revision"]);
            }
        }
        SnapshotCmd::Drop { alias, label } => {
            c.delete(&format!("/api/v1/publishes/{alias}/snapshots/{label}"))?;
        }
        SnapshotCmd::Prune {
            alias,
            older_than,
            keep_last,
            dry_run,
        } => {
            let list: Vec<Value> = c.post(
                &format!("/api/v1/publishes/{alias}/snapshots/prune"),
                &clean(json!({
                    "older_than": older_than, "keep_last": keep_last, "dry_run": dry_run,
                })),
            )?;
            if json {
                print_json(&Value::Array(list));
            } else {
                if dry_run {
                    println!("would drop:");
                }
                print_labels(&list);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn ages() {
        assert_eq!(super::parse_age("3600"), Ok(3600));
        assert_eq!(super::parse_age("90d"), Ok(90 * 86400));
        assert_eq!(super::parse_age("2w"), Ok(14 * 86400));
        assert!(super::parse_age("5y").is_err());
        assert!(super::parse_age("d").is_err());
    }
}
