//! The kernel's one page cache per file: what the view serves it and records
//! from it (ledger R1.4; target git-model §3, §18).
//!
//! Linux FUSE keeps one page cache per inode, whichever open faulted a page in,
//! and writes dirty pages back through the first writable handle on that inode
//! (`__fuse_write_file_get`), setting `FUSE_WRITE_CACHE`. So a page write does
//! not say which program made its stores, and a page fill cannot be private to
//! one open. The view answers per file:
//!
//! - a page fill (`FUSE_READ` without `FUSE_READ_LOCKOWNER`) reads the file's
//!   *image*: its saved bytes plus every open's recorded calls, in arrival
//!   order, which is what a Linux page would hold;
//! - a page write records only the bytes that differ from what that page was
//!   last filled with or written back as, so a stale page never reverts a
//!   descriptor write;
//! - the recorded bytes belong to the file, not to the handle the kernel named:
//!   the next close of any write-open saves them with its own calls.
//!
//! Descriptor reads stay private to their open (R1.1, R1.3). Nothing here is
//! durable except the [`Run`]s in the journal's page-write records.
//!
//! Interface only until bead agentfs-eh5.3 fills it. The hooks keep today's
//! behaviour, so the actor stays green while the other lanes run.
#![allow(dead_code)] // agentfs-eh5.3 wires these and removes this line.

use serde::{Deserialize, Serialize};

use super::{Call, Handle, Result, Volume};

/// One contiguous run of bytes a page write changed, in file offsets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Run {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// The per-file state of a file with a page fill or page write while it is
/// open for writing. It lives from the first such event to the close of the
/// file's last write-open.
#[derive(Default)]
pub(super) struct ObjectPages {}

/// The runs of `page`, which starts at file offset `at`, that differ from
/// `base`. Bytes past the end of `base` compare with zero, which is what the
/// kernel fills a page's tail with. Pure; adjacent changed bytes make one run.
pub(super) fn changed_runs(_base: &[u8], _page: &[u8], _at: u64) -> Vec<Run> {
    unimplemented!("agentfs-eh5.3")
}

impl Volume {
    /// The bytes for a page fill of `object` in `tree`: up to `size` bytes of
    /// its image from `offset`, short at the image's end. Records them as the
    /// base of each page they cover.
    ///
    /// # Errors
    ///
    /// [`super::Error::NotFound`] when `tree` holds no such object.
    pub fn page_fill(
        &mut self,
        _tree: &str,
        _object: u64,
        _offset: u64,
        _size: u32,
    ) -> Result<Vec<u8>> {
        unimplemented!("agentfs-eh5.3")
    }

    /// Records a page write the kernel sent through `handle`: only the changed
    /// bytes, in the journal before this returns. A write that changed nothing
    /// records nothing.
    ///
    /// # Errors
    ///
    /// [`super::Error::NotFound`] for an unknown handle; a journal failure as
    /// from [`Volume::write`], which also stops the actor.
    pub fn page_writeback(&mut self, _handle: u64, _offset: u64, _bytes: &[u8]) -> Result<()> {
        unimplemented!("agentfs-eh5.3")
    }

    /// The size a reply must give the kernel for `object` once the file has
    /// page state: its image's length, because the kernel keeps one size per
    /// inode and drops cached pages past any smaller size it is told
    /// (`fuse_change_attributes`). `None` when the file has no page state; the
    /// view then answers as it does for any other file.
    pub fn inode_size(&self, _tree: &str, _object: u64) -> Option<u64> {
        None
    }

    /// Hook: a descriptor call on `object` is in the journal. Replays call it
    /// too. No I/O.
    pub(super) fn pages_note_call(
        &mut self,
        _tree: &str,
        _object: u64,
        _call: &Call,
    ) -> Result<()> {
        Ok(())
    }

    /// Hook: a page-write record at `seq` is in the journal. Replays call it
    /// too, in journal order. No I/O.
    pub(super) fn pages_note_writeback(
        &mut self,
        _seq: u64,
        _handle: u64,
        _runs: &[Run],
    ) -> Result<()> {
        Ok(())
    }

    /// Hook: `closing` has left the handle table and is about to save. Returns
    /// the calls its save applies to the saved bytes, in arrival order: its own
    /// and the file's page writes not saved yet.
    pub(super) fn pages_calls_for_close(&mut self, closing: &Handle) -> Vec<Call> {
        closing.calls.iter().map(|(_, call)| call.clone()).collect()
    }

    /// Hook: a close of `object` in `tree` has updated the saved bytes. Rebuilds
    /// the image over them, or drops the page state with the last write-open.
    pub(super) fn pages_saved(&mut self, _tree: &str, _object: u64) -> Result<()> {
        Ok(())
    }
}
