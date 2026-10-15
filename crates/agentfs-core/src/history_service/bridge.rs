//! Synchronous callers on an async runtime (design §14): Tokio's "bridging
//! with sync code" pattern (tokio.rs/tokio/topics/bridging).
//!
//! A [`super::HistoryClient`] or [`super::HistoryServer`] owns a
//! multi-thread runtime and runs each request with [`Bridge::block_on`] on
//! the calling thread: FUSE worker threads, the listener's thread, the
//! restore and main threads, any number at once. The rules that keep that
//! sound:
//!
//! - `block_on` is never called from a runtime worker or inside another
//!   `block_on` (Tokio panics);
//! - no task spawned on a runtime holds a client, a server or a bridge,
//!   since dropping a runtime inside a task panics;
//! - the service runs every `Recorder` call in `spawn_blocking`, never on
//!   a worker, and a front's hooks run on its listener thread.

use std::future::Future;

use tokio::runtime::{Builder, Handle, Runtime};

/// A multi-thread runtime that synchronous code drives.
pub(crate) struct Bridge {
    runtime: Runtime,
}

impl Bridge {
    /// Multi-thread runtime, `workers` worker threads named `name`, IO and
    /// time enabled.
    pub(crate) fn new(name: &str, workers: usize) -> std::io::Result<Bridge> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(workers.max(1))
            .thread_name(name)
            .enable_all()
            .build()?;
        Ok(Self { runtime })
    }

    /// Runs `future` on the calling thread; any number of threads at once.
    /// Never from a runtime worker or inside another `block_on` (Tokio
    /// panics).
    pub(crate) fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    pub(crate) fn handle(&self) -> &Handle {
        self.runtime.handle()
    }
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn block_on_runs_on_many_threads_at_once() {
        // Each thread's future waits for all the others: only concurrent
        // block_on calls finish.
        const THREADS: usize = 8;
        let bridge = Arc::new(Bridge::new("agentfs-test", 1).unwrap());
        let barrier = Arc::new(tokio::sync::Barrier::new(THREADS));
        let (done, finished) = std::sync::mpsc::channel();
        for _ in 0..THREADS {
            let (bridge, barrier, done) = (bridge.clone(), barrier.clone(), done.clone());
            std::thread::spawn(move || {
                bridge.block_on(barrier.wait());
                let _ = done.send(());
            });
        }
        for n in 0..THREADS {
            finished
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("only {n} of {THREADS} block_on calls finished"));
        }
    }

    #[test]
    fn spawned_tasks_run_on_the_bridges_workers() {
        let bridge = Bridge::new("agentfs-test", 2).unwrap();
        let task = bridge
            .handle()
            .spawn(async { std::thread::current().name().map(str::to_owned) });
        let name = bridge.block_on(task).unwrap();
        assert_eq!(name.as_deref(), Some("agentfs-test"));
    }
}
