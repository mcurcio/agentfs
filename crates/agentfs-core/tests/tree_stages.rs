//! The actor's stage measurements (git-model §18, ledger R2): what a save does
//! is counted by stage and by Git child, through a real `Volume` on real Git.
//! Measurements are never read by a decision; these tests read them.
use std::path::Path;
use std::sync::Mutex;

use agentfs_core::tree_actor::Volume;
use agentfs_core::tree_actor::stages::Stages;

/// The counters are process-wide: one test reads them at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn put(v: &mut Volume, path: &str, bytes: &[u8]) {
    let h = v.write_open("main", path, "test-writer", true).unwrap();
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
    ]
}

/// One save of one file on main, step by step. Each save runs the Git programs
/// below, in this many children: hash-object for the file, its tree, the line
/// map, the name history, the envelope tree, the commit, the object table and
/// the queue's marker blob (8); `git diff` aligning the lines (1);
/// `check-ref-format` for the branch and the object table, then `update-ref`
/// (3); `cat-file` of the commit, `ls-tree` of its envelope, `ls-tree` of the
/// files and `cat-file` of the one file (4).
const CHILDREN_PER_SAVE: u64 = 16;

#[test]
fn stages_add_up() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(&tmp.path().join("state")).unwrap();
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
        ("journal_fsync", 4 * SAVES),
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
    ];
    assert_eq!(got, expect);
    assert_eq!(after.saves - before.saves, SAVES);
    assert!(after.journal_bytes > before.journal_bytes);
    // Durations are only measured, never exact: each ran and none ran backwards.
    assert!(after.stages.journal_fsync.nanos > before.stages.journal_fsync.nanos);
    assert!(after.stages.objects.max_nanos > 0);
}

/// A save on main and the sync that carries it to a sandbox must cost the same
/// Git children whether the history holds ten saves or ninety (the pilot found
/// each sync walking the whole history, one child per commit).
#[test]
fn a_sync_costs_the_same_children_at_any_history_length() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let mut v = Volume::open(&state).unwrap();
    v.fork().unwrap();
    let cycle = |v: &mut Volume, i: u64| {
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
    let mut v = Volume::open(&state).unwrap();
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
        let mut v = Volume::open(&tmp.path().join("state")).unwrap();
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
    for entry in std::fs::read_dir(src.join("tree_actor")).unwrap() {
        files.push(entry.unwrap().path());
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
