//! Small traces through the real library: what the replay catches, and the findings
//! it found (MODEL.md, 36 to 49), each asserting the model's result. These need `git`
//! only: no mount.
use agentfs_conformance::{against_library, against_model};
use agentfs_model::calls::{
    Caller, Client, Recorded, RecordedCall, Took, TreeCalls, TreeId, parse, render,
};
use agentfs_model::recording::Write;
use agentfs_model::world::{Policies, World};

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

/// Every take the world's queues offer.
fn drain(w: &mut World) {
    loop {
        let mut took = false;
        for t in [TreeId::ROOT, TreeId::new(1)] {
            if w.take_next(t).unwrap() == Took::Taken {
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
    let f = w.create_fork(TreeId::ROOT);
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
        .get_mut(&TreeId::ROOT)
        .and_then(|t| t.get_mut("c"))
        .unwrap();
    file[0] = b'!';
    let e = against_library(&trace).unwrap_err();
    assert!(e.contains("T0:c"), "{e}");
}

/// A crash right after a take, with another file open: the take is one step, so it
/// wrote whole before the crash (finding 49), and the restart saves the open.
#[test]
fn a_crash_after_a_take_keeps_it_and_saves_the_open() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    save(&mut w, &Client::Fork(f), "c", &[at(2, "s\n")]);
    let open = w.open_write(&m1(), "a");
    w.write(open, at(0, "m\n"));
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Taken);
    World::crash(&mut w);
    assert_eq!(w.queue_len(TreeId::ROOT), 0);
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
    let f = w.create_fork(TreeId::ROOT);
    let h = w.open_trunc(&Client::Fork(f), "c");
    w.write(h, at(0, "-\n-\n-\n-\n-\n-\n"));
    World::close(&mut w, h);
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Empty);
    passes(&w);
}

/// Finding 37 (the owner, 2026-11-07, "2. B"): `O_TRUNC` then an append is a rewrite
/// whichever call wrote the bytes, so they align with the old lines: a kept `-` is the
/// old line, and a sandbox's edit of that line lands. Model and library both.
#[test]
fn finding_37_a_truncate_then_an_append_is_a_rewrite() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
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
        shown.trees[&TreeId::ROOT]["c"].as_slice(),
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
    let f = w.create_fork(TreeId::ROOT);
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
    let f = w.create_fork(TreeId::ROOT);
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
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    save(
        &mut w,
        &m1(),
        "c",
        &[Write::Truncate(0), at(0, "1\n2\n3\n4\n5\n6\n")],
    );
    assert_eq!(w.take_next(TreeId::new(1)).unwrap(), Took::Taken);
    save(&mut w, &m1(), "c", &[at(0, "M")]);
    save(&mut w, &s, "c", &[Write::Truncate(8), at(0, "S")]);
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Taken);
    save(&mut w, &s, "c", &[Write::Truncate(6), at(6, "yy")]);
    assert_eq!(w.take_next(TreeId::new(1)).unwrap(), Took::Taken);
    assert_eq!(
        w.shown().trees[&TreeId::new(1)]["c"].as_slice(),
        b"M\n2\n3\nyy"
    );
    drain(&mut w);
    assert_eq!(
        w.shown().trees[&TreeId::new(1)]["c"].as_slice(),
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
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    save(&mut w, &s, "a", &[at(2, "s\n")]);
    let open = w.open_write(&s, "a");
    save(&mut w, &m1(), "a", &[at(2, "m\n")]);
    assert_eq!(w.take_next(TreeId::new(1)).unwrap(), Took::Waiting);
    World::close(&mut w, open);
    assert_eq!(w.take_next(TreeId::new(1)).unwrap(), Took::Taken);
    passes(&w);
}

/// Finding 39: an atomic save in a sandbox loses to main's edit of the file it
/// replaced. The new file comes back, so each identity needs its own object: the view's
/// bytes match the commit's only if `t` and `c` are two objects.
#[test]
fn finding_39_a_losing_atomic_save_keeps_two_files_apart() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    let h = World::create(&mut w, &s, "t").unwrap();
    w.write(h, Write::Append("n\n-\n-\n-\n-\n-\n".into()));
    World::close(&mut w, h);
    World::rename(&mut w, &s, "t", "c");
    save(&mut w, &m1(), "c", &[at(0, "m\n")]);
    drain(&mut w);
    let shown = w.shown();
    let sandbox = &shown.trees[&TreeId::new(1)];
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
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    let old = w.open_write(&s, "c");
    w.write(old, at(0, "x\n"));
    World::unlink(&mut w, &s, "c");
    save(&mut w, &m1(), "c", &[at(0, "m\n")]);
    drain(&mut w);
    World::close(&mut w, old);
    drain(&mut w);
    assert_eq!(w.orphans().len(), 1, "the old writer saved nothing");
    let shown = w.shown();
    assert_eq!(
        shown.trees[&TreeId::new(1)]["c"].as_slice(),
        b"m\n-\n-\n-\n-\n-\n"
    );
    passes(&w);
}

/// R11.3 again: a sandbox's rename over a file it did not just create loses, and the
/// replaced file comes back. A writer that held it open still reaches no file.
#[test]
fn an_old_writer_stays_orphaned_when_the_rename_over_loses() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    let old = w.open_write(&s, "c");
    w.write(old, at(0, "x\n"));
    World::rename(&mut w, &s, "b", "c");
    save(&mut w, &m1(), "c", &[at(0, "m\n")]);
    drain(&mut w);
    World::close(&mut w, old);
    drain(&mut w);
    assert_eq!(w.orphans().len(), 1, "the old writer saved nothing");
    passes(&w);
}

/// Finding 44 (the owner, 2026-11-07, his 3, option a): main unlinks a file a sandbox has
/// renamed, and the rename is undecided, so the sandbox keeps the file under its name, and
/// its lines until the parent decides. Then the rename loses, and the file goes.
#[test]
fn finding_44_a_kept_rename_of_a_file_the_parent_unlinked_keeps_its_lines() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    let lines = w.shown().trees[&TreeId::ROOT]["a"].clone();
    assert!(!lines.is_empty());
    World::rename(&mut w, &s, "a", "e");
    World::unlink(&mut w, &m1(), "a");
    assert_eq!(w.take_next(TreeId::new(1)).unwrap(), Took::Taken);
    assert_eq!(w.shown().trees[&TreeId::new(1)]["e"], lines);
    let undecided = w.trace().len();
    drain(&mut w);
    assert!(w.trace().len() > undecided);
    assert!(!w.shown().trees[&TreeId::new(1)].contains_key("e"));
    passes(&w);
}

/// Finding 48 (the owner, 2026-11-08, "A"; R11.5): the upstream edited the file before its
/// unlink. The edit arrives, as a sync brings any decided line (R3d), and the fork's
/// undecided line stays (R3a), in the model and the library alike. The library's sync
/// commit then holds a line neither of its git parents holds, since an upstream that ended
/// a file stores no map of it; a commit in the merge's own history wrote it, so
/// `one_writer` accepts it.
#[test]
fn finding_48_a_kept_file_takes_the_upstreams_last_lines_and_keeps_its_own() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    save(&mut w, &s, "a", &[at(2, "s\n")]);
    World::rename(&mut w, &s, "a", "e");
    save(&mut w, &m1(), "a", &[at(4, "m\n")]);
    World::unlink(&mut w, &m1(), "a");
    assert_eq!(w.take_next(TreeId::new(1)).unwrap(), Took::Taken);
    let kept = &w.shown().trees[&TreeId::new(1)]["e"];
    assert!(kept.starts_with(b"-\ns\nm\n"), "{kept:?}");
    passes(&w);
}

/// Finding 45 (the owner, 2026-11-07, his 4, option a): a sandbox's save landing on main's
/// current head is a fast-forward, in the library too. So there is nothing to deliver before
/// main's next commit, which waits for an open file.
#[test]
fn finding_45_a_landing_on_the_parents_head_is_a_fast_forward() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    save(&mut w, &s, "a", &[at(0, "s\n")]);
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Taken);
    let _held = w.open_write(&s, "a");
    save(&mut w, &m1(), "a", &[at(2, "m\n")]);
    assert_eq!(w.take_next(TreeId::new(1)).unwrap(), Took::Waiting);
    passes(&w);
}

/// Finding 46 (the owner, 2026-11-07, his 5, option a; R9.1): a landing locks the names its
/// change touches too. A landing's unlink of `a` waits while main's open of another file
/// holds the name `a`, the close ends the wait, and the take goes.
#[test]
fn finding_46_a_landing_waits_for_an_open_file_that_took_a_name_it_touches() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    let held = w.open_write(&m1(), "b");
    World::rename(&mut w, &m1(), "b", "a");
    World::unlink(&mut w, &s, "a");
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Waiting);
    World::close(&mut w, held);
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Taken);
    passes(&w);
}

/// Finding 47, seed 484: a sandbox's unlink agrees with main's unlink of the same file,
/// though main edited the file first and the sandbox never saw that edit (R10.3).
#[test]
fn finding_47_two_unlinks_agree_over_an_unseen_edit() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    save(&mut w, &m1(), "c", &[at(2, "m\n")]);
    World::unlink(&mut w, &m1(), "c");
    World::unlink(&mut w, &s, "c");
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Taken);
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
    passes(&w);
}

/// Finding 47, seed 484, as the seed had it: the sandbox's delete is half of a rename-over,
/// which lands whole, so `b` takes the name `c`.
#[test]
fn finding_47_a_rename_over_agrees_with_an_unlink_over_an_unseen_edit() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    save(&mut w, &m1(), "c", &[at(2, "m\n")]);
    World::unlink(&mut w, &m1(), "c");
    World::rename(&mut w, &s, "b", "c");
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Taken);
    assert!(w.collisions.is_empty(), "{:?}", w.collisions);
    let shown = w.shown();
    assert!(shown.trees[&TreeId::ROOT].contains_key("c"));
    assert!(!shown.trees[&TreeId::ROOT].contains_key("b"));
    passes(&w);
}

/// Finding 46, seed 124: a sync that changes no line or name of the sandbox's own view of a
/// file locks no name of it. The sandbox replaced `d` with `a` by a rename-over, undecided,
/// so main's edit of its old `d` keeps no name there, and the sandbox's open of the new `d`
/// does not hold the sync back.
#[test]
fn finding_46_a_sync_of_a_file_the_sandbox_replaced_passes_the_name_it_took() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    World::rename(&mut w, &s, "a", "d");
    save(&mut w, &m1(), "d", &[at(2, "m\n")]);
    let _held = w.open_write(&s, "d");
    assert_eq!(w.take_next(TreeId::new(1)).unwrap(), Took::Taken);
    passes(&w);
}

/// Finding 49 (seeds 78, 164 and 189; the owner, 2026-11-08, "B"): a take is one atomic
/// step in the model too. Main takes `c`'s landing while `d`'s waits for an open file: the
/// take has written `c` when `take_next` returns, before the close that frees `d`'s landing
/// and main's next save. The trace is the order the calls ran in, so a fresh model and the
/// library take the same entries at the same calls.
#[test]
fn finding_49_a_take_is_one_step_so_its_trace_replays() {
    let mut w = world();
    let tree = TreeId::ROOT;
    let (f, g) = (w.create_fork(tree), w.create_fork(tree));
    let held = w.open_write(&m1(), "d");
    save(&mut w, &Client::Fork(f), "d", &[at(2, "s\n")]);
    save(&mut w, &Client::Fork(g), "c", &[at(2, "t\n")]);
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Taken);
    assert_eq!(
        &w.shown().trees[&TreeId::ROOT]["c"][..4],
        b"-\nt\n",
        "c's landing is written when its take returns"
    );
    World::close(&mut w, held);
    save(&mut w, &m1(), "a", &[at(2, "m\n")]);
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Taken);
    assert_eq!(&w.shown().trees[&TreeId::ROOT]["d"][..4], b"-\ns\n");
    let takes: Vec<&RecordedCall> = w
        .trace()
        .iter()
        .map(|r| &r.call)
        .filter(|c| matches!(c, RecordedCall::Take { .. }))
        .collect();
    assert_eq!(takes.len(), 2, "one recorded call per take: {takes:?}");
    passes(&w);
}

/// Finding 51 (seed 198 of the phase 1 generator, 2026-11-08; R6.1): nothing of the fork
/// waits, but a sync to the upstream's commit shows a held-open file differently, so it
/// is a merge, not a fast-forward: the model keeps the file's saved line, and so must the
/// library, and the replay's `history_shape` reads the same merge.
#[test]
fn finding_51_a_sync_that_cannot_show_the_upstreams_commit_merges() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    save(&mut w, &m1(), "a", &[at(4, "n2\n")]);
    World::rename(&mut w, &m1(), "d", "c");
    let held = w.open_write(&s, "d");
    save(&mut w, &m1(), "a", &[at(0, "n3\n")]);
    save(&mut w, &s, "d", &[at(0, "n4\n")]);
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Taken);
    w.write(held, at(0, "-\n-\n-\n-\n-\n-\n"));
    w.write(held, Write::Truncate(12));
    assert_eq!(w.take_next(TreeId::new(1)).unwrap(), Took::Taken);
    passes(&w);
}

/// Finding 51 (CI seed 37 of the phase 1 generator, 2026-11-08; R6.1, R6.2): a fork's save
/// loses to main's, and the fork's sync can then fast-forward to main's landing, since that
/// landing holds the fork's commit and nothing of the fork waits. The library merged instead,
/// because its fork still listed the lost version in its line map's ancestry, so the fork's
/// next save landed as a merge too, and a later sync had one more commit to deliver past an
/// open file: it answered Taken where the model waits.
#[test]
fn finding_51_a_sync_after_a_lost_save_fast_forwards() {
    let mut w = world();
    let f = w.create_fork(TreeId::ROOT);
    let s = Client::Fork(f);
    save(&mut w, &s, "c", &[at(2, "s\n")]);
    save(&mut w, &m1(), "c", &[at(2, "m\n")]);
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Taken);
    assert_eq!(w.collisions.len(), 1, "the fork's save lost");
    assert_eq!(w.take_next(TreeId::new(1)).unwrap(), Took::Taken);
    assert_eq!(w.head(f), w.head(TreeId::ROOT));
    let _held = w.open_write(&s, "c");
    save(&mut w, &s, "a", &[at(2, "t\n")]);
    assert_eq!(w.take_next(TreeId::ROOT).unwrap(), Took::Taken);
    save(&mut w, &m1(), "c", &[at(4, "n\n")]);
    assert_eq!(w.take_next(TreeId::new(1)).unwrap(), Took::Waiting);
    passes(&w);
}
