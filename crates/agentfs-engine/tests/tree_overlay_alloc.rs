//! The overlay's estimate holds to what it measures: the live bytes a burst
//! of creates leaves allocated while the builder is held are within a factor
//! of two of the overlay the lag gate is told of (its entries at
//! `OVERLAY_ENTRY_BYTES` each; docs/trees.md, "History trails the files").
//!
//! One test in its own binary: the counting allocator is process-wide.
#![cfg(feature = "failpoints")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicI64, Ordering};

use agentfs_contracts::TreeId;
use agentfs_contracts::pod::PodContext;
use agentfs_engine::tree_actor::Recording;
use trees::{ATTRS, identity, writer};

#[path = "support/trees.rs"]
mod trees;

/// Live bytes, process-wide: allocations and growth less frees.
struct Counting;

static LIVE: AtomicI64 = AtomicI64::new(0);

fn size(bytes: usize) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(size(layout.size()), Ordering::Relaxed);
        // SAFETY: the caller's layout, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(size(layout.size()), Ordering::Relaxed);
        // SAFETY: the caller's layout, passed on.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(size(layout.size()), Ordering::Relaxed);
        // SAFETY: the caller's pointer and layout, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(size(new_size) - size(layout.size()), Ordering::Relaxed);
        // SAFETY: the caller's pointer, layout and size, passed on.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const CREATES: usize = 2_000;

/// One create: a new file, written and closed.
fn create(rec: &Recording, path: &str) {
    let w = writer("w");
    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, path, &w, Some(ATTRS))
        .unwrap();
    rec.actor()
        .unwrap()
        .write(h, 0, path.as_bytes(), false)
        .unwrap();
    rec.actor().unwrap().close(h).unwrap();
}

#[test]
fn a_creates_overlay_estimate_is_within_twice_its_live_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let rec = Recording::create(
        &tmp.path().join("state"),
        &identity(),
        None,
        &PodContext::default(),
    )
    .unwrap();
    // One create first, so the measured ones start from a warm actor.
    create(&rec, "warm");
    rec.settle().unwrap();
    rec.progress().unwrap();
    let held = agentfs_failpoints::configure("tree.build_before_batch=error@*").unwrap();
    let overlay_before = rec.history().overlay_bytes(TreeId::Root);
    let before = LIVE.load(Ordering::Relaxed);
    for i in 0..CREATES {
        create(&rec, &format!("f{i:05}"));
    }
    let live = LIVE.load(Ordering::Relaxed) - before;
    let overlay = rec.history().overlay_bytes(TreeId::Root) - overlay_before;
    drop(held);
    let ratio = live as f64 / overlay as f64;
    eprintln!(
        "{CREATES} creates with the builder held: {live} live bytes ({} B a create), \
         overlay estimate {overlay} B ({} B a create): {ratio:.2} live bytes per estimated byte",
        live / CREATES as i64,
        overlay / CREATES as u64,
    );
    assert!(
        (0.5..=2.0).contains(&ratio),
        "{live} live bytes for an overlay estimated at {overlay} B: not within a factor of 2"
    );
    rec.settle().unwrap();
}
