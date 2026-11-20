//! A built batch costs memory in proportion to the batch, not to the volume:
//! the builder hands its state across by sharing it (persistent maps), never
//! by copying it (docs/trees.md, "History trails the files").
//!
//! One test in its own binary: the counting allocator is process-wide, and
//! the test thread only waits in `settle` while the builder works.

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

/// One create: a new file, written and closed.
fn create(v: &mut TreeActor, path: &str) {
    let h = v
        .write_open(TreeId::Root, path, &writer("w"), Some(ATTRS))
        .unwrap();
    v.write(h, 0, path.as_bytes(), false).unwrap();
    v.close(h).unwrap();
}

/// Bytes allocated while `BATCH` creates are recorded and built, on a volume
/// that already holds `n` files.
fn batch_bytes(n: usize) -> u64 {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(&tmp.path().join("state"));
    for i in 0..n {
        create(&mut v, &format!("f{i:06}"));
    }
    v.settle().unwrap();
    // One batch first, so the measured one starts from a builder at rest.
    for i in 0..BATCH {
        create(&mut v, &format!("warm{i:03}"));
    }
    v.settle().unwrap();
    let before = ALLOCATED.load(Ordering::Relaxed);
    for i in 0..BATCH {
        create(&mut v, &format!("new{i:03}"));
    }
    v.settle().unwrap();
    ALLOCATED.load(Ordering::Relaxed) - before
}

#[test]
fn a_batch_allocates_in_proportion_to_the_batch_not_the_volume() {
    let small = batch_bytes(2_000);
    let large = batch_bytes(20_000);
    eprintln!("batch of {BATCH} creates: {small} B at 2,000 files, {large} B at 20,000");
    assert!(
        large <= 2 * small,
        "a batch at 20,000 files allocated {large} B, over twice the {small} B at 2,000"
    );
}
