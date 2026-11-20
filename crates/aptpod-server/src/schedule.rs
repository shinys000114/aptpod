// SPDX-License-Identifier: GPL-3.0-only

//! The mirror scheduler and the public status files.
//!
//! Once a minute (in the configured time zone) every enabled mirror whose
//! schedule matches is synced, unless it is still syncing; runs missed
//! while the server was down are not replayed. Status files are rewritten
//! whenever a sync starts or ends and when definitions change.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::api::AppState;
use crate::cron::Cron;
use crate::mirrors::MirrorDef;
use crate::runner::SyncOpts;

pub fn start(st: &Arc<AppState>) -> std::thread::JoinHandle<()> {
    let st = st.clone();
    std::thread::Builder::new()
        .name("scheduler".into())
        .spawn(move || {
            write_status(&st);
            let mut fired: HashMap<String, i64> = HashMap::new();
            while !st.mirror_runner.is_stopping() {
                tick(&st, &mut fired);
                // Wake well within every minute; stop promptly.
                for _ in 0..20 {
                    if st.mirror_runner.is_stopping() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        })
        .expect("spawning the scheduler")
}

fn tick(st: &Arc<AppState>, fired: &mut HashMap<String, i64>) {
    let now = jiff::Timestamp::now().to_zoned_datetime(st.cfg.tz());
    let minute = now.timestamp().as_second().div_euclid(60);
    let Ok(mirrors) = st.auth.mirrors() else {
        return;
    };
    for (name, def) in mirrors {
        let Some(cron) = def
            .enabled
            .then_some(())
            .and(def.schedule.as_deref())
            .and_then(|s| Cron::parse(s).ok())
        else {
            continue;
        };
        if !cron.matches(now.datetime()) || fired.get(&name) == Some(&minute) {
            continue;
        }
        fired.insert(name.clone(), minute);
        if let Some(job) = st.mirror_runner.running(&name) {
            tracing::warn!(mirror = %name, job, "scheduled sync skipped: still syncing");
            continue;
        }
        match st
            .mirror_runner
            .submit(st, &name, def, "scheduler", SyncOpts::default())
        {
            Ok(job) => tracing::info!(mirror = %name, job, "scheduled sync"),
            Err(e) => tracing::error!(mirror = %name, "scheduled sync: {e:?}"),
        }
    }
}

/// When `def` syncs next, if it is scheduled.
pub fn next_run(st: &AppState, def: &MirrorDef) -> Option<String> {
    let cron = Cron::parse(def.schedule.as_deref()?).ok()?;
    if !def.enabled {
        return None;
    }
    let now = jiff::Timestamp::now().to_zoned_datetime(st.cfg.tz());
    cron.next_after(&now).map(|z| iso(&z))
}

fn iso(z: &jiff::ZonedDateTime) -> String {
    z.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

fn local(st: &AppState, t: Option<i64>) -> Option<jiff::ZonedDateTime> {
    jiff::Timestamp::from_second(t?)
        .ok()
        .map(|ts| ts.to_zoned_datetime(st.cfg.tz()))
}

/// Rewrites `<status_dir>/<name>.json`, `<name>.svg` and `index.json`, and
/// removes files of mirrors that are gone. Errors are logged.
pub fn write_status(st: &AppState) {
    let Some(dir) = &st.cfg.status_dir else {
        return;
    };
    // Workers, the scheduler and the API all call this; one at a time.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = write_all(st, dir) {
        tracing::error!("writing mirror status to {}: {e}", dir.display());
    }
}

fn write_all(st: &AppState, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mirrors = st.auth.mirrors().map_err(|e| e.to_string())?;
    let mut index = Vec::new();
    for (name, def) in &mirrors {
        let s = st.auth.mirror_status(name).map_err(|e| e.to_string())?;
        let running = st.mirror_runner.running(name).is_some();
        let state = if running {
            "syncing"
        } else {
            match s.state.as_deref() {
                Some("done") => "synced",
                Some("failed") => "failed",
                Some("interrupted") => "interrupted",
                Some("running") => "syncing",
                _ => "never synced",
            }
        };
        let when = local(st, s.finished).map(|z| z.strftime("%Y%m%d%H%M%S").to_string());
        let message = match (&when, state) {
            (Some(w), "synced" | "failed" | "interrupted") => format!("{state} {w}"),
            _ => state.to_string(),
        };
        let color = match state {
            "synced" => "#4b0",
            "failed" => "#e05d44",
            "interrupted" => "#dfb317",
            "syncing" => "#007ec6",
            _ => "#9f9f9f",
        };
        let entry = json!({
            "name": name,
            "kind": def.kind,
            "state": state,
            "started": local(st, s.started).map(|z| iso(&z)),
            "finished": local(st, s.finished).map(|z| iso(&z)),
            "last_success": local(st, s.last_ok).map(|z| iso(&z)),
            "summary": s.summary,
            "schedule": def.schedule,
            "next_run": next_run(st, def),
        });
        write_atomic(&dir.join(format!("{name}.json")), &pretty(&entry))?;
        write_atomic(
            &dir.join(format!("{name}.svg")),
            &badge("status", &message, color),
        )?;
        index.push(entry);
    }
    write_atomic(&dir.join("index.json"), &pretty(&Value::Array(index)))?;
    // Files of mirrors that no longer exist.
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let file = e.file_name().to_string_lossy().to_string();
            let stem = file
                .strip_suffix(".json")
                .or_else(|| file.strip_suffix(".svg"));
            if let Some(stem) = stem
                && stem != "index"
                && !mirrors.iter().any(|(n, _)| n == stem)
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default() + "\n"
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    // `.<name>.tmp`: unique per target (name.json and name.svg must not
    // share one) and hidden from the cleanup below.
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name().expect("file name").to_string_lossy()
    ));
    std::fs::write(&tmp, text)
        .and_then(|()| {
            std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o644))
        })
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A flat two-part badge, `label | message`.
pub fn badge(label: &str, message: &str, color: &str) -> String {
    // Verdana 11px is about 6.5px per character; 10px padding per side.
    let w = |s: &str| (s.chars().count() as f64 * 6.5).round() as u32 + 10;
    let (lw, mw) = (w(label), w(message));
    let (label, message) = (esc(label), esc(message));
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{t}\" height=\"20\" role=\"img\" aria-label=\"{label}: {message}\">\
<title>{label}: {message}</title><g shape-rendering=\"crispEdges\"><rect width=\"{lw}\" height=\"20\" fill=\"#555\"/>\
<rect x=\"{lw}\" width=\"{mw}\" height=\"20\" fill=\"{color}\"/></g><g fill=\"#fff\" text-anchor=\"middle\" \
font-family=\"Verdana,Geneva,DejaVu Sans,sans-serif\" font-size=\"11\"><text x=\"{lx}\" y=\"14\">{label}</text>\
<text x=\"{mx}\" y=\"14\">{message}</text></g></svg>\n",
        t = lw + mw,
        lx = lw / 2,
        mx = lw + mw / 2,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn badge_escapes_and_sizes() {
        let b = super::badge("status", "synced 20261120122730 <x>", "#4b0");
        assert!(b.starts_with("<svg") && b.contains("synced 20261120122730 &lt;x&gt;"));
        assert!(b.contains("fill=\"#4b0\""));
    }
}
