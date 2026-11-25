//! A close's work under the actor's lock (docs/trees.md, "File maps"): an
//! open whose saved map did not move and whose length changed closes in one
//! hold, comparing no byte, however many calls it made; any other close
//! decides with the lock released and holds it twice, briefly. The holds
//! are the calling thread's (other threads of the volume hold the lock at
//! events of their own); the bytes compared are read through the
//! process-wide stage counters, so one test runs at a time.
use std::sync::{Arc, Mutex, PoisonError};

use agentfs_contracts::pod::PodContext;
use agentfs_engine::tree_actor::stages::{Stages, holds_on_this_thread};
use agentfs_engine::tree_actor::{Recording, Saved, TreeId};
use trees::{ATTRS, identity, writer};

#[path = "support/trees.rs"]
mod trees;

/// The counters are process-wide: one test reads them at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// A volume with `a` saved as ten bytes.
fn with_a(dir: &std::path::Path) -> Arc<Recording> {
    let rec = Recording::create(
        &dir.join("state"),
        &identity(),
        None,
        &PodContext::default(),
    )
    .unwrap();
    let w = writer("w");
    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, "a", &w, Some(ATTRS))
        .unwrap();
    rec.actor()
        .unwrap()
        .write(h, 0, b"0123456789", false)
        .unwrap();
    rec.close_with(h, |_| Ok(())).unwrap();
    rec
}

/// The lock holds and bytes compared by one `close_with`, which runs on
/// this thread.
fn close_costs(rec: &Arc<Recording>, handle: u64) -> (Saved, u64, u64) {
    let before = Stages::current();
    let held = holds_on_this_thread();
    let saved = rec.close_with(handle, |_| Ok(())).unwrap();
    let holds = holds_on_this_thread() - held;
    let after = Stages::current();
    (saved, holds, after.bytes_compared - before.bytes_compared)
}

/// A4: a write that changes the length, its base not moved: one hold of
/// the lock and no byte compared. The same length decides off the lock: two
/// holds, and its own bytes compared.
#[test]
fn a_close_whose_base_did_not_move_is_one_hold() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let rec = with_a(tmp.path());
    let w = writer("w");
    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, "a", &w, None)
        .unwrap();
    rec.actor().unwrap().write(h, 0, b"more", true).unwrap();
    let (saved, holds, compared) = close_costs(&rec, h);
    assert!(matches!(saved, Saved::Commit(_)), "{saved:?}");
    assert_eq!((holds, compared), (1, 0));
    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, "a", &w, None)
        .unwrap();
    rec.actor().unwrap().write(h, 2, b"AB", false).unwrap();
    let (saved, holds, compared) = close_costs(&rec, h);
    assert!(matches!(saved, Saved::Commit(_)), "{saved:?}");
    assert_eq!((holds, compared), (2, 2));
    assert_eq!(
        rec.actor().unwrap().read(TreeId::Root, "a").unwrap(),
        b"01AB456789more"
    );
}

/// A6: an open of 100,000 overwrites of a small range and one length change
/// closes as fast: the handle keeps no call list to replay, and its view,
/// a few pieces, is the save.
#[test]
fn a_hundred_thousand_calls_close_in_one_hold() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let rec = with_a(tmp.path());
    let w = writer("w");
    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, "a", &w, None)
        .unwrap();
    {
        let mut actor = rec.actor().unwrap();
        for n in 0..100_000_u64 {
            actor
                .write(h, n % 8, &[b'a' + (n % 26) as u8], false)
                .unwrap();
        }
        actor.write(h, 0, b"!", true).unwrap();
    }
    let (saved, holds, compared) = close_costs(&rec, h);
    assert!(matches!(saved, Saved::Commit(_)), "{saved:?}");
    assert_eq!((holds, compared), (1, 0));
    let bytes = rec.actor().unwrap().read(TreeId::Root, "a").unwrap();
    assert_eq!(bytes.len(), 11);
    assert_eq!(&bytes[8..], b"89!");
}
