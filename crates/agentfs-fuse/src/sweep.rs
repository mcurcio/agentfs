//! The periodic sweep (design §3.1, §3.5): every `agentfs.sweepInterval`
//! it retries records that failed (the repair queue) and records files that
//! are held open and changed, which no close would ever record. Both are
//! late by design: the sweep is a backstop, not a boundary.

use std::io;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use agentfs_core::service::Stop;

use crate::ops::Ops;

/// The running sweep thread.
pub struct Sweeper {
    stop: Arc<Stop>,
    thread: JoinHandle<()>,
}

impl Sweeper {
    /// Starts sweeping `ops` every `interval`.
    pub fn start(ops: Arc<Ops>, interval: Duration) -> io::Result<Self> {
        let stop = Arc::new(Stop::default());
        let thread = {
            let stop = stop.clone();
            thread::Builder::new()
                .name("agentfs-sweep".into())
                .spawn(move || {
                    while !stop.wait(interval) {
                        ops.sweep(false);
                    }
                })?
        };
        Ok(Self { stop, thread })
    }

    /// Stops the thread, waiting for a sweep in progress.
    pub fn stop(self) {
        self.stop.stop();
        let _ = self.thread.join();
    }
}
