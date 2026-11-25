//! Saves history could not check (docs/trees.md, "When history cannot check
//! a save"): a save the builder's replay does not reproduce keeps the bytes
//! the workload saved, its commit is flagged, the flag is counted from
//! history, and a later release checks it again; the store's ref rebuild
//! leaves the notes refs alone.
//!
//! The flags are made with the failpoint `tree.build_replay_skew`, so most
//! tests need the `failpoints` feature: `cargo test -p agentfs-core
//! --features failpoints --test it tree_verify::`.
#![allow(
    clippy::disallowed_methods,
    reason = "a test reads the store History writes"
)]
#![cfg_attr(
    not(feature = "failpoints"),
    allow(
        dead_code,
        unused_imports,
        reason = "the flagged saves need failpoints"
    )
)]

use std::path::Path;
use std::process::Command;

use agentfs_contracts::pod::PodContext;
use agentfs_core::tree_actor::{
    EVIDENCE_REF, Outcome, TreeActor, TreeId, UnverifiedCounts, VERIFY_REF, Verdict, verify_flagged,
};
use agentfs_core::tree_git::{RefUpdate, TreeGit};
use git2::Oid;
use trees::{ATTRS, fanned, new_actor, reopen, writer};

use crate::trees;

const ROOT: TreeId = TreeId::Root;

/// The bytes the flagged save leaves: two writes, then a close.
const SAVED: &[u8] = b"first second";

fn repo(root: &Path) -> TreeGit {
    TreeGit::open(root.join("history.git")).unwrap()
}

fn message(repo: &TreeGit, commit: Oid) -> String {
    String::from_utf8(repo.read_commit(commit).unwrap().message).unwrap()
}

/// The blob at `path`, '/'-separated, in a commit's tree; `None` if absent.
fn at_path(repo: &TreeGit, commit: Oid, path: &str) -> Option<Vec<u8>> {
    let mut oid = repo.read_commit(commit).unwrap().tree;
    for part in path.split('/') {
        oid = repo
            .list_tree(oid)
            .unwrap()
            .into_iter()
            .find(|e| e.name == part.as_bytes())?
            .oid;
    }
    Some(repo.read_blob(oid).unwrap())
}

/// The file `name`'s blob at `commit`.
fn file_at(repo: &TreeGit, commit: Oid, name: &str) -> Option<Vec<u8>> {
    at_path(repo, commit, &format!("files/{}", fanned(name)))
}

/// Each `Agentfs-Unverified` trailer of `message`.
fn flags(message: &str) -> Vec<&str> {
    message
        .lines()
        .filter_map(|line| line.strip_prefix("Agentfs-Unverified: "))
        .collect()
}

fn counts(unresolved: u64, mismatch_confirmed: u64) -> UnverifiedCounts {
    UnverifiedCounts {
        unresolved,
        mismatch_confirmed,
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// Two writes and a close of `name`.
fn save_twice(v: &mut TreeActor, name: &str) {
    let h = v.write_open(ROOT, name, &writer("w"), Some(ATTRS)).unwrap();
    v.write(h, 0, &SAVED[..6], false).unwrap();
    v.write(h, 6, &SAVED[6..], false).unwrap();
    v.close(h).unwrap();
}

/// A volume whose save of `a` the builder flagged: the replay skewed (its
/// last own call dropped), built, the volume closed. Returns the flagged
/// commit.
#[cfg(feature = "failpoints")]
fn flagged_volume(root: &Path) -> Oid {
    let skew = agentfs_failpoints::configure("tree.build_replay_skew=error@*").unwrap();
    let mut v = new_actor(root);
    save_twice(&mut v, "a");
    v.settle().unwrap();
    let head = v.head(ROOT).unwrap();
    drop(v);
    drop(skew);
    head
}

/// A1: a save the builder's replay does not reproduce is built from its
/// record: the read and the head's blob are the bytes the workload saved,
/// the head is flagged with the two fingerprints, its evidence holds the
/// save's two calls, and the flag counts once, from history, across a
/// restart; a later save builds unflagged.
#[cfg(feature = "failpoints")]
#[test]
fn a_save_the_builder_cannot_reproduce_keeps_the_saved_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let skew = agentfs_failpoints::configure("tree.build_replay_skew=error@*").unwrap();
    let mut v = new_actor(tmp.path());
    save_twice(&mut v, "a");
    v.settle().unwrap();
    assert_eq!(v.read(ROOT, "a").unwrap(), SAVED);
    let repo = repo(tmp.path());
    let head = v.head(ROOT).unwrap();
    assert_eq!(file_at(&repo, head, "a").as_deref(), Some(SAVED));
    let text = message(&repo, head);
    let flag = flags(&text);
    assert_eq!(flag.len(), 1, "{text}");
    let (recorded, rest) = flag[0]
        .strip_prefix("recorded ")
        .and_then(|rest| rest.split_once(" replayed "))
        .unwrap();
    let replayed = rest.split_once(" by ").unwrap().0;
    assert!(recorded.starts_with("12:"), "{text}");
    assert_ne!(replayed, "none");
    assert_ne!(recorded, replayed);
    let evidence: serde_json::Value =
        serde_json::from_str(&repo.note(EVIDENCE_REF, head).unwrap().unwrap()).unwrap();
    assert_eq!(evidence["format"], "agentfs-verify-evidence-1");
    assert_eq!(evidence["calls"].as_array().unwrap().len(), 2);
    assert_eq!(evidence["path"], "a");
    assert_eq!(v.verification().counts(), counts(1, 0));
    drop(v);
    drop(skew);
    let mut v = reopen(tmp.path());
    v.settle().unwrap();
    assert_eq!(v.read(ROOT, "a").unwrap(), SAVED);
    assert_eq!(v.verification().counts(), counts(1, 0));
    save_twice(&mut v, "b");
    v.settle().unwrap();
    let later = v.head(ROOT).unwrap();
    assert!(flags(&message(&repo, later)).is_empty());
    assert_eq!(file_at(&repo, later, "b").as_deref(), Some(SAVED));
    assert_eq!(v.verification().counts(), counts(1, 0));
}

/// A7: a batch built again after a failure past its pack (the replay skews
/// at every attempt) makes the same commit: its evidence is written once,
/// the head carries one flag, and the flag counts once.
#[cfg(feature = "failpoints")]
#[test]
fn a_batch_built_again_writes_evidence_once() {
    let tmp = tempfile::tempdir().unwrap();
    // Nothing built until the save is recorded: one batch holds it.
    let points = agentfs_failpoints::configure("tree.build_before_batch=error@*").unwrap();
    let mut v = new_actor(tmp.path());
    save_twice(&mut v, "a");
    // In one step: the close asked for a build, and an attempt that started
    // with no spec in force would build the batch once, unskewed.
    points
        .set("tree.build_replay_skew=error@*;tree.build_after_pack=error@1")
        .unwrap();
    // The first attempt fails past its pack, wherever a request finds it.
    if v.settle().is_err() {
        v.settle().unwrap();
    }
    assert!(
        agentfs_failpoints::hits("tree.build_after_pack") >= 2,
        "the batch was built twice"
    );
    let dir = tmp.path().join("history.git");
    assert_eq!(
        git(&dir, &["rev-list", "--count", EVIDENCE_REF]).trim(),
        "1"
    );
    let head = v.head(ROOT).unwrap();
    assert_eq!(flags(&message(&repo(tmp.path()), head)).len(), 1);
    assert_eq!(v.verification().counts(), counts(1, 0));
}

/// A8: `agentfs verify`'s check. The release that flagged a save writes
/// nothing; a later release with the replay still skewed writes
/// mismatch-confirmed, and one whose replay agrees writes verified, each a
/// notes commit on the last; a verified flag counts in neither.
#[cfg(feature = "failpoints")]
#[test]
fn a_later_release_checks_a_flag_again() {
    let tmp = tempfile::tempdir().unwrap();
    let head = flagged_volume(tmp.path());
    let pod = PodContext::default();
    let report = verify_flagged(tmp.path(), &pod, agentfs_core::tree_actor::release()).unwrap();
    assert_eq!(report.checked.len(), 1);
    assert_eq!(report.checked[0].outcome, Outcome::SameRelease);
    assert_eq!(report.checked[0].path, "a");
    assert_eq!(report.standing, 1);
    let store = repo(tmp.path());
    assert_eq!(store.note_tip(VERIFY_REF).unwrap(), None, "nothing written");
    {
        let _skew = agentfs_failpoints::configure("tree.build_replay_skew=error@*").unwrap();
        let report = verify_flagged(tmp.path(), &pod, Some("agentfs 0.0.0 build later-1")).unwrap();
        assert_eq!(
            report.checked[0].outcome,
            Outcome::Wrote(Verdict::MismatchConfirmed)
        );
        assert_eq!(report.standing, 1);
    }
    let mismatched = store.note_tip(VERIFY_REF).unwrap().unwrap();
    let v = reopen(tmp.path());
    assert_eq!(v.verification().counts(), counts(0, 1));
    drop(v);
    let report = verify_flagged(tmp.path(), &pod, Some("agentfs 0.0.0 build later-2")).unwrap();
    assert_eq!(report.checked[0].outcome, Outcome::Wrote(Verdict::Verified));
    assert_eq!(report.standing, 0);
    let verified = store.note_tip(VERIFY_REF).unwrap().unwrap();
    assert_eq!(
        store.read_commit(verified).unwrap().parents,
        vec![mismatched]
    );
    assert_eq!(
        store.note(VERIFY_REF, head).unwrap().unwrap(),
        "Agentfs-Verdict: mismatch-confirmed by agentfs 0.0.0 build later-1\n\
         Agentfs-Verdict: verified by agentfs 0.0.0 build later-2\n"
    );
    let v = reopen(tmp.path());
    assert_eq!(v.verification().counts(), counts(0, 0));
    drop(v);
    // Checked once: a verified flag is not checked again.
    let report = verify_flagged(tmp.path(), &pod, Some("agentfs 0.0.0 build later-3")).unwrap();
    assert!(report.checked.is_empty());
}

/// A8: a person's acknowledgement, appended by hand with `git notes
/// append`, clears the flag.
#[cfg(feature = "failpoints")]
#[test]
fn an_acknowledgement_appended_by_hand_clears_a_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let head = flagged_volume(tmp.path());
    git(
        &tmp.path().join("history.git"),
        &[
            "-c",
            "user.name=operator",
            "-c",
            "user.email=operator@example.com",
            "notes",
            "--ref=agentfs-verify",
            "append",
            "-m",
            "Agentfs-Verdict: acknowledged by an operator",
            &head.to_string(),
        ],
    );
    let v = reopen(tmp.path());
    assert_eq!(v.verification().counts(), counts(0, 0));
}

/// The child of [`a_start_under_another_release_checks_a_flag`]: opens the
/// volume at `TREE_VERIFY_CHILD`, and closes it. A no-op in a normal run.
#[test]
fn verify_child() {
    let Some(root) = std::env::var_os("TREE_VERIFY_CHILD") else {
        return;
    };
    drop(TreeActor::open(Path::new(&root), &PodContext::default()).unwrap());
}

/// A8: a start under another release (this test binary with one byte
/// appended, so its hash differs) checks the standing flag before it
/// serves, and writes the verdict.
#[cfg(feature = "failpoints")]
#[test]
fn a_start_under_another_release_checks_a_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let head = flagged_volume(&root);
    let other = tmp.path().join("other-release");
    // Written by a child of its own: no descriptor of it open for writing
    // in this process, which another test's fork could hold at the exec.
    let copied = Command::new("sh")
        .args([
            "-c",
            "cp \"$1\" \"$2\" && printf x >> \"$2\" && chmod 755 \"$2\"",
            "sh",
        ])
        .arg(std::env::current_exe().unwrap())
        .arg(&other)
        .status()
        .unwrap();
    assert!(copied.success());
    let out = Command::new(&other)
        .args([
            test_name!("verify_child").as_str(),
            "--exact",
            "--test-threads",
            "1",
        ])
        .env("TREE_VERIFY_CHILD", &root)
        .env_remove(agentfs_failpoints::ENV_VAR)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let note = repo(&root).note(VERIFY_REF, head).unwrap().unwrap();
    assert!(
        note.starts_with("Agentfs-Verdict: verified by agentfs "),
        "{note}"
    );
    let mine = agentfs_core::tree_actor::release().unwrap();
    assert!(!note.contains(mine), "written by the other release: {note}");
    let v = reopen(&root);
    assert_eq!(v.verification().counts(), counts(0, 0));
}

/// A10: a verdict note that does not parse never stops a start: the flag
/// stands, counted unresolved, and `agentfs verify` reports it unreadable.
#[cfg(feature = "failpoints")]
#[test]
fn an_unreadable_verdict_note_does_not_stop_a_start() {
    let tmp = tempfile::tempdir().unwrap();
    let head = flagged_volume(tmp.path());
    let store = repo(tmp.path());
    let committer = git2::Signature::now("operator", "operator@example.com").unwrap();
    store
        .write_note(VERIFY_REF, head, "not a verdict\n", &committer, None)
        .unwrap();
    let v = reopen(tmp.path());
    assert_eq!(v.verification().counts(), counts(1, 0));
    drop(v);
    let report = verify_flagged(
        tmp.path(),
        &PodContext::default(),
        Some("agentfs 0.0.0 build later"),
    )
    .unwrap();
    assert!(report.checked.is_empty());
    assert_eq!(report.unreadable, vec![head]);
    assert_eq!(report.standing, 1);
}

/// `agentfs verify` refuses a volume another process holds.
#[test]
fn verify_refuses_a_held_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let v = new_actor(tmp.path());
    let held = verify_flagged(tmp.path(), &PodContext::default(), None);
    assert!(
        matches!(held, Err(agentfs_core::tree_actor::Error::Busy)),
        "{held:?}"
    );
    drop(v);
    let report = verify_flagged(tmp.path(), &PodContext::default(), None).unwrap();
    assert!(report.checked.is_empty() && report.unreadable.is_empty());
    assert_eq!(report.standing, 0);
}

/// A store with one commit on `refs/heads/root` and a note on it under
/// [`VERIFY_REF`].
fn noted_store(path: &Path) -> (TreeGit, Oid, Oid) {
    let repo = TreeGit::init(path).unwrap();
    repo.write_config("refs/heads/root", &Default::default())
        .unwrap();
    let tree = repo.write_tree(&[]).unwrap();
    let tip = repo.write_commit(tree, &[], "c\n").unwrap();
    repo.sync_objects().unwrap();
    repo.ref_transaction(&[RefUpdate {
        refname: "refs/heads/root".into(),
        new: Some(tip),
        expected_old: None,
    }])
    .unwrap();
    let committer = git2::Signature::now("agentfs", "agentfs@localhost").unwrap();
    let note = repo
        .write_note(VERIFY_REF, tip, "kept\n", &committer, None)
        .unwrap();
    (repo, tip, note)
}

fn pointers(tip: Oid) -> Vec<RefUpdate> {
    vec![RefUpdate {
        refname: "refs/heads/root".into(),
        new: Some(tip),
        expected_old: None,
    }]
}

/// A9: the ref rebuild remakes the pointer refs and leaves `refs/notes/`
/// as it was, loose or packed, and reads no note: a garbage notes ref does
/// not fail it.
#[test]
fn a_ref_rebuild_keeps_the_notes_refs() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo.git");
    let (repo, tip, note) = noted_store(&path);
    repo.rebuild_refs(&pointers(tip)).unwrap();
    assert_eq!(repo.note_tip(VERIFY_REF).unwrap(), Some(note));
    assert_eq!(
        repo.note(VERIFY_REF, tip).unwrap().as_deref(),
        Some("kept\n")
    );
    git(&path, &["update-ref", "refs/heads/gone", &tip.to_string()]);
    git(&path, &["pack-refs", "--all"]);
    repo.rebuild_refs(&pointers(tip)).unwrap();
    assert_eq!(
        repo.list_refs("refs/").unwrap(),
        vec![
            ("refs/heads/root".to_owned(), tip),
            (VERIFY_REF.to_owned(), note),
        ]
    );
    let packed = std::fs::read_to_string(path.join("packed-refs")).unwrap();
    assert!(
        packed
            .lines()
            .all(|line| line.starts_with('#') || line.ends_with(VERIFY_REF)),
        "{packed}"
    );
    assert!(repo.head("refs/heads/gone").unwrap().is_none());
    // A notes ref damaged by hand: the rebuild never reads it, and leaves it.
    std::fs::write(path.join("refs/notes/agentfs-verify"), b"not a ref\n").unwrap();
    repo.rebuild_refs(&pointers(tip)).unwrap();
    assert_eq!(
        std::fs::read(path.join("refs/notes/agentfs-verify")).unwrap(),
        b"not a ref\n"
    );
    assert_eq!(repo.head("refs/heads/root").unwrap(), Some(tip));
}

/// A9: a rebuild cut short after its removals leaves the notes refs intact.
#[cfg(feature = "failpoints")]
#[test]
fn a_ref_rebuild_cut_short_keeps_the_notes_refs() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo.git");
    let (repo, tip, note) = noted_store(&path);
    git(&path, &["pack-refs", "--all"]);
    {
        let _cut = agentfs_failpoints::configure("tree.ref_rebuild_after_remove=error").unwrap();
        assert!(repo.rebuild_refs(&pointers(tip)).is_err());
    }
    assert_eq!(repo.note_tip(VERIFY_REF).unwrap(), Some(note));
    assert_eq!(
        repo.note(VERIFY_REF, tip).unwrap().as_deref(),
        Some("kept\n")
    );
    repo.rebuild_refs(&pointers(tip)).unwrap();
    assert_eq!(repo.note_tip(VERIFY_REF).unwrap(), Some(note));
}

/// A9: notes git writes with a fan-out are read, and a note written here
/// lands where git reads it; a write from a stale tip moves nothing.
#[test]
fn notes_are_read_and_written_in_gits_layout() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo.git");
    let repo = TreeGit::init(&path).unwrap();
    repo.write_config("refs/heads/root", &Default::default())
        .unwrap();
    // Enough notes that git fans them out, in one fast-import.
    let mut stream = String::new();
    for n in 1..=300 {
        stream.push_str(&format!(
            "commit refs/heads/root\nmark :{n}\ncommitter c <c@x> {n} +0000\ndata 2\n{:02}\n",
            n % 100
        ));
    }
    for n in 1..=300 {
        stream.push_str(&format!(
            "commit refs/notes/agentfs-verify\ncommitter c <c@x> 1 +0000\ndata 1\nn\nN inline :{n}\ndata 3\nn{:02}\n",
            n % 100
        ));
    }
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(&path)
        .args(["fast-import", "--quiet"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), stream.as_bytes()).unwrap();
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    let top = git(&path, &["ls-tree", VERIFY_REF]);
    assert!(
        top.lines().all(|line| line.contains(" tree ")),
        "fanned out: {top}"
    );
    let notes = repo.notes(VERIFY_REF).unwrap();
    assert_eq!(notes.len(), 300);
    let tip = repo.head("refs/heads/root").unwrap().unwrap();
    let before = repo.note_tip(VERIFY_REF).unwrap();
    assert_eq!(repo.note(VERIFY_REF, tip).unwrap().as_deref(), Some("n00"));
    let committer = git2::Signature::now("agentfs", "agentfs@localhost").unwrap();
    let after = repo
        .write_note(VERIFY_REF, tip, "replaced\n", &committer, before)
        .unwrap();
    assert_eq!(
        git(
            &path,
            &["notes", "--ref=agentfs-verify", "show", &tip.to_string()]
        ),
        "replaced\n"
    );
    assert_eq!(repo.notes(VERIFY_REF).unwrap().len(), 300);
    assert!(
        repo.write_note(VERIFY_REF, tip, "stale\n", &committer, before)
            .is_err()
    );
    assert_eq!(repo.note_tip(VERIFY_REF).unwrap(), Some(after));
    assert_eq!(
        repo.note(VERIFY_REF, tip).unwrap().as_deref(),
        Some("replaced\n")
    );
}
