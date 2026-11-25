//! A built batch costs memory in proportion to the batch, not to the volume:
//! the builder hands its state across by sharing it (persistent maps), never
//! by copying it (docs/trees.md, "History trails the files").
//!
//! One test in its own binary: the counting allocator is process-wide, and
//! the test thread only waits in `settle` while the builder works.
//!
//! The volumes are [`small`] files and ten times it: 1,000 locally, 2,000
//! on GitHub Actions or with `AGENTFS_TEST_VOLUMES=full` (docs/development.md,
//! "Running tests"). What the test shows is the ratio between a tenfold
//! volume's batches, not either size, and nearly all of its time is building
//! the volumes (2,000 and 20,000 took 71 s, most of it the larger volume's
//! creates). The cost of the local sizes is sensitivity: a local run catches
//! a volume-sized allocation once a batch only above about 11 bytes per file
//! (4.6 at 2,000 and 20,000, in CI). A listing of the root's files takes the
//! ratio to 2.76 and a copy of the state map (about 18 bytes an entry) to
//! about 1.95, both over the bar's 1.75; a `Vec<u64>` of the volume's ids (8
//! bytes a file, about 1.65) is not caught locally.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use agentfs_core::tree_actor::{TreeActor, TreeId};
use trees::{ATTRS, Volumes, new_actor, writer};

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

/// The smaller volume's files, 1,000 locally and 2,000 in CI; the larger
/// holds ten times as many.
fn small() -> usize {
    Volumes::from_env().files(1_000, 2_000)
}

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
    let files = small();
    let small = batch_bytes(files);
    let large = batch_bytes(10 * files);
    eprintln!(
        "{BATCH} creates in {} batches: {small} B at {files} files, {large} B at {}",
        2 * BATCH,
        10 * files
    );
    // A change to a persistent map copies its path, which grows with the
    // log of the volume: about 1.39 times from 1,000 files to 10,000 over
    // these batches (25.4 MB and 35.2 MB; 1.45 from 2,000 to 20,000). An
    // allocation of c bytes per file once a batch adds 0.1c MB and 1.0c MB
    // over the 100 batches, so the bar fails it when c is over about 11
    // (about 4.6 at CI's 2,000 and 20,000).
    assert!(
        4 * large <= 7 * small,
        "batches at {} files allocated {large} B, over 1.75 times the {small} B at {files}",
        10 * files
    );
}
