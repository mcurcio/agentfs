//! A built batch costs memory in proportion to the batch, not to the volume:
//! the builder hands its state across by sharing it (persistent maps), never
//! by copying it (docs/trees.md, "History trails the files").
//!
//! One test in its own binary: the counting allocator is process-wide, and
//! the test thread only waits in `settle` while the builder works.
//!
//! The volumes are 1,000 and 10,000 files ([`SMALL`] and ten times it). What
//! the test shows is the ratio between a tenfold volume's batches, not either
//! size, and nearly all of its time is building the volumes (2,000 and 20,000
//! took 71 s, most of it the larger volume's creates). At these sizes one
//! volume-sized allocation a batch (a listing of the root's files) still
//! takes the ratio to 2.76, over the bar's 1.75.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use agentfs_core::tree_actor::{TreeActor, TreeId};
use trees::{ATTRS, new_actor, writer};

#[path = "support/trees.rs"]
mod trees;

/// Every byte asked of the allocator, process-wide: allocations and growth.
struct Counting;

static ALLOCATED: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: the caller's layout, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: the caller's layout, passed on.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's pointer and layout, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(
            (new_size as u64).saturating_sub(layout.size() as u64),
            Ordering::Relaxed,
        );
        // SAFETY: the caller's pointer, layout and size, passed on.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The batch measured: this many creates, each written and closed.
const BATCH: usize = 50;

/// The smaller volume's files; the larger holds ten times as many.
const SMALL: usize = 1_000;

/// One create: a new file, written and closed.
fn create(v: &mut TreeActor, path: &str) {
    let h = v
        .write_open(TreeId::Root, path, &writer("w"), Some(ATTRS))
        .unwrap();
    v.write(h, 0, path.as_bytes(), false).unwrap();
    v.close(h).unwrap();
}

/// One create whose two steps are each built before the next record: the
/// builder is idle at each step, so each is one batch, whatever the timing.
/// How the builder would group steps appended while it works is a race this
/// test must not measure: one more batch costs about as much as ten creates.
fn create_in_two_batches(v: &mut TreeActor, path: &str) {
    let h = v
        .write_open(TreeId::Root, path, &writer("w"), Some(ATTRS))
        .unwrap();
    v.settle().unwrap();
    v.write(h, 0, path.as_bytes(), false).unwrap();
    v.close(h).unwrap();
    v.settle().unwrap();
}

/// Bytes allocated while `BATCH` creates are recorded and built, each step in
/// a batch of its own, on a volume that already holds `n` files.
fn batch_bytes(n: usize) -> u64 {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(&tmp.path().join("state"));
    for i in 0..n {
        create(&mut v, &format!("f{i:06}"));
    }
    v.settle().unwrap();
    // The same batches first, so the measured ones start from a builder at
    // rest.
    for i in 0..BATCH {
        create_in_two_batches(&mut v, &format!("warm{i:03}"));
    }
    let before = ALLOCATED.load(Ordering::Relaxed);
    for i in 0..BATCH {
        create_in_two_batches(&mut v, &format!("new{i:03}"));
    }
    ALLOCATED.load(Ordering::Relaxed) - before
}

#[test]
fn a_batch_allocates_in_proportion_to_the_batch_not_the_volume() {
    let small = batch_bytes(SMALL);
    let large = batch_bytes(10 * SMALL);
    eprintln!(
        "{BATCH} creates in {} batches: {small} B at {SMALL} files, {large} B at {}",
        2 * BATCH,
        10 * SMALL
    );
    // A change to a persistent map copies its path, which grows with the
    // log of the volume: about 1.39 times from 1,000 files to 10,000 over
    // these batches (25.4 MB and 35.2 MB; 1.45 from 2,000 to 20,000). A
    // volume-sized allocation once a batch makes it over 2.5.
    assert!(
        4 * large <= 7 * small,
        "batches at {} files allocated {large} B, over 1.75 times the {small} B at {SMALL}",
        10 * SMALL
    );
}
