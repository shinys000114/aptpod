// SPDX-License-Identifier: GPL-3.0-only

//! The single writer: one thread owns the writing [`Repo`] handle and runs
//! jobs one at a time, in arrival order. Requests wait for their job's
//! result; queries use a separate read handle and never wait here.

use std::sync::Mutex;
use std::sync::mpsc;
use std::thread::JoinHandle;

use aptpod_repo::Repo;

type Task = Box<dyn FnOnce(&mut Repo) + Send>;

pub struct Writer {
    tx: Mutex<Option<mpsc::Sender<Task>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug)]
pub struct Stopped;

impl Writer {
    pub fn start(mut repo: Repo) -> Self {
        let (tx, rx) = mpsc::channel::<Task>();
        let thread = std::thread::Builder::new()
            .name("writer".into())
            .spawn(move || {
                for task in rx {
                    // A panicking job fails its request (the reply channel is
                    // dropped) but must not take the writer down with it;
                    // database changes roll back with their transaction.
                    let r = &mut repo;
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || task(r)))
                        .is_err()
                    {
                        tracing::error!("a writer job panicked");
                    }
                }
            })
            .expect("spawning the writer thread");
        Self {
            tx: Mutex::new(Some(tx)),
            thread: Mutex::new(Some(thread)),
        }
    }

    /// Runs `f` on the writer thread and waits for it.
    pub async fn run<T, F>(&self, f: F) -> Result<T, Stopped>
    where
        T: Send + 'static,
        F: FnOnce(&mut Repo) -> T + Send + 'static,
    {
        let (done, wait) = tokio::sync::oneshot::channel();
        let task: Task = Box::new(move |repo| {
            let _ = done.send(f(repo));
        });
        {
            let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
            tx.as_ref()
                .ok_or(Stopped)?
                .send(task)
                .map_err(|_| Stopped)?;
        }
        wait.await.map_err(|_| Stopped)
    }

    /// [`Writer::run`] for plain (non-tokio) threads, e.g. mirror workers.
    pub fn run_blocking<T, F>(&self, f: F) -> Result<T, Stopped>
    where
        T: Send + 'static,
        F: FnOnce(&mut Repo) -> T + Send + 'static,
    {
        let (done, wait) = mpsc::channel();
        let task: Task = Box::new(move |repo| {
            let _ = done.send(f(repo));
        });
        {
            let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
            tx.as_ref()
                .ok_or(Stopped)?
                .send(task)
                .map_err(|_| Stopped)?;
        }
        wait.recv().map_err(|_| Stopped)
    }

    /// Lets queued jobs finish, then stops the thread.
    pub fn shutdown(&self) {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(t) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = t.join();
        }
    }
}
