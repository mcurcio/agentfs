//! Recording a change boundary (design §3.1, §3.2, §3.5).
//!
//! The FUSE operations describe what changed as [`Change`]s on raw paths.
//! [`Boundary::record`] classifies each path ([`crate::names`]): `.git`
//! internals are dropped silently, invalid names raise `PathRefused`, and
//! the rest go to [`Recorder::record`] as one record, synchronously, before
//! the operation replies.
//!
//! When recording fails (design §3.5) the core has already raised
//! `RecordFailed`. FLUSH returns `EIO` and leaves the inode dirty, so the
//! next boundary records it again. A namespace operation (rename, unlink,
//! link, ...) has already changed the backing tree by then, so its paths go
//! on the *repair queue*: the sweep re-derives each queued path's absolute
//! state from the backing tree and records it. Records hold absolute state,
//! so a repair is idempotent and cannot undo a later change.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use agentfs_core::paths::PathError;
use agentfs_core::record::{Caller, Trigger};
use agentfs_core::volume::{Input, Recorder, VolumeError};

use crate::names::{Class, classify};

/// One path's new state, before classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Relative to the mount root.
    pub path: Vec<u8>,
    pub state: State,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// A regular file whose content is at `backing` (a `/proc/self/fd`
    /// path the caller keeps open until the record returns).
    File {
        backing: PathBuf,
        executable: bool,
    },
    Symlink {
        target: Vec<u8>,
    },
    Removed,
}

impl Change {
    pub fn removed(path: Vec<u8>) -> Self {
        Self {
            path,
            state: State::Removed,
        }
    }
}

/// The recorder as the FUSE layer uses it, with the repair queue.
pub struct Boundary {
    recorder: Recorder,
    /// Paths whose record failed after the backing tree changed, with the
    /// caller that changed them.
    repairs: Mutex<BTreeMap<Vec<u8>, Caller>>,
}

impl Boundary {
    pub fn new(recorder: Recorder) -> Self {
        Self {
            recorder,
            repairs: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn recorder(&self) -> &Recorder {
        &self.recorder
    }

    /// Records `changes` as one record. Returns whether anything was
    /// journaled; an error means nothing was (and `RecordFailed` was
    /// raised).
    pub fn record(
        &self,
        trigger: Trigger,
        caller: Caller,
        changes: Vec<Change>,
    ) -> Result<bool, VolumeError> {
        let mut inputs = Vec::with_capacity(changes.len());
        for change in changes {
            let path = match classify(&change.path) {
                Class::Record(path) => path,
                Class::InGitDir => continue,
                Class::Invalid { raw, error } => {
                    // A removal of a path git cannot store removes nothing
                    // that was ever recorded; only new content is refused.
                    if change.state != State::Removed {
                        self.recorder.refuse_invalid(&raw, error);
                    }
                    continue;
                }
            };
            inputs.push(match change.state {
                State::File {
                    backing,
                    executable,
                } => Input::Write {
                    path,
                    backing,
                    executable,
                },
                State::Symlink { target } => match String::from_utf8(target) {
                    Ok(target) => Input::Symlink { path, target },
                    Err(e) => {
                        let target = String::from_utf8_lossy(e.as_bytes()).into_owned();
                        self.recorder.refuse_invalid(
                            path.as_str(),
                            PathError::NotUtf8(format!("symlink target {target}")),
                        );
                        continue;
                    }
                },
                State::Removed => Input::Remove { path },
            });
        }
        if inputs.is_empty() {
            return Ok(false);
        }
        Ok(self.recorder.record(trigger, caller, inputs)?.seq.is_some())
    }

    /// Queues `paths` for the sweep to re-derive and record (design §3.5).
    pub fn queue_repair(&self, paths: impl IntoIterator<Item = Vec<u8>>, caller: Caller) {
        let mut queue = self.lock();
        for path in paths {
            queue.insert(path, caller);
        }
    }

    /// Takes everything queued.
    pub fn take_repairs(&self) -> Vec<(Vec<u8>, Caller)> {
        std::mem::take(&mut *self.lock()).into_iter().collect()
    }

    pub fn pending_repairs(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<Vec<u8>, Caller>> {
        self.repairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The trigger a recorded change gets when the sweep repairs it.
pub const REPAIR_TRIGGER: Trigger = Trigger::Sweep;
