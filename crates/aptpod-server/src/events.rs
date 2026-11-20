// SPDX-License-Identifier: GPL-3.0-only

//! Job events: every job start and end is written to the job log and
//! broadcast to `GET /api/v1/events` (server-sent events).

use std::convert::Infallible;
use std::sync::Arc;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use tokio_stream::wrappers::BroadcastStream;

use crate::api::{ApiError, AppState, Caller, St};
use crate::authdb::Role;

#[derive(Debug, Clone, Serialize)]
pub struct JobEvent {
    pub id: i64,
    pub kind: String,
    pub resource: String,
    pub user: String,
    /// running, done or failed
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Open job: finish it with [`Job::finish`].
pub struct Job {
    st: Arc<AppState>,
    event: JobEvent,
}

impl AppState {
    pub fn job_start(
        self: &Arc<Self>,
        kind: &str,
        resource: &str,
        user: &str,
    ) -> Result<Job, ApiError> {
        let id = self.auth.job_start(kind, resource, user)?;
        let event = JobEvent {
            id,
            kind: kind.into(),
            resource: resource.into(),
            user: user.into(),
            state: "running",
            summary: None,
            error: None,
        };
        let _ = self.events.send(event.clone());
        Ok(Job {
            st: self.clone(),
            event,
        })
    }
}

impl Job {
    pub fn id(&self) -> i64 {
        self.event.id
    }

    /// Ends the job in `state` (done, failed, interrupted).
    pub fn end(
        mut self,
        state: &'static str,
        summary: &str,
        error: Option<&str>,
    ) -> Result<(), ApiError> {
        self.st.auth.job_end(self.event.id, state, summary, error)?;
        self.event.state = state;
        self.event.summary = Some(summary.into()).filter(|s: &String| !s.is_empty());
        self.event.error = error.map(String::from);
        let _ = self.st.events.send(self.event);
        Ok(())
    }

    pub fn finish(mut self, result: Result<&str, &str>) -> Result<(), ApiError> {
        self.st.auth.job_finish(self.event.id, result)?;
        match result {
            Ok(s) => {
                self.event.state = "done";
                self.event.summary = Some(s.into());
            }
            Err(e) => {
                self.event.state = "failed";
                self.event.error = Some(e.into());
            }
        }
        let _ = self.st.events.send(self.event);
        Ok(())
    }
}

/// Streams job events until the client goes away or the server stops.
pub async fn events(
    axum::extract::State(st): St,
    caller: Caller,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    caller.need(Role::Reader)?;
    let mut stopping = st.stopping.subscribe();
    let stream = BroadcastStream::new(st.events.subscribe())
        // A slow client misses events rather than holding the server.
        .filter_map(|e| async move { e.ok() })
        .map(|e| {
            Ok(Event::default()
                .event("job")
                .json_data(&e)
                .unwrap_or_else(|_| Event::default().comment("unserializable")))
        })
        .take_until(async move {
            let _ = stopping.wait_for(|s| *s).await;
        });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
