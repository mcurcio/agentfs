//! Small traces through the real library: what the replay catches, and the findings
//! it found (MODEL.md, 36 to 41), each asserting the model's result. These need `git`
//! only: no mount.
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

/// Finding 36 (the owner, 2026-11-07, "1. A"): a save that changes no byte makes no
/// commit and no queue entry, in the model and in the real library.
#[test]
fn finding_36_a_save_that_changes_nothing_commits_nothing() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    let h = w.open_trunc(&Client::Fork(f), "c");
    w.write(h, at(0, "-\n-\n-\n-\n-\n-\n"));
    World::close(&mut w, h);
    assert_eq!(w.take_next(TreeId::Main).unwrap(), Took::Empty);
    passes(&w);
}

/// Finding 37 (the owner, 2026-11-07, "2. B"): `O_TRUNC` then an append is a rewrite
/// whichever call wrote the bytes, so they align with the old lines: a kept `-` is the
/// old line, and a sandbox's edit of that line lands. Model and library both.
#[test]
fn finding_37_a_truncate_then_an_append_is_a_rewrite() {
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
    let shown = w.shown();
    assert_eq!(
        shown.trees[&TreeId::Main]["c"].as_slice(),
        b"n\nyy",
        "the sandbox's edit of the kept line lands"
    );
    passes(&w);
}

/// Finding 40 was finding 37 in another form: an open whose calls mix an append with an
/// offset write. Before the owner's "2. B" the model made the append's line new first
/// and the library aligned the whole result, so one line of repeated lines sat elsewhere.
/// Now the whole result aligns in both, and a sandbox's edit lands in the same place.
#[test]
fn finding_40_an_append_mixed_with_an_offset_write_is_a_rewrite() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    let h = w.open_write(&m1(), "c");
    w.write(h, Write::Append("-\n".into()));
    w.write(h, at(0, "n\n"));
    World::close(&mut w, h);
    save(&mut w, &Client::Fork(f), "c", &[at(10, "s\n")]);
    drain(&mut w);
    passes(&w);
}

/// Finding 41 was the same again: a pwrite and then an append. The model aligned the
/// bytes before the append and the library the whole result, which only showed in what
/// a collision names, and only on a release of git whose diff split that hunk.
#[test]
fn finding_41_an_offset_write_then_an_append_is_one_alignment() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    let s = Client::Fork(f);
    let h = w.open_write(&s, "c");
    w.write(h, at(4, "n8\n-\n-\n-\n"));
    World::close(&mut w, h);
    let h = w.open_write(&s, "c");
    w.write(h, at(0, "n6\n-\n-\n-\n-\n-\n"));
    w.write(h, Write::Append("}\n".into()));
    World::close(&mut w, h);
    save(&mut w, &m1(), "c", &[at(0, "m\n")]);
    drain(&mut w);
    assert!(!w.collisions.is_empty(), "the sandbox's saves lose to main");
    passes(&w);
}

/// Finding 43's calls: R10.9 hides lines the sandbox itself deleted in a save that lost.
/// Model and library agree on what each tree shows; the open question is only the
/// model's check (`finding_43_...` in the model's scenarios, ignored).
#[test]
fn finding_43_model_and_library_agree_on_a_hidden_line_the_sandbox_deleted() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    let s = Client::Fork(f);
    save(
        &mut w,
        &m1(),
        "c",
        &[Write::Truncate(0), at(0, "1\n2\n3\n4\n5\n6\n")],
    );
    assert_eq!(w.take_next(TreeId::Fork(1)).unwrap(), Took::Taken);
    w.finish_take(TreeId::Fork(1)).unwrap();
    save(&mut w, &m1(), "c", &[at(0, "M")]);
    save(&mut w, &s, "c", &[Write::Truncate(8), at(0, "S")]);
    assert_eq!(w.take_next(TreeId::Main).unwrap(), Took::Taken);
    w.finish_take(TreeId::Main).unwrap();
    save(&mut w, &s, "c", &[Write::Truncate(6), at(6, "yy")]);
    assert_eq!(w.take_next(TreeId::Fork(1)).unwrap(), Took::Taken);
    w.finish_take(TreeId::Fork(1)).unwrap();
    assert_eq!(
        w.shown().trees[&TreeId::Fork(1)]["c"].as_slice(),
        b"M\n2\n3\nyy"
    );
    drain(&mut w);
    assert_eq!(
        w.shown().trees[&TreeId::Fork(1)]["c"].as_slice(),
        b"M\n2\n3\n4\n5\n6\n"
    );
    passes(&w);
}

/// Finding 38 (the owner, 2026-11-07, "3. B"): any change to the line map counts as a
/// change for a sync past an open file. The sandbox keeps its own undecided line, so
/// its file would read the same, and the sync waits all the same, model and library.
#[test]
fn finding_38_a_sync_past_an_open_file_waits_for_any_line_map_change() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    let s = Client::Fork(f);
    save(&mut w, &s, "a", &[at(2, "s\n")]);
    let open = w.open_write(&s, "a");
    save(&mut w, &m1(), "a", &[at(2, "m\n")]);
    assert_eq!(w.take_next(TreeId::Fork(1)).unwrap(), Took::Waiting);
    World::close(&mut w, open);
    assert_eq!(w.take_next(TreeId::Fork(1)).unwrap(), Took::Taken);
    w.finish_take(TreeId::Fork(1)).unwrap();
    passes(&w);
}

/// Finding 39: an atomic save in a sandbox loses to main's edit of the file it
/// replaced. The new file comes back, so each identity needs its own object: the view's
/// bytes match the commit's only if `t` and `c` are two objects.
#[test]
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
    let shown = w.shown();
    let sandbox = &shown.trees[&TreeId::Fork(1)];
    assert_eq!(sandbox["c"].as_slice(), b"m\n-\n-\n-\n-\n-\n");
    assert_eq!(sandbox["t"].as_slice(), b"n\n-\n-\n-\n-\n-\n");
    passes(&w);
}

/// R11.3, the same mechanism as finding 39: a sandbox's unlink loses to main's edit, so
/// the file comes back. A writer that held the unlinked file open still reaches no
/// file: the file that came back is a new object.
#[test]
fn an_old_writer_stays_orphaned_when_the_unlink_loses() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    let s = Client::Fork(f);
    let old = w.open_write(&s, "c");
    w.write(old, at(0, "x\n"));
    World::unlink(&mut w, &s, "c");
    save(&mut w, &m1(), "c", &[at(0, "m\n")]);
    drain(&mut w);
    World::close(&mut w, old);
    drain(&mut w);
    assert_eq!(w.orphans.len(), 1, "the old writer saved nothing");
    let shown = w.shown();
    assert_eq!(
        shown.trees[&TreeId::Fork(1)]["c"].as_slice(),
        b"m\n-\n-\n-\n-\n-\n"
    );
    passes(&w);
}

/// R11.3 again: a sandbox's rename over a file it did not just create loses, and the
/// replaced file comes back. A writer that held it open still reaches no file.
#[test]
fn an_old_writer_stays_orphaned_when_the_rename_over_loses() {
    let mut w = world();
    let f = w.create_fork(agentfs_model::world::Tree::Main);
    let s = Client::Fork(f);
    let old = w.open_write(&s, "c");
    w.write(old, at(0, "x\n"));
    World::rename(&mut w, &s, "b", "c");
    save(&mut w, &m1(), "c", &[at(0, "m\n")]);
    drain(&mut w);
    World::close(&mut w, old);
    drain(&mut w);
    assert_eq!(w.orphans.len(), 1, "the old writer saved nothing");
    passes(&w);
}
