//! The state lock's heartbeat and the fence it renews (design §14.2), in a
//! process of their own: the fence is process-wide.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agentfs_core::lock::StateLock;
use agentfs_core::volume::{self, StateDir};

#[test]
fn the_heartbeat_renews_the_fence_and_the_journal_stops_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let state = StateDir::new(dir.path().join("state"));
    volume::init(&state, "vol", None).unwrap();
    // No fence yet: nothing is refused.
    agentfs_core::fence::check().unwrap();
    let lock = StateLock::acquire(&state, "serve-history", Duration::ZERO).unwrap();
    let lost = Arc::new(AtomicBool::new(false));
    let flag = lost.clone();
    let beat = lock
        .start_heartbeat(Duration::from_millis(250), move |_| {
            flag.store(true, Ordering::SeqCst);
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(600));
    // Renewed every 50 ms: fresh well past the 250 ms fence.
    agentfs_core::fence::check().unwrap();
    assert!(!lost.load(Ordering::SeqCst));
    let holder = StateLock::holder(&state).unwrap().unwrap();
    assert!(holder.contains("command=serve-history"), "{holder}");

    // The heartbeat stops (as a node cut off from its NFS server stops
    // renewing): after the fence, the journal refuses to write.
    drop(beat);
    std::thread::sleep(Duration::from_millis(400));
    let err = agentfs_core::fence::check().unwrap_err();
    assert!(err.to_string().contains("fence"), "{err}");
    let (journal, _) =
        agentfs_core::journal::Journal::open(dir.path().join("j"), 0, Box::new(|| Ok(()))).unwrap();
    let draft = agentfs_core::record::Draft {
        trigger: agentfs_core::record::Trigger::Close,
        caller: agentfs_core::record::Caller {
            uid: 0,
            gid: 0,
            pid: 1,
        },
        time_ms: 1,
        changes: vec![agentfs_core::record::Change::Remove {
            path: agentfs_core::paths::RepoPath::new("a").unwrap(),
        }],
        operation: None,
        atomic: false,
        origin: None,
    };
    assert!(journal.record(draft).is_err(), "fenced: nothing journaled");
    assert!(journal.uncommitted().is_empty());
}
