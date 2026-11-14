//! The actor's stage measurements (git-model §18, ledger R2): what a save does
//! on the call path and in the builder behind it is counted by stage and by Git
//! child, through a real `TreeActor` on real Git.
//! Measurements are never read by a decision; these tests read them.
use std::path::Path;
use std::sync::Mutex;

use agentfs_contracts::pod::PodContext;
use agentfs_core::tree_actor::stages::Stages;
use agentfs_core::tree_actor::{Recording, Saved, TreeActor, TreeId};
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

/// One save of one file on the root, built: the builder runs two Git children,
/// `git diff` aligning the lines and `git fast-import` packing the commit; the
/// call path runs none.
const CHILDREN_PER_SAVE: u64 = 2;

/// The objects one save feeds `git fast-import` by mark: the changed file's
/// blob and the commit. Its line map and name history are inline, its trees
/// fast-import's; an unchanged file's blob is already known.
const OBJECTS_PER_SAVE: u64 = 2;

/// The objects one save of a file stores, counted in the pack: the file's
/// blob and line map, the seven folders on their two paths (the commit's own
/// tree, `files/`, `files/<aa>/`, `files/<aa>/<bb>/` and the same under
/// `lines/`) and the commit. Its name history is unchanged.
const STORED_PER_SAVE: u64 = 10;

/// Saves of an existing file, each built in its own batch: what the builder
/// does for each, stage by stage.
#[test]
fn stages_add_up() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
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
        // the open, the truncate, the write, and the close as a step.
        ("journal_append", 4 * SAVES),
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
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(&tmp.path().join("state"));
    let h = v
        .write_open(TreeId::Root, "big", &writer("w"), true)
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
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(&tmp.path().join("state"));
    let file: Vec<u8> = (0..(1 << 20)).map(|i| (i % 251) as u8).collect();
    put(&mut v, "a", &file);
    v.settle().unwrap();
    let h = v
        .write_open(TreeId::Root, "a", &writer("w"), false)
        .unwrap();
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
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let rec = Recording::create(
        &tmp.path().join("state"),
        &identity(),
        &PodContext::default(),
    )
    .unwrap();
    let w = writer("test-writer");
    let held = agentfs_core::failpoints::configure("tree.build_before_batch=error@*").unwrap();
    let before = Stages::current();
    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, "a", &w, true)
        .unwrap();
    rec.actor()
        .unwrap()
        .write(h, 0, &[b'x'; 4096], false)
        .unwrap();
    rec.durable(h).unwrap();
    rec.actor().unwrap().close(h).unwrap();
    rec.actor()
        .unwrap()
        .rename(TreeId::Root, "a", "b", &w)
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

/// Calls are appended with no fsync, and a FLUSH makes all of an open's
/// records durable with one: close_4k's path (open, truncate, write, FLUSH,
/// RELEASE) does one journal fsync on the call path where it did four. The
/// builder's batch makes the close's record durable with one more.
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
    let h = rec
        .actor()
        .unwrap()
        .write_open(TreeId::Root, "a", &w, true)
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
    assert_eq!(after.journal_append.count - before.journal_append.count, 10);
}

/// Objects in the store's packs, as `git count-objects` counts them.
fn packed(state: &Path) -> u64 {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(state.join("history.git"))
        .args(["count-objects", "-v"])
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("in-pack: "))
        .unwrap()
        .parse()
        .unwrap()
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
/// sub-folders): one changed file in a tree of 100 files and in one of
/// 10,000 stores the same objects, feeds the builder the same, and starts the
/// same Git children.
#[test]
fn a_save_stores_the_same_objects_at_100_and_10000_files() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let cost = |files: usize| {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let mut v = filled(&state, files);
        let (before, stored) = (v.metrics().stages, packed(&state));
        put(&mut v, "a", b"changed\n");
        v.settle().unwrap();
        let after = v.metrics().stages;
        (
            packed(&state) - stored,
            after.object_writes - before.object_writes,
            after.git_children - before.git_children,
        )
    };
    let expect = (STORED_PER_SAVE, OBJECTS_PER_SAVE, CHILDREN_PER_SAVE);
    assert_eq!(cost(100), expect, "at 100 files");
    assert_eq!(cost(10_000), expect, "at 10,000 files");
}

/// A file's first bytes are all new lines: the builder aligns them with no
/// `git diff` child, so a create's save starts only `git fast-import`.
#[test]
fn an_empty_files_first_alignment_runs_no_child() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
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
                true,
            )
            .unwrap();
        v.settle().unwrap();
        let before = v.metrics().stages;
        v.write(h, 0, bytes, false).unwrap();
        v.close(h).unwrap();
        v.settle().unwrap();
        let after = v.metrics().stages;
        assert_eq!(after.align.count - before.align.count, 1, "aligned once");
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
    let _one = ONE_AT_A_TIME.lock().unwrap();
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
/// 10,000 (two fixed levels of 256, never a fan-out that grows).
#[test]
fn a_files_path_never_changes_when_siblings_are_added() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
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
    assert_eq!(path(10), expect);
    assert_eq!(path(10_000), expect);
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
