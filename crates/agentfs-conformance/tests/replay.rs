//! Small traces through the real library: what the replay catches, and the open
//! findings (MODEL.md, 36 to 40), each as an ignored test asserting the model's
//! result. These need `git` only: no mount.
use agentfs_conformance::{against_library, against_model};
use agentfs_model::calls::{
    Caller, Recorded, RecordedCall, Took, TreeCalls, TreeId, parse, render,
};
use agentfs_model::world::{Client, Policies, World, Write};

fn world() -> World {
    World::recording(Policies::default())
}

fn m1() -> Client {
    Client::Main("m1".into())
}

fn save(w: &mut World, c: &Client, p: &str, calls: &[Write]) {
    let h = w.open_write(c, p);
    for call in calls {
        w.write(h, call.clone());
    }
    World::close(w, h);
}

fn at(offset: usize, bytes: &str) -> Write {
    Write::Pwrite {
        offset,
        bytes: bytes.into(),
    }
}

/// Every take the world's queues offer, finished.
fn drain(w: &mut World) {
    loop {
        let mut took = false;
        for t in [TreeId::Main, TreeId::Fork(1)] {
            if w.take_next(t).unwrap() == Took::Taken {
                w.finish_take(t).unwrap();
                took = true;
            }
        }
        if !took {
            return;
        }
    }
}

fn passes(w: &World) {
    let trace = parse(&render(w.trace())).unwrap();
    against_model(&trace).unwrap();
    against_library(&trace).unwrap();
}

/// R3b and R3e: a collision through the real library, its record compared.
#[test]
fn a_collision_replays_the_same_on_the_library() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    save(&mut w, &m1(), "c", &[at(2, "m\n")]);
    save(&mut w, &Client::Fork(f), "c", &[at(2, "s\n")]);
    drain(&mut w);
    assert_eq!(w.collisions.len(), 1);
    passes(&w);
}

/// The compare can fail: one byte of what the trace says main shows, changed.
#[test]
fn a_changed_byte_in_the_trace_is_a_mismatch() {
    let mut w = world();
    save(&mut w, &m1(), "c", &[at(0, "m\n")]);
    let mut trace: Vec<Recorded> = w.trace().to_vec();
    let shown = trace
        .iter_mut()
        .rev()
        .find_map(|r| r.shown.as_mut())
        .unwrap();
    let file = shown
        .trees
        .get_mut(&TreeId::Main)
        .and_then(|t| t.get_mut("c"))
        .unwrap();
    file[0] = b'!';
    let e = against_library(&trace).unwrap_err();
    assert!(e.contains("Main:c"), "{e}");
}

/// A crash in the middle of a take: the model's take went back to its queue, so
/// the replay does not make it, and the entry is taken after the restart.
#[test]
fn a_crash_during_a_take_undoes_it() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    save(&mut w, &Client::Fork(f), "c", &[at(2, "s\n")]);
    let open = w.open_write(&m1(), "a");
    w.write(open, at(0, "m\n"));
    assert_eq!(w.take_next(TreeId::Main).unwrap(), Took::Taken);
    World::crash(&mut w);
    drain(&mut w);
    assert!(w.trace().iter().any(|r| r.call == RecordedCall::Crash));
    passes(&w);
}

/// A client's end closes its opens, and the replay finds the same bytes.
#[test]
fn a_clients_end_saves_its_opens() {
    let mut w = world();
    let h = w.open_write(&m1(), "c");
    w.write(h, at(0, "m\n"));
    TreeCalls::end_client(&mut w, &Caller::Main("m1".into())).unwrap();
    passes(&w);
}

/// Finding 36: a save that changes no byte makes no commit in the model; the real
/// library makes one, and a queue entry for it.
#[test]
#[ignore = "coherence finding 36: a save that leaves the bytes as they were commits, so the queues differ"]
fn finding_36_a_save_that_changes_nothing_commits_nothing() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    let h = w.open_trunc(&Client::Fork(f), "c");
    w.write(h, at(0, "-\n-\n-\n-\n-\n-\n"));
    World::close(&mut w, h);
    assert_eq!(w.take_next(TreeId::Main).unwrap(), Took::Empty);
    passes(&w);
}

/// Finding 37: `O_TRUNC` then an append. In the model the appended lines are new
/// lines (R8: "an append adds") and every old line is deleted, so a sandbox's edit
/// of an old line meets a delete. In the real library the bytes align with the old
/// lines, so that edit lands on a line the save kept.
#[test]
#[ignore = "coherence finding 37: an append after a truncate is aligned in the real library, and is new lines in the model"]
fn finding_37_an_append_after_a_truncate_makes_new_lines() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    let h = w.open_trunc(&m1(), "c");
    w.write(h, Write::Append("-\nyy".into()));
    World::close(&mut w, h);
    save(
        &mut w,
        &Client::Fork(f),
        "c",
        &[at(0, "n\n-\n-\n-\n-\n-\n")],
    );
    drain(&mut w);
    passes(&w);
}

/// Finding 38: a parent's commit that leaves the sandbox's open file as it is, since
/// the sandbox keeps its own undecided line there. The model delivers it (R9.3 holds
/// back a commit that touches the open file); the real library waits, because the
/// file's line map gained that version in its history.
#[test]
#[ignore = "coherence finding 38: the real sync counts a change only to a line's history as touching an open file"]
fn finding_38_a_sync_past_an_open_file_it_leaves_as_it_is() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    let s = Client::Fork(f);
    save(&mut w, &s, "a", &[at(2, "s\n")]);
    let _open = w.open_write(&s, "a");
    save(&mut w, &m1(), "a", &[at(2, "m\n")]);
    assert_eq!(w.take_next(TreeId::Fork(1)).unwrap(), Took::Taken);
    w.finish_take(TreeId::Fork(1)).unwrap();
    passes(&w);
}

/// Finding 39: an atomic save in a sandbox loses to main's edit of the file it
/// replaced. The new file is still there after the sync, so two names hold one real
/// object, and the view's bytes stop matching the commit.
#[test]
#[ignore = "coherence finding 39: after a losing atomic save, two names share one object and the view differs from its commit"]
fn finding_39_a_losing_atomic_save_keeps_two_files_apart() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    let s = Client::Fork(f);
    let h = World::create(&mut w, &s, "t").unwrap();
    w.write(h, Write::Append("n\n-\n-\n-\n-\n-\n".into()));
    World::close(&mut w, h);
    World::rename(&mut w, &s, "t", "c");
    save(&mut w, &m1(), "c", &[at(0, "m\n")]);
    drain(&mut w);
    passes(&w);
}
