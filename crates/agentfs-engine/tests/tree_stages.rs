//! The actor's stage measurements (git-model §18, ledger R2): what a save does
//! on the call path and in the builder behind it is counted by stage and by Git
//! child, through a real `TreeActor` on real Git.
//! Measurements are never read by a decision; these tests read them.
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use agentfs_contracts::pod::PodContext;
use agentfs_engine::tree_actor::stages::Stages;
use agentfs_engine::tree_actor::{Recording, Saved, Took, TreeActor, TreeId};
use trees::{ATTRS, Volumes, identity, new_actor, reopen, rowless_fork, writer};

#[path = "support/trees.rs"]
mod trees;

/// The counters are process-wide: one test reads them at a time. A test that
/// fails holding the lock poisons it; the others still take it and run.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn put(v: &mut TreeActor, path: &str, bytes: &[u8]) {
    let h = v
        .write_open(TreeId::Root, path, &writer("test-writer"), Some(ATTRS))
        .unwrap();
    v.truncate(h, 0).unwrap();
    v.write(h, 0, bytes, false).unwrap();
    v.close(h).unwrap();
}

fn count(stages: &Stages, before: &Stages) -> Vec<(&'static str, u64)> {
    let diff =
        |name,
         a: agentfs_engine::tree_actor::stages::Stage,
         b: agentfs_engine::tree_actor::stages::Stage| { (name, a.count - b.count) };
    vec![
        diff(
            "journal_append",
            stages.journal_append,
            before.journal_append,
        ),
        diff("journal_fsync", stages.journal_fsync, before.journal_fsync),
        diff("compaction", stages.compaction, before.compaction),
        diff("align", stages.align, before.align),
        diff("objects", stages.objects, before.objects),
        diff("ref_copy", stages.ref_copy, before.ref_copy),
        diff("checkout", stages.checkout, before.checkout),
        diff("queue_plan", stages.queue_plan, before.queue_plan),
        diff("snapshot_read", stages.snapshot_read, before.snapshot_read),
        diff("judge", stages.judge, before.judge),
        diff("sync_merge", stages.sync_merge, before.sync_merge),
        diff(
            "prior_decisions",
            stages.prior_decisions,
            before.prior_decisions,
        ),
        diff("event_owners", stages.event_owners, before.event_owners),
        ("git_children", stages.git_children - before.git_children),
        ("object_writes", stages.object_writes - before.object_writes),
        (
            "object_dir_syncs",
            stages.object_dir_syncs - before.object_dir_syncs,
        ),
    ]
}

/// One save of one file on the root, built: the builder runs two Git children,
/// `git diff` aligning the lines and `git fast-import` packing the commit; the
/// call path runs none.
const CHILDREN_PER_SAVE: u64 = 2;

/// The objects one save feeds `git fast-import` by mark: the changed file's
/// blob and the commit. Its line map and name history are inline, its trees
/// fast-import's; an unchanged file's blob is already known.
const OBJECTS_PER_SAVE: u64 = 2;

/// The objects one save of a file stores, counted by id: the file's
/// blob and line map, the seven folders on their two paths (the commit's own
/// tree, `files/`, `files/<aa>/`, `files/<aa>/<bb>/` and the same under
/// `lines/`) and the commit. Its name history is unchanged.
const STORED_PER_SAVE: u64 = 10;

/// Saves of an existing file, each built in its own batch: what the builder
/// does for each, stage by stage.
#[test]
fn stages_add_up() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(&tmp.path().join("state"));
    put(&mut v, "a", b"seed\n");
    v.settle().unwrap();
    let before = v.metrics();
    const SAVES: u64 = 5;
    for i in 0..SAVES {
        put(&mut v, "a", format!("line {i}\n").as_bytes());
        v.settle().unwrap();
    }
    let after = v.metrics();
    let mut got = count(&after.stages, &before.stages);
    let (_, syncs) = got.pop().unwrap();
    let expect = [
        // the open, the truncate, the write, the close's record, its layout
        // chunk, and the close as a step.
        ("journal_append", 6 * SAVES),
        // A call is appended with no fsync; the builder makes a batch durable
        // before it reads it.
        ("journal_fsync", SAVES),
        ("compaction", 0),
        ("align", SAVES),
        // The edits and the pack's install.
        ("objects", 2 * SAVES),
        ("ref_copy", SAVES),
        ("checkout", SAVES),
        ("queue_plan", 0),
        ("snapshot_read", 0),
        ("judge", 0),
        ("sync_merge", 0),
        ("prior_decisions", 0),
        ("event_owners", 0),
        ("git_children", CHILDREN_PER_SAVE * SAVES),
        ("object_writes", OBJECTS_PER_SAVE * SAVES),
    ];
    assert_eq!(got, expect);
    // Each batch fsyncs its pack's files (pack, index and maybe a reverse index).
    assert!(syncs >= 2 * SAVES, "{syncs} pack files fsynced");
    assert_eq!(after.saves - before.saves, SAVES);
    assert!(after.journal_bytes > before.journal_bytes);
    // Durations are only measured, never exact: each ran and none ran backwards.
    assert!(after.stages.journal_append.nanos > before.stages.journal_append.nanos);
    assert!(after.stages.objects.max_nanos > 0);
}

/// A write costs its own bytes at any file size: a 4 KiB write into a 1 GiB
/// file (sparse, in the engine) copies 4 KiB, its frame's tail, and nothing
/// of the file (docs/trees.md, "File maps").
#[test]
fn bytes_copied_per_write_is_its_own() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(&tmp.path().join("state"));
    let h = v
        .write_open(TreeId::Root, "big", &writer("w"), Some(ATTRS))
        .unwrap();
    v.truncate(h, 1 << 30).unwrap();
    v.write(h, 0, &[1; 4096], false).unwrap();
    // Nothing of the builder's runs while this is measured: no step is unbuilt.
    v.settle().unwrap();
    let before = Stages::current().bytes_copied;
    v.write(h, 512 << 20, &[2; 4096], false).unwrap();
    let copied = Stages::current().bytes_copied - before;
    assert_eq!(
        copied, 4096,
        "a 4 KiB write into a 1 GiB file copied {copied} bytes"
    );
    assert_eq!(v.read_open(h).unwrap().len(), 1 << 30);
}

/// A close that changes nothing is told by comparing only the open's own
/// runs, never the whole file.
#[test]
fn unchanged_compares_only_the_opens_runs() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(&tmp.path().join("state"));
    let file: Vec<u8> = (0..(1 << 20)).map(|i| (i % 251) as u8).collect();
    put(&mut v, "a", &file);
    v.settle().unwrap();
    let h = v.write_open(TreeId::Root, "a", &writer("w"), None).unwrap();
    let at = 300_000;
    v.write(h, at as u64, &file[at..at + 4096], false).unwrap();
    v.write(h, (at + 1024) as u64, &file[at + 1024..at + 2048], false)
        .unwrap();
    let before = Stages::current().bytes_compared;
    assert_eq!(v.close(h).unwrap(), Saved::Unchanged);
    let compared = Stages::current().bytes_compared - before;
    assert_eq!(
        compared, 4096,
        "the open's runs are 4096 bytes; {compared} compared"
    );
}

/// The call path runs no Git and writes no object: with the builder held, a
/// create, writes, a FLUSH, a close, a rename and an unlink start no Git
/// child and do one journal fsync, the FLUSH's.
#[cfg(feature = "failpoints")]
#[test]
fn the_call_path_runs_no_git() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let rec = Recording::create(
        &tmp.path().join("state"),
        &identity(),
        None,
        &PodContext::default(),
    )
    .unwrap();
    let w = writer("test-writer");
    let held = agentfs_failpoints::configure("tree.build_before_batch=error@*").unwrap();
    let before = Stages::current();
    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, "a", &w, Some(ATTRS))
        .unwrap();
    rec.actor()
        .unwrap()
        .write(h, 0, &[b'x'; 4096], false)
        .unwrap();
    rec.durable(h).unwrap();
    rec.actor().unwrap().close(h).unwrap();
    rec.actor()
        .unwrap()
        .rename(
            TreeId::Root,
            "a",
            "b",
            agentfs_engine::tree_actor::RenameMode::Replace,
            &w,
        )
        .unwrap();
    rec.actor().unwrap().unlink(TreeId::Root, "b", &w).unwrap();
    let after = Stages::current();
    drop(held);
    assert_eq!(after.git_children - before.git_children, 0);
    assert_eq!(after.object_writes - before.object_writes, 0);
    assert_eq!(after.object_dir_syncs - before.object_dir_syncs, 0);
    assert_eq!(after.align.count - before.align.count, 0);
    assert_eq!(after.journal_fsync.count - before.journal_fsync.count, 1);
    // The builder catches up once let go.
    rec.settle().unwrap();
    assert_eq!(rec.actor().unwrap().files(TreeId::Root).unwrap(), vec![]);
}

/// Each hold of the actor's lock through the call path is one `actor_lock`
/// run, from the lock taken to its release; the longest is kept.
#[test]
fn each_hold_of_the_actor_lock_is_counted() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let rec = Recording::create(
        &tmp.path().join("state"),
        &identity(),
        None,
        &PodContext::default(),
    )
    .unwrap();
    rec.settle().unwrap();
    let before = Stages::current().actor_lock;
    for _ in 0..3 {
        drop(rec.actor().unwrap());
    }
    let held = rec.actor().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    drop(held);
    let after = Stages::current().actor_lock;
    assert_eq!(after.count - before.count, 4);
    assert!(after.max_nanos >= 20_000_000, "the longest hold: {after:?}");
}

/// The holds of the actor's lock each kind of work ran in during `work`,
/// by kind, and the waits to take the lock.
fn holds_during(work: impl FnOnce()) -> (Vec<(&'static str, u64)>, u64) {
    let before = Stages::current();
    work();
    let after = Stages::current();
    let kinds = after
        .lock_hold
        .by_kind()
        .into_iter()
        .zip(before.lock_hold.by_kind())
        .map(|((kind, a), (_, b))| (kind, a.count - b.count))
        .collect();
    (kinds, after.lock_wait.count - before.lock_wait.count)
}

/// How many of this process's threads are named `name`.
fn threads_named(name: &str) -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter(|task| {
            let comm = task.as_ref().unwrap().path().join("comm");
            std::fs::read_to_string(comm).is_ok_and(|n| n.trim_end() == name)
        })
        .count()
}

/// The holds of `kind` in `kinds`.
fn of(kinds: &[(&'static str, u64)], kind: &str) -> u64 {
    kinds
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, n)| *n)
        .unwrap_or_else(|| panic!("no kind {kind}"))
}

/// Each kind of work under the actor's lock is recorded in the hold it
/// runs in (docs/telemetry.md, "The volume lock"): a plain call is
/// `other` only, a close is `close`, a built point reached is `catch_up`,
/// a fork is `fork_copy`, and a take is `take`. Every hold is also one wait
/// to take the lock. No drop runs under the lock: what a catch-up lets go
/// drops on the volume's `tree-drops` thread, so `drops` stays zero.
/// Compaction runs off the lock, on history's compactor: `compaction` counts
/// only the actor's walk onto carried frames, which a volume with nothing
/// carried never makes.
#[test]
fn each_kind_of_hold_is_recorded_when_its_work_runs() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let rec = Recording::create(
        &tmp.path().join("state"),
        &identity(),
        None,
        &PodContext::default(),
    )
    .unwrap();
    rec.settle().unwrap();
    let w = writer("test-writer");

    let (kinds, waits) = holds_during(|| drop(rec.actor().unwrap()));
    assert_eq!(
        kinds,
        vec![
            ("compaction", 0),
            ("catch_up", 0),
            ("drops", 0),
            ("close", 0),
            ("take", 0),
            ("fork_copy", 0),
            ("page_image", 0),
            ("other", 1),
        ]
    );
    assert_eq!(waits, 1);

    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, "a", &w, Some(ATTRS))
        .unwrap();
    rec.actor().unwrap().write(h, 0, b"base\n", false).unwrap();
    // The close's step is built after its hold or, rarely, within it:
    // either that hold or the built event's catches up.
    let (kinds, _) = holds_during(|| {
        rec.close_with(h, |_| Ok(())).unwrap();
        rec.settle().unwrap();
        rec.progress().unwrap();
    });
    assert!(of(&kinds, "close") >= 1, "{kinds:?}");
    assert!(of(&kinds, "catch_up") >= 1, "{kinds:?}");
    assert_eq!(of(&kinds, "drops"), 0, "{kinds:?}");
    assert!(
        threads_named("tree-drops") >= 1,
        "the volume's drop thread runs"
    );
    assert_eq!(of(&kinds, "take") + of(&kinds, "fork_copy"), 0);
    assert_eq!(of(&kinds, "compaction"), 0);

    let mut fork = TreeId::Root;
    let (kinds, _) = holds_during(|| fork = rowless_fork(&mut rec.actor().unwrap()));
    assert_eq!(of(&kinds, "fork_copy"), 1, "{kinds:?}");

    let h = rec
        .actor()
        .unwrap()
        .write_open(fork, "a", &w, Some(ATTRS))
        .unwrap();
    rec.actor()
        .unwrap()
        .write(h, 0, b"base\nfork\n", false)
        .unwrap();
    rec.close_with(h, |_| Ok(())).unwrap();
    let (kinds, _) = holds_during(|| {
        assert_eq!(rec.take_next(TreeId::Root).unwrap(), Took::Taken);
    });
    assert!(of(&kinds, "take") >= 1, "{kinds:?}");

    // Past the compaction threshold: the close's built event compacts, on
    // history's thread, and no hold of the lock is a compaction's.
    let generation = rec.history().checkpointed().generation;
    let before = Stages::current().checkpoints;
    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, "big", &w, Some(ATTRS))
        .unwrap();
    let chunk = vec![b'x'; 33 << 20];
    for _ in 0..2 {
        rec.actor().unwrap().write(h, 0, &chunk, true).unwrap();
    }
    rec.actor().unwrap().truncate(h, 1).unwrap();
    let (kinds, _) = holds_during(|| {
        rec.close_with(h, |_| Ok(())).unwrap();
        rec.settle().unwrap();
        let published = rec.history().wait_checkpointed(generation).unwrap();
        rec.history().wait_compacted(published.generation).unwrap();
        rec.progress().unwrap();
    });
    assert!(Stages::current().checkpoints > before, "it compacted");
    assert_eq!(of(&kinds, "compaction"), 0, "{kinds:?}");
}

/// A page fill that builds a file's image from a live open's calls reads the
/// journal under the actor's lock: a named hold of its own, `page_image`,
/// and so one the lock's guard lets read (a debug build panics on a journal
/// read under the lock in any hold that is not named for it).
#[test]
fn a_page_fill_under_the_lock_is_a_page_image_hold() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let rec = Recording::create(
        &tmp.path().join("state"),
        &identity(),
        None,
        &PodContext::default(),
    )
    .unwrap();
    let w = writer("test-writer");
    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, "a", &w, Some(ATTRS))
        .unwrap();
    rec.actor().unwrap().write(h, 0, b"calls", false).unwrap();
    let object = rec.actor().unwrap().open_object(h).unwrap();
    let (kinds, _) = holds_during(|| {
        let filled = rec
            .actor()
            .unwrap()
            .page_fill(TreeId::Root, object, 0, 4096)
            .unwrap();
        assert_eq!(filled, b"calls");
    });
    assert_eq!(of(&kinds, "page_image"), 1, "{kinds:?}");
    rec.close_with(h, |_| Ok(())).unwrap();
}

/// A call held at the lag gate is counted under the cause that held it,
/// from its hold to the built event that admits it: here the overlay limit.
#[test]
fn a_gate_wait_is_recorded_by_its_cause() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let rec = Recording::create(
        &tmp.path().join("state"),
        &identity(),
        None,
        &PodContext::default(),
    )
    .unwrap();
    rec.settle().unwrap();
    rec.set_overlay_limit(1);
    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, "a", &writer("test-writer"), Some(ATTRS))
        .unwrap();
    rec.actor().unwrap().write(h, 0, b"a\n", false).unwrap();
    rec.close_with(h, |_| Ok(())).unwrap();
    let before = Stages::current().gate_wait;
    let (tx, rx) = std::sync::mpsc::channel();
    rec.admit_then(
        TreeId::Root,
        Box::new(move |admitted| tx.send(admitted).unwrap()),
    );
    assert!(rx.try_recv().is_err(), "held: the overlay is over 1 byte");
    // Built, then the actor moves onto the built point: the overlay is gone.
    rec.settle().unwrap();
    rec.progress().unwrap();
    rx.recv().unwrap().unwrap();
    let after = Stages::current().gate_wait;
    assert_eq!(after.overlay.count - before.overlay.count, 1);
    assert_eq!(after.lag.count - before.lag.count, 0);
    assert!(after.overlay.nanos > before.overlay.nanos);
}

/// Calls are appended with no fsync, and a FLUSH makes all of an open's
/// records durable with one: close_4k's path (open, truncate, write, FLUSH,
/// RELEASE) does one journal fsync on the call path where it did four. The
/// builder's batch makes the close's record durable with one more.
#[test]
fn a_call_is_not_fsynced_and_flush_fsyncs_once() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let rec = Recording::create(
        &tmp.path().join("state"),
        &identity(),
        None,
        &PodContext::default(),
    )
    .unwrap();
    let w = writer("test-writer");
    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, "a", &w, Some(ATTRS))
        .unwrap();
    rec.settle().unwrap();
    let before = Stages::current();
    {
        let mut v = rec.actor().unwrap();
        v.truncate(h, 0).unwrap();
        for i in 0..8_u64 {
            v.write(h, i * 512, &[b'x'; 512], false).unwrap();
        }
    }
    let written = Stages::current();
    assert_eq!(written.journal_fsync.count, before.journal_fsync.count);
    rec.durable(h).unwrap();
    // Covered already: a second FLUSH finds nothing to sync.
    rec.durable(h).unwrap();
    rec.actor().unwrap().close(h).unwrap();
    rec.settle().unwrap();
    let after = Stages::current();
    assert_eq!(after.journal_fsync.count - before.journal_fsync.count, 2);
    // The truncate, eight writes, the close's record, its layout chunk and
    // its step.
    assert_eq!(after.journal_append.count - before.journal_append.count, 12);
}

/// The distinct objects the store holds, by id. The history's repack runs in
/// the background at each built event and deletes the packs it rolled up only
/// at a later run, so a count of packed copies (`git count-objects`
/// "in-pack") rises and falls with it; nothing prunes, so the set of ids
/// only grows.
fn stored(state: &Path) -> BTreeSet<String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(state.join("history.git"))
        .args([
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
            "--unordered",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The larger tree of the layout tests: 1,000 files locally, 10,000 on
/// GitHub Actions or with `AGENTFS_TEST_VOLUMES=full` (docs/development.md,
/// "Running tests").
fn many() -> usize {
    Volumes::from_env().files(1_000, 10_000)
}

/// Every value of `AGENTFS_TEST_VOLUMES`, on GitHub Actions and off it: unset
/// is the small sizes locally and the full ones in CI, and CI refuses the
/// small sizes, so no workflow can quietly run them.
#[test]
fn the_volume_sizes_are_full_on_actions_and_refused_small_there() {
    use std::env::VarError::{NotPresent, NotUnicode};
    let set = |v: &str| Ok(v.to_owned());
    assert_eq!(Volumes::parse(Err(NotPresent), false), Ok(Volumes::Small));
    assert_eq!(Volumes::parse(Err(NotPresent), true), Ok(Volumes::Full));
    assert_eq!(Volumes::parse(set("small"), false), Ok(Volumes::Small));
    assert!(Volumes::parse(set("small"), true).is_err());
    for on_actions in [false, true] {
        assert_eq!(Volumes::parse(set("full"), on_actions), Ok(Volumes::Full));
        for bad in ["", "Full", "bogus"] {
            assert!(Volumes::parse(set(bad), on_actions).is_err(), "{bad:?}");
        }
        let not_unicode = Err(NotUnicode(std::ffi::OsString::from("full")));
        assert!(Volumes::parse(not_unicode, on_actions).is_err());
    }
}

/// A volume whose root holds `files` files, `f0` to `f<files - 1>`, then `a`,
/// all built. The creates are recorded back to back and built in batches.
fn filled(state: &Path, files: usize) -> TreeActor {
    let mut v = new_actor(state);
    for i in 0..files {
        put(&mut v, &format!("f{i}"), format!("file {i}\n").as_bytes());
    }
    put(&mut v, "a", b"seed\n");
    v.settle().unwrap();
    v
}

/// A save's cost grows with its change, not the tree (Q1-A, hashed
/// sub-folders; docs/trees.md, "What a commit holds"): one changed file in a
/// tree of 100 files and in one of [`many`] stores the same objects, feeds
/// the builder the same, and starts the same Git children. The counts are
/// exact, so any cost that grows with the tree shows at either size. The
/// expected counts already pin the two fixed levels at 100 files; the larger
/// size guards against a cost or a layout that changes with the tree's size.
/// The accepted gap, in a local run only: a change that sets in only between
/// 1,001 and 10,000 files (a third level past some threshold) is not
/// exercised, because 10,000 files cost 50 s of creates. CI runs 10,000.
#[test]
fn a_save_stores_the_same_objects_at_100_files_and_at_many() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let cost = |files: usize| {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let mut v = filled(&state, files);
        let (before, held) = (v.metrics().stages, stored(&state));
        put(&mut v, "a", b"changed\n");
        v.settle().unwrap();
        let after = v.metrics().stages;
        (
            stored(&state).difference(&held).count() as u64,
            after.object_writes - before.object_writes,
            after.git_children - before.git_children,
        )
    };
    let expect = (STORED_PER_SAVE, OBJECTS_PER_SAVE, CHILDREN_PER_SAVE);
    let many = many();
    assert_eq!(cost(100), expect, "at 100 files");
    assert_eq!(cost(many), expect, "at {many} files");
}

/// A file's first bytes are all new lines: the builder takes them as the
/// append case with no `git diff` child, so a create's save starts only
/// `git fast-import`.
#[test]
fn an_empty_files_first_alignment_runs_no_child() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(&tmp.path().join("state"));
    v.settle().unwrap();
    for (i, bytes) in [&b"one line\n"[..], b"a\nb\nno newline", b"\n\n", b"x"]
        .into_iter()
        .enumerate()
    {
        // The create is built first, so the save's batch is its own.
        let h = v
            .write_open(
                TreeId::Root,
                &format!("new{i}"),
                &writer("test-writer"),
                Some(ATTRS),
            )
            .unwrap();
        v.settle().unwrap();
        let before = v.metrics().stages;
        v.write(h, 0, bytes, false).unwrap();
        v.close(h).unwrap();
        v.settle().unwrap();
        let after = v.metrics().stages;
        // Bytes that only extend the empty file are the append case.
        assert_eq!(after.align.count - before.align.count, 0, "not aligned");
        assert_eq!(
            after.git_children - before.git_children,
            1,
            "only fast-import ran for {bytes:?}"
        );
        assert_eq!(v.read(TreeId::Root, &format!("new{i}")).unwrap(), bytes);
    }
}

/// `git hash-object --stdin` of `name`: the id the layout's folders come from.
fn hash_object(name: &str) -> String {
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(name.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The paths of every blob under `top` in the root's head, from `git ls-tree`.
fn listed(state: &Path, top: &str) -> Vec<String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(state.join("history.git"))
        .args(["ls-tree", "-r", "-z", "--name-only", "refs/heads/root", top])
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .split_terminator('\0')
        .map(str::to_owned)
        .collect()
}

/// Each file's bytes and line map sit at `<aa>/<bb>/<name>` and its name
/// history at `<cc>/<dd>/<id>`, the folders hex digits 1-2 and 3-4 of `git
/// hash-object --stdin` of the name or the identity in decimal.
#[test]
fn fanout_matches_git_hash_object() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let mut v = new_actor(&state);
    let names = [
        "a",
        "b.txt",
        "with space",
        "ünï",
        "metadata.json",
        ".hidden",
    ];
    for name in names {
        put(&mut v, name, b"x\n");
    }
    v.settle().unwrap();
    let fanned = |name: &str| {
        let hex = hash_object(name);
        format!("{}/{}/{name}", &hex[..2], &hex[2..4])
    };
    let mut files: Vec<String> = names
        .iter()
        .map(|n| format!("files/{}", fanned(n)))
        .collect();
    let mut lines: Vec<String> = names
        .iter()
        .map(|n| format!("lines/{}", fanned(n)))
        .collect();
    files.sort();
    lines.sort();
    assert_eq!(listed(&state, "files/"), files);
    assert_eq!(listed(&state, "lines/"), lines);
    let ids: Vec<String> = v
        .files(TreeId::Root)
        .unwrap()
        .into_iter()
        .map(|(_, object, _)| object.to_string())
        .collect();
    let mut histories: Vec<String> = ids
        .iter()
        .map(|id| format!("names/{}", fanned(id)))
        .collect();
    histories.sort();
    assert_eq!(listed(&state, "names/"), histories);
}

/// A file's path depends on its name alone: the same at 10 files and at
/// [`many`] (two fixed levels of 256, never a fan-out that grows;
/// docs/trees.md, "What a commit holds"). The expected path pins the two
/// levels at 10 files; the larger size guards against a layout that changes
/// with the tree's size, such as a fan-out that adds a level when a level
/// passes 256 entries. The accepted gap, in a local run only: a change that
/// sets in only between 1,001 and 10,000 files is not exercised, because
/// 10,000 files cost 50 s of creates. CI runs 10,000.
#[test]
fn a_files_path_never_changes_when_siblings_are_added() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let path = |files: usize| {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let _v = filled(&state, files);
        listed(&state, "files/")
            .into_iter()
            .find(|p| p.ends_with("/a"))
            .unwrap()
    };
    let hex = hash_object("a");
    let expect = format!("files/{}/{}/a", &hex[..2], &hex[2..4]);
    let many = many();
    assert_eq!(path(10), expect, "at 10 files");
    assert_eq!(path(many), expect, "at {many} files");
}

/// A save on the root and the sync that carries it to a sandbox must cost the same
/// Git children whether the history holds ten saves or ninety (the pilot found
/// each sync walking the whole history, one child per commit).
#[test]
fn a_sync_costs_the_same_children_at_any_history_length() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let mut v = new_actor(&state);
    rowless_fork(&mut v);
    let cycle = |v: &mut TreeActor, i: u64| {
        v.settle().unwrap();
        let before = Stages::current().git_children;
        put(v, "a", format!("line {i}\n").as_bytes());
        while v.progress().unwrap() {}
        v.settle().unwrap();
        Stages::current().git_children - before
    };
    let mut early = 0;
    let mut late = 0;
    for i in 0..90 {
        let children = cycle(&mut v, i);
        if i == 9 {
            early = children;
        }
        if i == 89 {
            late = children;
        }
    }
    let metrics = v.metrics();
    assert_eq!(metrics.syncs, metrics.saves, "every save's sync was taken");
    assert!(
        late * 10 <= early * 12,
        "children per save and sync: {early} at the tenth, {late} at the ninetieth"
    );
    // A reopened actor rebuilds what the syncs look up, from history.
    drop(v);
    let mut v = reopen(&state);
    let syncs = v.metrics().syncs;
    put(&mut v, "a", b"after reopen\n");
    while v.progress().unwrap() {}
    assert_eq!(v.metrics().syncs, syncs + 1);
}

/// The held-log measurement found a sync of an 8 MiB file starting one Git child
/// per line (8,234 children, 70 seconds). One save is one commit however many
/// lines it wrote, so a sync costs the same children for 10 lines and 600.
#[test]
fn a_sync_costs_the_same_children_for_any_number_of_lines() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    // A volume of its own for each size, so each save meets the same tree.
    let children = |lines: usize| {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = new_actor(&tmp.path().join("state"));
        rowless_fork(&mut v);
        let bytes: Vec<u8> = (0..lines)
            .flat_map(|i| format!("line {i}\n").into_bytes())
            .collect();
        // The sandbox holds the first save's lines when the second one syncs.
        put(&mut v, "a", &bytes);
        while v.progress().unwrap() {}
        v.settle().unwrap();
        let before = Stages::current().git_children;
        put(
            &mut v,
            "a",
            &bytes
                .iter()
                .map(|b| b.to_ascii_uppercase())
                .collect::<Vec<_>>(),
        );
        while v.progress().unwrap() {}
        v.settle().unwrap();
        Stages::current().git_children - before
    };
    let (few, many) = (children(10), children(600));
    assert_eq!(
        few, many,
        "children per save and sync: {few} for 10 lines, {many} for 600"
    );
}
