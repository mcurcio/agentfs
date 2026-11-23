//! Pending saves (docs/trees.md, "Recording and durability"): a RELEASE is
//! begun under the lock and answered; its save is decided and landed later
//! by `SaveToDecide::finish`. Meanwhile the file's own client reads its
//! pending content, a delete, a rename onto it, a replace or a path truncate
//! waits for the save, a rename of it goes first, takes hold it, and a
//! file's saves land in the order their closes began. Through `Recording`
//! and `Sessions`, with two read-write clients of the root.
#![allow(
    clippy::disallowed_methods,
    reason = "a test is its own composition root"
)]

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, mpsc};

use agentfs_contracts::caps::{ClientCap, Minter};
use agentfs_contracts::clients::{ClientId, Ordinal};
use agentfs_contracts::name::{ClientLabel, LauncherId};
use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::ports::{SessionAuthority, SessionBinder};
use agentfs_core::session::{Sessions, TreeSession};
use agentfs_core::tree_actor::{
    Closing, PathStep, Recording, RenameMode, SaveToDecide, Saved, Took, TreeId,
};
use trees::{ATTRS, identity, rowless_fork, writer};

#[path = "support/trees.rs"]
mod trees;

const ROOT: TreeId = TreeId::Root;

/// A volume served, with its sessions.
fn serve(dir: &Path) -> (Arc<Recording>, Sessions) {
    let rec = Recording::create(dir, &identity(), None, &PodContext::default()).unwrap();
    let sessions = Sessions::new(Arc::clone(&rec));
    (rec, sessions)
}

/// A session bound to write the root as client `label`.
fn client(sessions: &mut Sessions, label: &str) -> TreeSession {
    let id = ClientId {
        launcher: LauncherId::parse("cli").unwrap(),
        tree: ROOT,
        label: ClientLabel::parse(label).unwrap(),
        n: Ordinal(1),
    };
    let writes_as = WriterAt {
        writer: id.writer(),
        at: PodContext::default(),
    };
    let session = sessions.open();
    let cap = ClientCap::mint(&Minter::new(), id, ROOT, writes_as);
    sessions.bind(session, SessionAuthority::Write(cap));
    sessions.session(session).unwrap()
}

/// `path` written whole as `bytes` and closed by `session`: one save.
fn save(session: &TreeSession, path: &str, bytes: &[u8]) -> Saved {
    let mut guard = session.lock().unwrap();
    let handle = guard.write_open(path, Some(ATTRS)).unwrap();
    guard.write(handle, 0, bytes, false).unwrap();
    guard.close(handle).unwrap()
}

/// An open of the saved `path` by `session`, `bytes` written over it at
/// `offset` (the length kept), and its close begun: a pending save.
fn pending(session: &TreeSession, path: &str, offset: u64, bytes: &[u8]) -> SaveToDecide {
    let handle = {
        let mut guard = session.lock().unwrap();
        let handle = guard.write_open(path, None).unwrap();
        guard.write(handle, offset, bytes, false).unwrap();
        handle
    };
    match session.start_close(handle).unwrap() {
        Closing::Decide(save) => save,
        Closing::Saved(saved) => panic!("the close saved at its start: {saved:?}"),
    }
}

fn read(rec: &Recording, tree: TreeId, path: &str) -> Vec<u8> {
    rec.actor().unwrap().read(tree, path).unwrap()
}

/// What an open of `path` by `session` reads.
fn opened(session: &TreeSession, path: &str) -> Vec<u8> {
    session
        .lock()
        .unwrap()
        .open_map(path)
        .unwrap()
        .to_vec()
        .unwrap()
}

fn commit(saved: Saved) -> u64 {
    match saved {
        Saved::Commit(step) => step.get(),
        other => panic!("not a commit: {other:?}"),
    }
}

/// The subjects of the root's commits, newest first.
fn root_subjects(dir: &Path) -> Vec<String> {
    let output = Command::new("git")
        .env("GIT_DIR", dir.join("history.git"))
        .args(["log", "--format=%s", "refs/heads/root"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A callback for `when_saved_then` that says when it ran.
fn noted() -> (agentfs_core::tree_actor::Then, mpsc::Receiver<bool>) {
    let (ran, heard) = mpsc::channel();
    (
        Box::new(move |result| ran.send(result.is_ok()).unwrap()),
        heard,
    )
}

/// The close is begun and answered with its save pending: the saved
/// content is the old one until `finish` lands it, and the metrics count it.
#[test]
fn a_started_close_returns_before_its_decision() {
    let tmp = tempfile::tempdir().unwrap();
    let (rec, mut sessions) = serve(tmp.path());
    let a = client(&mut sessions, "a");
    save(&a, "f", b"old!");
    let save = pending(&a, "f", 0, b"new!");
    assert_eq!(read(&rec, ROOT, "f"), b"old!");
    let metrics = rec.actor().unwrap().metrics();
    assert_eq!(metrics.pending_saves, 1);
    assert!(metrics.oldest_pending_save_ns > 0);
    let saved = save.finish().unwrap();
    assert!(matches!(saved, Saved::Commit(_)), "{saved:?}");
    assert_eq!(read(&rec, ROOT, "f"), b"new!");
    assert_eq!(rec.actor().unwrap().metrics().pending_saves, 0);
}

/// Close-to-open for the file's own client: its open reads the pending
/// content; another client's reads the saved one until the save lands.
#[test]
fn an_open_by_its_client_reads_the_pending_content() {
    let tmp = tempfile::tempdir().unwrap();
    let (_rec, mut sessions) = serve(tmp.path());
    let a = client(&mut sessions, "a");
    let b = client(&mut sessions, "b");
    save(&a, "f", b"old!");
    let save = pending(&a, "f", 0, b"new!");
    assert_eq!(opened(&a, "f"), b"new!");
    assert_eq!(opened(&b, "f"), b"old!");
    save.finish().unwrap();
    assert_eq!(opened(&a, "f"), b"new!");
    assert_eq!(opened(&b, "f"), b"new!");
}

/// A write-open by the file's client while its save is pending starts from
/// the pending content, and its own save lands after the pending one.
#[test]
fn a_write_open_of_a_pending_file_starts_from_its_content_and_saves_after_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (rec, mut sessions) = serve(tmp.path());
    let a = client(&mut sessions, "a");
    save(&a, "f", b"old!");
    let save = pending(&a, "f", 0, b"new!");
    let g = {
        let mut guard = a.lock().unwrap();
        let g = guard.write_open("f", None).unwrap();
        assert_eq!(guard.read_open(g).unwrap().to_vec().unwrap(), b"new!");
        guard.write(g, 0, b"+tail", true).unwrap();
        g
    };
    let first = commit(save.finish().unwrap());
    let second = commit(a.close(g).unwrap());
    assert!(first < second, "{first} then {second}");
    assert_eq!(read(&rec, ROOT, "f"), b"new!+tail");
}

/// The owner, 2026-11-23: a delete waits for the file's pending save, so
/// history holds the save, then the delete.
#[test]
fn an_unlink_of_a_pending_file_waits_for_its_save() {
    let tmp = tempfile::tempdir().unwrap();
    let (rec, mut sessions) = serve(tmp.path());
    let a = client(&mut sessions, "a");
    save(&a, "f", b"old!");
    let save = pending(&a, "f", 0, b"new!");
    let step = a.lock().unwrap().unlink("f").unwrap();
    let PathStep::WaitFor(waiting) = step else {
        panic!("the unlink ran: {step:?}");
    };
    let (then, heard) = noted();
    a.when_saved_then(waiting, then);
    assert!(
        heard.try_recv().is_err(),
        "the callback ran before the save"
    );
    save.finish().unwrap();
    assert_eq!(heard.try_recv(), Ok(true), "the landing ran the callback");
    let retried = a.lock().unwrap().unlink("f").unwrap();
    assert_eq!(retried, PathStep::Done(()));
    rec.settle().unwrap();
    let subjects = root_subjects(tmp.path());
    assert!(subjects[0].starts_with("Unlink by "), "{subjects:?}");
    assert!(subjects[1].starts_with("Direct by "), "{subjects:?}");
}

/// A rename of the pending file goes first and its save lands at the new
/// name; a rename onto a pending file waits.
#[test]
fn a_rename_of_a_pending_file_goes_first_and_onto_it_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let (rec, mut sessions) = serve(tmp.path());
    let a = client(&mut sessions, "a");
    save(&a, "f", b"old!");
    save(&a, "x", b"x");
    let save = pending(&a, "f", 0, b"new!");
    let renamed = a
        .lock()
        .unwrap()
        .rename("f", "g", RenameMode::Replace)
        .unwrap();
    assert_eq!(renamed, PathStep::Done(()));
    commit(save.finish().unwrap());
    assert_eq!(read(&rec, ROOT, "g"), b"new!");
    let onto = pending(&a, "g", 0, b"NEW!");
    let step = a
        .lock()
        .unwrap()
        .rename("x", "g", RenameMode::Replace)
        .unwrap();
    assert!(matches!(step, PathStep::WaitFor(_)), "{step:?}");
    onto.finish().unwrap();
    assert_eq!(read(&rec, ROOT, "x"), b"x");
}

/// A whole-file replace and a path truncate of the pending file wait, with
/// nothing recorded.
#[test]
fn a_replace_or_path_truncate_of_a_pending_file_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let (rec, mut sessions) = serve(tmp.path());
    let a = client(&mut sessions, "a");
    save(&a, "f", b"old!");
    let save = pending(&a, "f", 0, b"new!");
    let appended = rec.actor().unwrap().metrics().history.appended;
    let replaced = a
        .lock()
        .unwrap()
        .replace_file("f", ATTRS, &mut &b"zz"[..], 2)
        .unwrap();
    assert!(matches!(replaced, PathStep::WaitFor(_)), "{replaced:?}");
    let truncated = a.lock().unwrap().truncate_path("f", 0).unwrap();
    assert!(matches!(truncated, PathStep::WaitFor(_)), "{truncated:?}");
    assert_eq!(rec.actor().unwrap().metrics().history.appended, appended);
    save.finish().unwrap();
    assert_eq!(read(&rec, ROOT, "f"), b"new!");
}

/// Two closes of one file land in the order they began: B, begun after A,
/// waits for A's landing and is replayed on A's save, though B's decision
/// is quick and A's is not (it compares the whole file to find its one
/// changed byte, the last).
#[test]
fn two_closes_of_one_file_land_in_close_order() {
    const LEN: usize = 4 << 20;
    let tmp = tempfile::tempdir().unwrap();
    let (rec, mut sessions) = serve(tmp.path());
    let a = client(&mut sessions, "a");
    let b = client(&mut sessions, "b");
    save(&a, "f", &vec![b'0'; LEN]);
    let ha = a.lock().unwrap().write_open("f", None).unwrap();
    let mut written = vec![b'0'; LEN];
    written[LEN - 1] = b'A';
    a.lock().unwrap().write(ha, 0, &written, false).unwrap();
    let hb = b.lock().unwrap().write_open("f", None).unwrap();
    b.lock().unwrap().write(hb, 0, b"BBBB", false).unwrap();
    let Closing::Decide(first) = a.start_close(ha).unwrap() else {
        panic!("A saved at its start");
    };
    let Closing::Decide(second) = b.start_close(hb).unwrap() else {
        panic!("B saved at its start");
    };
    let (started, starting) = mpsc::channel();
    let later = std::thread::spawn(move || {
        started.send(()).unwrap();
        second.finish()
    });
    starting.recv().unwrap();
    let a_step = commit(first.finish().unwrap());
    let b_step = commit(later.join().unwrap().unwrap());
    assert!(a_step < b_step, "A's save at {a_step}, B's at {b_step}");
    let mut expected = written;
    expected[..4].copy_from_slice(b"BBBB");
    assert!(
        read(&rec, ROOT, "f") == expected,
        "f holds A's save, then B's"
    );
}

/// A pending save dropped unfinished (a panicking worker) stays pending as
/// a failed save: counted, its waiters run, its file held, and saved at
/// the tree's next step.
#[test]
fn a_save_dropped_unfinished_stays_pending_and_holds_its_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (rec, mut sessions) = serve(tmp.path());
    let a = client(&mut sessions, "a");
    save(&a, "f", b"old!");
    let save = pending(&a, "f", 0, b"new!");
    let step = a.lock().unwrap().unlink("f").unwrap();
    let PathStep::WaitFor(waiting) = step else {
        panic!("the unlink ran: {step:?}");
    };
    let (then, heard) = noted();
    a.when_saved_then(waiting, then);
    drop(save);
    let metrics = rec.actor().unwrap().metrics();
    assert_eq!((metrics.failed_saves, metrics.pending_saves), (1, 1));
    assert_eq!(heard.try_recv(), Ok(true), "the drop ran the callback");
    assert_eq!(read(&rec, ROOT, "f"), b"old!");
    a.lock().unwrap().mkdir("d", ATTRS).unwrap();
    assert_eq!(read(&rec, ROOT, "f"), b"new!");
    assert_eq!(rec.actor().unwrap().metrics().pending_saves, 0);
}

/// `settle` waits for a pending save before it waits for history.
#[test]
fn settle_waits_for_a_pending_save() {
    const LEN: usize = 4 << 20;
    let tmp = tempfile::tempdir().unwrap();
    let (rec, mut sessions) = serve(tmp.path());
    let a = client(&mut sessions, "a");
    save(&a, "f", &vec![b'0'; LEN]);
    // At rest: a settle that did not wait would answer at once.
    rec.settle().unwrap();
    let save = pending(&a, "f", 0, &vec![b'1'; LEN]);
    let (started, starting) = mpsc::channel();
    let settling = {
        let rec = Arc::clone(&rec);
        std::thread::spawn(move || {
            started.send(()).unwrap();
            let settled = rec.settle();
            (settled, read(&rec, ROOT, "f")[0])
        })
    };
    starting.recv().unwrap();
    let step = commit(save.finish().unwrap());
    let (settled, first) = settling.join().unwrap();
    let settled = settled.unwrap();
    assert_eq!(first, b'1', "settle answered before the save landed");
    assert!(
        settled.get() >= step,
        "settled at {settled:?}, saved at {step}"
    );
}

/// Rule 9: a pending save holds its file for takes. A sync into it waits
/// and lands once the save is built; a sync of another file lands
/// meanwhile.
#[test]
fn a_take_waits_for_a_pending_save_and_lands_at_its_built_event() {
    let tmp = tempfile::tempdir().unwrap();
    let (rec, mut sessions) = serve(tmp.path());
    let a = client(&mut sessions, "a");
    save(&a, "f", b"base\n");
    save(&a, "g", b"base\n");
    let fork = rowless_fork(&mut rec.actor().unwrap());
    let w = writer("w");
    let h = {
        let mut actor = rec.actor().unwrap();
        let h = actor.write_open(fork, "f", &w, None).unwrap();
        actor.write(h, 0, b"BASE\n", false).unwrap();
        h
    };
    let Closing::Decide(save_f) = rec.start_close_with(h, |_| Ok(())).unwrap() else {
        panic!("the fork's close saved at its start");
    };
    let append = |path: &str| {
        let mut guard = a.lock().unwrap();
        let h = guard.write_open(path, None).unwrap();
        guard.write(h, 0, b"more\n", true).unwrap();
        guard.close(h).unwrap();
    };
    append("g");
    assert_eq!(rec.take_next(fork).unwrap(), Took::Taken);
    assert_eq!(read(&rec, fork, "g"), b"base\nmore\n");
    append("f");
    assert_eq!(rec.take_next(fork).unwrap(), Took::Waiting);
    save_f.finish().unwrap();
    assert_eq!(rec.take_next(fork).unwrap(), Took::Taken);
    assert_eq!(read(&rec, fork, "f"), b"BASE\nmore\n");
}
