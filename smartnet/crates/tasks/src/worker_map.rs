//! A map of named single-thread worker pools.
//!
//! Each worker is a dedicated OS thread that processes closures sent to it via a channel.
//! This is a substitute for `spawn_blocking` that reuses the same OS thread for the same
//! named task, like a 1-thread thread pool keyed by name.

use dashmap::DashMap;
use std::{panic::AssertUnwindSafe, thread};
use tokio::sync::{mpsc, oneshot};

type BoxedTask = Box<dyn FnOnce() + Send + 'static>;

/// A single-thread worker that processes closures sequentially on a dedicated OS thread.
struct WorkerThread {
    /// Sender to submit work to this worker's thread.
    tx: mpsc::UnboundedSender<BoxedTask>,
    /// The OS thread handle. Taken during shutdown to join.
    handle: Option<thread::JoinHandle<()>>,
}

impl WorkerThread {
    /// Spawns a new worker thread with the given name.
    fn new(name: &'static str) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<BoxedTask>();
        let handle = thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                while let Some(task) = rx.blocking_recv() {
                    let _ = std::panic::catch_unwind(AssertUnwindSafe(task));
                }
            })
            .unwrap_or_else(|e| panic!("failed to spawn worker thread {name:?}: {e}"));

        Self { tx, handle: Some(handle) }
    }
}

/// A map of named single-thread workers.
///
/// Each unique name gets a dedicated OS thread that is reused for all tasks submitted under
/// that name. Workers are created lazily on first use.
pub(crate) struct WorkerMap {
    workers: DashMap<&'static str, WorkerThread>,
}

impl Default for WorkerMap {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkerMap {
    /// Creates a new empty `WorkerMap`.
    pub(crate) fn new() -> Self {
        Self { workers: DashMap::new() }
    }

    /// Spawns a closure on the dedicated worker thread for the given name.
    ///
    /// If no worker thread exists for this name yet, one is created with the given name as
    /// the OS thread name. The closure executes on the worker's OS thread and the returned
    /// future resolves with the result.
    pub(crate) fn spawn_on<F, R>(&self, name: &'static str, f: F) -> oneshot::Receiver<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();

        let task: BoxedTask = Box::new(move || {
            let _ = result_tx.send(f());
        });

        let worker = self.workers.entry(name).or_insert_with(|| WorkerThread::new(name));
        let _ = worker.tx.send(task);

        result_rx
    }
}

impl Drop for WorkerMap {
    fn drop(&mut self) {
        for (_, mut w) in std::mem::take(&mut self.workers) {
            // Drop sender so the thread's recv loop exits, then join.
            drop(w.tx);
            if let Some(handle) = w.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

impl std::fmt::Debug for WorkerMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerMap").field("num_workers", &self.workers.len()).finish()
    }
}
