//! Signals and shutdown for the long-running commands, `run` and `mount`
//! (design §11.2, §17.1).
//!
//! SIGINT and SIGTERM, the end of a FUSE session (something else
//! unmounted the volume) and a failure of `--passthrough on` all arrive on
//! one channel, so the command waits for whichever comes first. Once
//! shutdown has begun, another signal exits at once with status 130.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use agentfs_core::service::Stop;
use anyhow::{Context as _, Result};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

/// Exit status after a second signal.
pub const EXIT_SECOND_SIGNAL: i32 = 130;

/// Why the daemon woke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    Signal(i32),
    /// The FUSE session ended without being asked to.
    Unmounted,
    /// `--passthrough on` could not be honoured after INIT.
    PassthroughFailed,
}

pub struct Daemon {
    tx: Sender<Wake>,
    rx: Receiver<Wake>,
}

impl Daemon {
    /// Registers the signal handlers. Call it before anything starts, so an
    /// early signal is not lost.
    pub fn start() -> Result<Self> {
        let mut signals = Signals::new([SIGINT, SIGTERM]).context("installing signal handlers")?;
        let (tx, rx) = channel();
        let to_main = tx.clone();
        std::thread::Builder::new()
            .name("agentfs-signals".into())
            .spawn(move || {
                for signal in signals.forever() {
                    if to_main.send(Wake::Signal(signal)).is_err() {
                        break;
                    }
                }
            })
            .context("starting the signal thread")?;
        Ok(Self { tx, rx })
    }

    /// Wakes the daemon with `wake` when `ended` is set.
    pub fn watch(&self, ended: Arc<Stop>, wake: Wake) -> Result<()> {
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("agentfs-watch".into())
            .spawn(move || {
                while !ended.wait(Duration::from_secs(3600)) {}
                let _ = tx.send(wake);
            })
            .context("starting the session watcher")?;
        Ok(())
    }

    /// Whether a signal arrived, without waiting: a standby waiting for
    /// the lock stops on it (design §14.2).
    pub fn signalled(&self) -> Option<Wake> {
        self.rx.try_recv().ok()
    }

    /// Blocks until the first wake-up, then makes any later signal exit
    /// the process at once.
    pub fn wait(self) -> Wake {
        // The sender lives in `self.tx` too, so `recv` cannot fail.
        let wake = self.rx.recv().unwrap_or(Wake::Signal(0));
        let rx = self.rx;
        std::thread::spawn(move || {
            for wake in rx {
                if let Wake::Signal(_) = wake {
                    eprintln!("second signal: exiting now");
                    std::process::exit(EXIT_SECOND_SIGNAL);
                }
            }
        });
        wake
    }
}
