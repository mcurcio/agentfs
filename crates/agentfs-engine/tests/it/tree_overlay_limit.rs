//! Backpressure by memory: with the builder held back, the lag gate holds a
//! tree's new work while its overlay (the changes history has not built yet)
//! is over the limit, and admits it at the built event that brings it under
//! (docs/trees.md, "History trails the files").
#![cfg(feature = "failpoints")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agentfs_engine::tree_actor::{Recording, TreeId};
use trees::{ATTRS, identity, writer};

use crate::trees;

/// The limit the test sets: small, so a few hundred creates reach it.
const LIMIT: u64 = 256 << 10;
const CREATES: usize = 5_000;

/// One create through the call path, as a view makes it: the gate first,
/// then a new file written and closed.
fn create(rec: &Recording, path: &str) {
    rec.admit(TreeId::Root).unwrap();
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
fn the_overlay_stays_bounded_under_a_create_burst() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    drop(
        Recording::create(
            &state,
            &identity(),
            None,
            &agentfs_contracts::pod::PodContext::default(),
        )
        .unwrap(),
    );
    let _slow = agentfs_failpoints::configure("tree.build_before_batch=sleep(100)@*").unwrap();
    let rec = Recording::open(&state, &agentfs_contracts::pod::PodContext::default()).unwrap();
    rec.set_overlay_limit(LIMIT);
    // The view's take worker: at each built or failed event it calls
    // `progress`, which moves the actor onto the built point.
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let (rec, stop) = (Arc::clone(&rec), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut epoch = 0;
            while !stop.load(Ordering::Acquire) {
                rec.progress().unwrap();
                epoch = rec.wait_for_work(epoch);
            }
        })
    };
    let history = rec.history();
    create(&rec, "f00000");
    let one = history.overlay_bytes(TreeId::Root);
    assert!(one > 0, "a create adds to the overlay");
    let mut max = one;
    for i in 1..CREATES {
        create(&rec, &format!("f{i:05}"));
        max = max.max(history.overlay_bytes(TreeId::Root));
    }
    eprintln!(
        "{CREATES} creates: the overlay peaked at {max} B, limit {LIMIT} B, one create {one} B"
    );
    assert!(
        max <= LIMIT + one,
        "the overlay reached {max} B, over the limit {LIMIT} B plus one create's {one} B"
    );
    rec.settle().unwrap();
    stop.store(true, Ordering::Release);
    rec.wake_workers();
    worker.join().unwrap();
    rec.progress().unwrap();
    let after = history.overlay_bytes(TreeId::Root);
    assert!(
        after < one,
        "once built, the overlay {after} B is under one create's {one} B"
    );
}
