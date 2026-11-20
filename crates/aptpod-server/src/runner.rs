// SPDX-License-Identifier: GPL-3.0-only

//! Mirror workers: syncs do not touch the repository database, so they run
//! next to the single writer, on plain threads (the HTTP engine must not run
//! on a tokio worker). One sync per mirror at a time; every sync is a job
//! with its own log file.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use aptpod_mirror::fetch::Http;
use aptpod_mirror::http::{Ctx, SyncReport, sync};

use crate::api::{ApiError, AppState};
use crate::events::Job;
use crate::mirrors::MirrorDef;

/// One-off changes for a single sync.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncOpts {
    /// Delete unreferenced pool files this time even if `prune` is off.
    pub prune: bool,
    /// Re-hash every present file.
    pub verify_all: bool,
}

struct Task {
    name: String,
    def: MirrorDef,
    opts: SyncOpts,
    job: Job,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct Runner {
    queue: Mutex<VecDeque<Task>>,
    wake: Condvar,
    /// Mirrors queued or running: (job id, cancel flag).
    active: Mutex<HashMap<String, (i64, Arc<AtomicBool>)>>,
    stopping: AtomicBool,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Runner {
    pub fn start(st: &Arc<AppState>, workers: usize) {
        let mut threads = st.mirror_runner.threads.lock().unwrap();
        for i in 0..workers {
            let st = st.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("mirror-{i}"))
                    .spawn(move || worker(st))
                    .expect("spawning a mirror worker"),
            );
        }
    }

    /// Queues a sync; refused while one of the same mirror is queued or
    /// running. Returns the job id.
    pub fn submit(
        &self,
        st: &Arc<AppState>,
        name: &str,
        def: MirrorDef,
        user: &str,
        opts: SyncOpts,
    ) -> Result<i64, ApiError> {
        if self.stopping.load(Ordering::Relaxed) {
            return Err(crate::writer::Stopped.into());
        }
        let mut active = self.active.lock().unwrap();
        if let Some((job, _)) = active.get(name) {
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "conflict",
                format!("mirror {name:?} is already syncing (job {job})"),
            ));
        }
        let job = st.job_start("mirror-sync", &format!("mirror:{name}"), user)?;
        let id = job.id();
        st.auth.mirror_started(name, id)?;
        let cancel = Arc::new(AtomicBool::new(false));
        active.insert(name.to_string(), (id, cancel.clone()));
        drop(active);
        self.queue.lock().unwrap().push_back(Task {
            name: name.to_string(),
            def,
            opts,
            job,
            cancel,
        });
        self.wake.notify_one();
        Ok(id)
    }

    /// Asks a queued or running sync to stop; its job ends `interrupted`.
    pub fn stop(&self, name: &str) -> Option<i64> {
        let active = self.active.lock().unwrap();
        active.get(name).map(|(job, c)| {
            c.store(true, Ordering::Relaxed);
            *job
        })
    }

    pub fn running(&self, name: &str) -> Option<i64> {
        self.active.lock().unwrap().get(name).map(|(j, _)| *j)
    }

    /// Stops all syncs (between files / chunks) and waits for the workers.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        for (_, c) in self.active.lock().unwrap().values() {
            c.store(true, Ordering::Relaxed);
        }
        self.wake.notify_all();
        let threads: Vec<_> = self.threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
    }
}

fn worker(st: Arc<AppState>) {
    let r = &st.mirror_runner;
    loop {
        let task = {
            let mut q = r.queue.lock().unwrap();
            loop {
                if let Some(t) = q.pop_front() {
                    break Some(t);
                }
                if r.stopping.load(Ordering::Relaxed) {
                    break None;
                }
                q = r.wake.wait_timeout(q, Duration::from_secs(1)).unwrap().0;
            }
        };
        let Some(task) = task else { return };
        let name = task.name.clone();
        run_task(&st, task);
        r.active.lock().unwrap().remove(&name);
    }
}

pub fn log_path(st: &AppState, job: i64) -> PathBuf {
    st.cfg.data_dir.join("logs").join(format!("{job}.log"))
}

fn summary(r: &SyncReport) -> String {
    let mut s = format!(
        "{} file(s) downloaded ({:.1} MB), {} deleted",
        r.downloaded,
        r.downloaded_bytes as f64 / 1e6,
        r.deleted
    );
    if r.would_delete > 0 {
        s.push_str(&format!(", {} unreferenced left", r.would_delete));
    }
    if !r.ignored_missing.is_empty() {
        s.push_str(&format!(
            ", {} missing upstream (ignored)",
            r.ignored_missing.len()
        ));
    }
    s
}

fn run_task(st: &Arc<AppState>, task: Task) {
    let Task {
        name,
        def,
        opts,
        job,
        cancel,
    } = task;
    let id = job.id();
    let path = log_path(st, id);
    let file = std::fs::create_dir_all(path.parent().unwrap())
        .and_then(|()| std::fs::File::create(&path))
        .ok()
        .map(Mutex::new);
    let log = |line: &str| {
        if let Some(f) = &file {
            let t = time::OffsetDateTime::now_utc();
            let _ = writeln!(
                f.lock().unwrap(),
                "{:02}:{:02}:{:02} {line}",
                t.hour(),
                t.minute(),
                t.second()
            );
        }
    };
    log(&format!(
        "mirror {name}: {} -> {}",
        def.url,
        def.path.display()
    ));

    let run = || -> Result<SyncReport, String> {
        let mut engine = def.engine()?;
        if opts.prune {
            engine.prune = true;
        }
        engine.verify_all |= opts.verify_all;
        if cancel.load(Ordering::Relaxed) {
            return Err("interrupted".into());
        }
        let http = Http::new(Duration::from_secs(60)).map_err(|e| e.to_string())?;
        let state_dir = st.cfg.data_dir.join("mirrors").join(&name);
        let ctx = Ctx {
            http: &http,
            dest: &def.path,
            state_dir: &state_dir,
            cancel: &cancel,
            log: &log,
        };
        sync(&engine, &ctx).map_err(|e| match e {
            aptpod_mirror::Error::Interrupted => "interrupted".to_string(),
            e => e.to_string(),
        })
    };
    // A panic (e.g. on odd upstream data) fails this job, not the worker.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
        .unwrap_or_else(|_| Err("internal error (panic); see the server log".into()));

    let (state, sum, error, report) = match &result {
        Ok(r) if r.ok() => ("done", summary(r), None, Some(r)),
        Ok(r) => {
            let failed: Vec<String> = r
                .dists
                .iter()
                .filter(|d| !d.ok)
                .map(|d| format!("{}: {}", d.dist, d.error.as_deref().unwrap_or("failed")))
                .collect();
            ("failed", summary(r), Some(failed.join("; ")), Some(r))
        }
        Err(e) if e == "interrupted" => ("interrupted", String::new(), Some(e.clone()), None),
        Err(e) => ("failed", String::new(), Some(e.clone()), None),
    };
    log(&format!(
        "{state}: {sum}{}",
        error
            .as_deref()
            .map(|e| format!(" ({e})"))
            .unwrap_or_default()
    ));
    let report_json = report.and_then(|r| serde_json::to_value(r).ok());
    let shown = if sum.is_empty() {
        error.clone().unwrap_or_default()
    } else {
        sum.clone()
    };
    if let Err(e) = st
        .auth
        .mirror_finished(&name, state, &shown, report_json.as_ref())
    {
        tracing::error!("mirror {name}: recording the status: {e}");
    }
    if let Err(e) = job.end(state, &sum, error.as_deref()) {
        tracing::error!("mirror {name}: ending job {id}: {e:?}");
    }
    tracing::info!(mirror = %name, job = id, state, "mirror sync");
}
