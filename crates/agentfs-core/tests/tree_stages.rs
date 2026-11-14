//! The actor's stage measurements (git-model §18, ledger R2): what a save does
//! is counted by stage and by Git child, through a real `TreeActor` on real Git.
//! Measurements are never read by a decision; these tests read them.
use std::path::Path;
use std::sync::Mutex;

use agentfs_contracts::pod::PodContext;
use agentfs_core::tree_actor::stages::Stages;
use agentfs_core::tree_actor::{Recording, TreeActor, TreeId};
use trees::{identity, new_actor, reopen, writer};

#[path = "support/trees.rs"]
mod trees;

/// The counters are process-wide: one test reads them at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn put(v: &mut TreeActor, path: &str, bytes: &[u8]) {
    let h = v
        .write_open(TreeId::Root, path, &writer("test-writer"), true)
        .unwrap();
    v.truncate(h, 0).unwrap();
    v.write(h, 0, bytes, false).unwrap();
    v.close(h).unwrap();
}

fn count(stages: &Stages, before: &Stages) -> Vec<(&'static str, u64)> {
    let diff =
        |name,
         a: agentfs_core::tree_actor::stages::Stage,
         b: agentfs_core::tree_actor::stages::Stage| { (name, a.count - b.count) };
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

/// One save of one file on the root, step by step. A save runs one Git child,
/// `git diff` aligning the lines; its objects, the ref copy and the checkout run
/// in this process.
const CHILDREN_PER_SAVE: u64 = 1;

/// The objects one save hands to Git's store: the changed file's blob, the
/// files tree, the line map, the name history, the envelope tree, the commit
/// and the object table (a sync's marker blob only when the tree has forks, and
/// here it has none). An unchanged file's blob is already known.
const OBJECTS_PER_SAVE: u64 = 7;

#[test]
fn stages_add_up() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(&tmp.path().join("state"));
    put(&mut v, "a", b"seed\n");
    let before = v.metrics();
    const SAVES: u64 = 5;
    for i in 0..SAVES {
        put(&mut v, "a", format!("line {i}\n").as_bytes());
    }
    let after = v.metrics();
    let got = count(&after.stages, &before.stages);
    let expect = [
        // the open, the truncate, the write, and the close as a pointer step.
        ("journal_append", 4 * SAVES),
        // A call is appended with no fsync; only a FLUSH, fsync or FSYNCDIR syncs.
        ("journal_fsync", 0),
        ("compaction", 0),
        ("align", SAVES),
        ("objects", SAVES),
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
        // Each step's new objects are made durable by one fsync of `objects/`
        // before its journal record names them.
        ("object_dir_syncs", SAVES),
    ];
    assert_eq!(got, expect);
    assert_eq!(after.saves - before.saves, SAVES);
    assert!(after.journal_bytes > before.journal_bytes);
    // Durations are only measured, never exact: each ran and none ran backwards.
    assert!(after.stages.journal_append.nanos > before.stages.journal_append.nanos);
    assert!(after.stages.objects.max_nanos > 0);
}

/// Calls are appended with no fsync, and a FLUSH makes all of an open's
/// records durable with one: close_4k's path (open, truncate, write, FLUSH,
/// RELEASE) does one journal fsync where it did four.
#[test]
fn a_call_is_not_fsynced_and_flush_fsyncs_once() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let rec = Recording::create(
        &tmp.path().join("state"),
        &identity(),
        &PodContext::default(),
    )
    .unwrap();
    let w = writer("test-writer");
    let before = Stages::current();
    let h = {
        let mut v = rec.actor().unwrap();
        let h = v.write_open(TreeId::Root, "a", &w, true).unwrap();
        v.truncate(h, 0).unwrap();
        for i in 0..8_u64 {
            v.write(h, i * 512, &[b'x'; 512], false).unwrap();
        }
        h
    };
    let written = Stages::current();
    assert_eq!(written.journal_fsync.count, before.journal_fsync.count);
    rec.durable(h).unwrap();
    // Covered already: a second FLUSH finds nothing to sync.
    rec.durable(h).unwrap();
    rec.actor().unwrap().close(h).unwrap();
    let after = Stages::current();
    assert_eq!(after.journal_fsync.count - before.journal_fsync.count, 1);
    assert_eq!(after.journal_append.count - before.journal_append.count, 11);
}

/// A save's cost grows with its change, not the tree: one changed file in a
/// tree of forty hands Git's store the objects it would in a tree of one, and
/// starts the same Git children.
#[test]
fn a_save_stores_the_same_objects_at_any_tree_size() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let cost = |files: usize| {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = new_actor(&tmp.path().join("state"));
        for i in 0..files {
            put(&mut v, &format!("f{i}"), format!("file {i}\n").as_bytes());
        }
        put(&mut v, "a", b"seed\n");
        let before = v.metrics().stages;
        put(&mut v, "a", b"changed\n");
        let after = v.metrics().stages;
        (
            after.object_writes - before.object_writes,
            after.git_children - before.git_children,
        )
    };
    assert_eq!(cost(0), (OBJECTS_PER_SAVE, CHILDREN_PER_SAVE));
    assert_eq!(cost(40), (OBJECTS_PER_SAVE, CHILDREN_PER_SAVE));
}

/// A save on the root and the sync that carries it to a sandbox must cost the same
/// Git children whether the history holds ten saves or ninety (the pilot found
/// each sync walking the whole history, one child per commit).
#[test]
fn a_sync_costs_the_same_children_at_any_history_length() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let mut v = new_actor(&state);
    v.fork().unwrap();
    let cycle = |v: &mut TreeActor, i: u64| {
        let before = Stages::current().git_children;
        put(v, "a", format!("line {i}\n").as_bytes());
        while v.progress().unwrap() {}
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
    let _one = ONE_AT_A_TIME.lock().unwrap();
    // A volume of its own for each size, so each save meets the same tree.
    let children = |lines: usize| {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = new_actor(&tmp.path().join("state"));
        v.fork().unwrap();
        let bytes: Vec<u8> = (0..lines)
            .flat_map(|i| format!("line {i}\n").into_bytes())
            .collect();
        // The sandbox holds the first save's lines when the second one syncs.
        put(&mut v, "a", &bytes);
        while v.progress().unwrap() {}
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
        Stages::current().git_children - before
    };
    let (few, many) = (children(10), children(600));
    assert_eq!(
        few, many,
        "children per save and sync: {few} for 10 lines, {many} for 600"
    );
}

/// Production sources of the actor's path, up to their first test module.
fn production(path: &Path) -> String {
    let source = std::fs::read_to_string(path).unwrap();
    let end = source.find("#[cfg(test)]").unwrap_or(source.len());
    let end = source[..end].find("#[cfg(all(test").unwrap_or(end);
    source[..end].to_owned()
}

#[test]
fn one_spawn_site() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![src.join("tree_git.rs"), src.join("tree_reconcile.rs")];
    let mut dirs = vec![src.join("tree_actor")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(path);
            }
        }
    }
    for file in files {
        let source = production(&file);
        // `Command` methods take no argument; a thread scope's `spawn` takes one.
        for spawn in [".spawn()", ".output()", ".status()"] {
            assert!(
                !source.contains(spawn),
                "{} starts a process with `{spawn}` outside git_cli::counted",
                file.display()
            );
        }
    }
    let git_cli = production(&src.join("git_cli.rs"));
    assert_eq!(
        git_cli.matches(".spawn()").count(),
        1,
        "git_cli::counted is the one site"
    );
}
