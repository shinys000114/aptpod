// SPDX-License-Identifier: GPL-3.0-only

//! `aptpod-server`: apt repository and mirror server.

mod api;
mod authdb;
mod config;
mod configfile;
mod cron;
mod events;
mod mirror_api;
mod mirrors;
mod ops;
mod runner;
mod schedule;
mod users;
mod writer;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use aptpod_core::sign::Signer;
use aptpod_repo::Repo;
use clap::{Parser, Subcommand};

use crate::authdb::{AuthDb, Role};
use crate::config::Config;

#[derive(Parser)]
#[command(name = "aptpod-server", version = aptpod_core::version(), about = "apt repository and mirror server")]
struct Cli {
    /// Server configuration file.
    #[arg(short, long, default_value = "/etc/aptpod/server.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Runs the server (the default).
    Serve,
    /// Manages users; works while the server runs.
    #[command(subcommand)]
    User(UserCmd),
    /// Checks server.toml and the config file (or FILE) without a server;
    /// run it before `systemctl reload aptpod-server`.
    ConfigCheck {
        /// Default: config_file from server.toml.
        file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum UserCmd {
    /// Adds a user; asks for the password.
    Add {
        name: String,
        #[arg(long, default_value = "publisher")]
        role: Role,
        /// Read the password from the first line of stdin.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Sets a user's password.
    Passwd {
        name: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Changes a user's role.
    Role { name: String, role: Role },
    /// Removes a user and their tokens.
    Del { name: String },
    /// Lists users.
    Ls,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let res = Config::load(&cli.config).and_then(|cfg| match cli.command {
        None | Some(Command::Serve) => serve(cfg),
        Some(Command::User(cmd)) => user(&cfg, cmd),
        Some(Command::ConfigCheck { file }) => config_check(&cfg, file),
    });
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("aptpod-server: {e}");
            ExitCode::FAILURE
        }
    }
}

fn new_password(stdin: bool) -> Result<String, String> {
    let pw = if stdin {
        aptpod_core::term::read_stdin_line().map_err(|e| format!("reading stdin: {e}"))?
    } else {
        let a = aptpod_core::term::read_password("Password: ")
            .map_err(|e| format!("reading the password (or use --password-stdin): {e}"))?;
        let b = aptpod_core::term::read_password("Again: ").map_err(|e| e.to_string())?;
        if a != b {
            return Err("the passwords differ".into());
        }
        a
    };
    authdb::hash_password(&pw)
}

/// Refuses to touch a data directory owned by someone else: files created
/// there (e.g. by `sudo aptpod-server user add`) would be unreadable for the
/// service user.
fn check_owner(data: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(data) else {
        return Ok(());
    };
    let me = rustix::process::geteuid().as_raw();
    if meta.uid() != me {
        return Err(format!(
            "{} belongs to uid {}, not to you (uid {me}); run this as its owner, \
             e.g. `sudo -u aptpod aptpod-server ...`",
            data.display(),
            meta.uid()
        ));
    }
    Ok(())
}

fn user(cfg: &Config, cmd: UserCmd) -> Result<(), String> {
    check_owner(&cfg.data_dir)?;
    std::fs::create_dir_all(&cfg.data_dir)
        .map_err(|e| format!("{}: {e}", cfg.data_dir.display()))?;
    let db = AuthDb::open(&cfg.data_dir).map_err(|e| format!("server database: {e}"))?;
    let dberr = |e: rusqlite::Error| format!("server database: {e}");
    let missing = |name: &str| format!("no user {name:?}");
    match cmd {
        UserCmd::Add {
            name,
            role,
            password_stdin,
        } => {
            authdb::check_user_name(&name)?;
            if db.user(&name).map_err(dberr)?.is_some() {
                return Err(format!("user {name:?} already exists"));
            }
            let hash = new_password(password_stdin)?;
            db.user_add(&name, &hash, role).map_err(dberr)?;
            println!("added {name} ({})", role.as_str());
        }
        UserCmd::Passwd {
            name,
            password_stdin,
        } => {
            if db.user(&name).map_err(dberr)?.is_none() {
                return Err(missing(&name));
            }
            let hash = new_password(password_stdin)?;
            db.set_password(&name, &hash).map_err(dberr)?;
        }
        UserCmd::Role { name, role } => {
            if !db.set_role(&name, role).map_err(dberr)? {
                return Err(missing(&name));
            }
        }
        UserCmd::Del { name } => {
            if !db.user_del(&name).map_err(dberr)? {
                return Err(missing(&name));
            }
        }
        UserCmd::Ls => {
            for u in db.users().map_err(dberr)? {
                println!("{}\t{}", u.name, u.role.as_str());
            }
        }
    }
    Ok(())
}

/// Static checks of a config file: syntax, layouts, mirror definitions,
/// path rules and overlaps among its items (not against the database).
fn config_check(cfg: &Config, file: Option<PathBuf>) -> Result<(), String> {
    let Some(path) = file.or_else(|| cfg.config_file.clone()) else {
        println!("server.toml is valid; no config_file is set");
        return Ok(());
    };
    let f = configfile::FileConfig::load(&path)?;
    let mut errors = Vec::new();
    for (alias, l) in &f.publishes {
        if let Err(e) = aptpod_repo::layout::check_alias(alias) {
            errors.push(format!("publish {alias:?}: {e}"));
        }
        if let Err(e) = l.validate() {
            errors.push(format!("publish {alias:?}: {e}"));
        }
        if let Err(e) = cfg.allows_publish_path(&l.path) {
            errors.push(format!("publish {alias:?}: {e}"));
        }
    }
    for (name, d) in &f.mirrors {
        if let Err(e) = d.check(cfg) {
            errors.push(format!("mirror {name:?}: {e}"));
        }
    }
    let paths: Vec<(String, &Path)> = f
        .publishes
        .iter()
        .map(|(a, l)| (format!("publish {a:?}"), l.path.as_path()))
        .chain(
            f.mirrors
                .iter()
                .map(|(n, d)| (format!("mirror {n:?}"), d.path.as_path())),
        )
        .collect();
    for (i, (a, pa)) in paths.iter().enumerate() {
        for (b, pb) in &paths[i + 1..] {
            if aptpod_repo::layout::paths_overlap(pa, pb) {
                errors.push(format!("{a} and {b} have overlapping paths"));
            }
        }
    }
    if !errors.is_empty() {
        return Err(format!("{}:\n  {}", path.display(), errors.join("\n  ")));
    }
    println!(
        "{}: ok ({} publish(es), {} mirror(s)); `aptpod config plan` shows what a reload would change",
        path.display(),
        f.publishes.len(),
        f.mirrors.len()
    );
    Ok(())
}

/// Removes uploads nobody applied within [`aptpod_repo::history::GC_GRACE`].
fn sweep_uploads(dir: &Path) {
    let max_age = Duration::from_secs(aptpod_repo::history::GC_GRACE as u64);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() > max_age);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn serve(cfg: Config) -> Result<(), String> {
    tracing_subscriber::fmt()
        // No colour codes in the journal.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();
    let data = cfg.data_dir.clone();
    check_owner(&data)?;
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;

    // One server per data directory: the repository has a single writer.
    let lock = std::fs::File::create(data.join("lock"))
        .map_err(|e| format!("{}: {e}", data.join("lock").display()))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| format!("another aptpod-server is using {}", data.display()))?;

    let auth = AuthDb::open(&data).map_err(|e| format!("server database: {e}"))?;
    let n = auth
        .jobs_interrupted()
        .map_err(|e| format!("server database: {e}"))?;
    if n > 0 {
        tracing::warn!("{n} job(s) were interrupted by the last shutdown");
    }
    auth.mirrors_interrupted()
        .map_err(|e| format!("server database: {e}"))?;

    let mut repo = Repo::open(&data).map_err(|e| format!("repository: {e}"))?;
    repo.set_signer(cfg.signer().map(|s| Box::new(s) as Box<dyn Signer + Send>));
    match repo.repair() {
        Ok(a) if !a.published.is_empty() => {
            tracing::warn!("repaired published trees: {:?}", a.published)
        }
        Ok(_) => {}
        Err(e) => tracing::error!("repair failed: {e}; the published trees may be behind"),
    }
    let reader = Repo::open_reader(&data).map_err(|e| format!("repository: {e}"))?;

    let uploads = data.join("uploads");
    std::fs::create_dir_all(&uploads).map_err(|e| format!("{}: {e}", uploads.display()))?;
    sweep_uploads(&uploads);

    let state = Arc::new(api::AppState {
        cfg: cfg.clone(),
        auth,
        writer: writer::Writer::start(repo),
        reader: Mutex::new(reader),
        uploads: uploads.clone(),
        logins: tokio::sync::Semaphore::new(4),
        events: tokio::sync::broadcast::channel(256).0,
        stopping: tokio::sync::watch::channel(false).0,
        mirror_runner: runner::Runner::default(),
        config_file: configfile::FileState::default(),
    });
    runner::Runner::start(&state, cfg.mirror_workers);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting the runtime: {e}"))?;
    let res = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(&cfg.listen)
            .await
            .map_err(|e| format!("listening on {}: {e}", cfg.listen))?;
        tracing::info!(
            "aptpod-server {} listening on {}",
            aptpod_core::version(),
            listener
                .local_addr()
                .map_or(cfg.listen.clone(), |a| a.to_string())
        );
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(600));
            loop {
                tick.tick().await;
                let dir = uploads.clone();
                let _ = tokio::task::spawn_blocking(move || sweep_uploads(&dir)).await;
            }
        });
        if cfg.config_file.is_some() {
            apply_config_file(&state, "startup").await;
        }
        let st = state.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let Ok(mut hup) = signal(SignalKind::hangup()) else {
                return;
            };
            while hup.recv().await.is_some() {
                if st.cfg.config_file.is_none() {
                    tracing::warn!("SIGHUP: no config_file is set; nothing to reload");
                    continue;
                }
                apply_config_file(&st, "reload").await;
            }
        });
        axum::serve(listener, api::router(state.clone()))
            .with_graceful_shutdown(shutdown_signal(state.clone()))
            .await
            .map_err(|e| format!("serving: {e}"))
    });
    // Finish the job in progress before exiting; mirror syncs stop between
    // chunks and end `interrupted`.
    state.mirror_runner.shutdown();
    state.writer.shutdown();
    tracing::info!("stopped");
    drop(lock);
    res
}

/// Applies the config file and logs what happened.
async fn apply_config_file(st: &Arc<api::AppState>, why: &str) {
    match configfile::run(st, true).await {
        Ok(out) => {
            if out.plan.empty {
                tracing::info!("config file ({why}): up to date");
            }
            for l in &out.plan.lines {
                tracing::info!("config file ({why}): {l}");
            }
            for w in &out.plan.warnings {
                tracing::warn!("config file ({why}): {w}");
            }
        }
        Err(e) => tracing::error!("config file ({why}) not applied: {}", e.message()),
    }
}

async fn shutdown_signal(state: Arc<api::AppState>) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    state.stopping.send_replace(true);
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        super::Cli::command().debug_assert();
    }
}
