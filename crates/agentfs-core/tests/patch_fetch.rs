//! Fetching the patch queue from a remote (design §12): the inbox and
//! approved refs of this volume only, forced, with deleted ones pruned;
//! failures and the time limit are outcomes, never errors.

use std::net::TcpListener;
use std::path::Path;
use std::time::{Duration, Instant};

use git2::{Oid, Repository, Signature};

use agentfs_core::config::Settings;
use agentfs_core::events::PushFailureKind;
use agentfs_core::patch::fetch::fetch_inbox;
use agentfs_core::volume::{self, StateDir};

/// A state area for `vol` whose only remote is `url`.
fn volume_with_remote(dir: &Path, url: &str) -> (StateDir, Settings) {
    let state = StateDir::new(dir.join("state"));
    volume::init(&state, "vol", None).unwrap();
    let mut config = git2::Config::open(&state.git().join("config")).unwrap();
    config.set_str("remote.origin.url", url).unwrap();
    let settings = Settings::load(&state.git()).unwrap();
    (state, settings)
}

fn commit(repo: &Repository, content: &str) -> Oid {
    let sig = Signature::now("Ada", "ada@example.com").unwrap();
    let blob = repo.blob(content.as_bytes()).unwrap();
    let mut tree = repo.treebuilder(None).unwrap();
    tree.insert("a.txt", blob, 0o100644).unwrap();
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    repo.commit(None, &sig, &sig, content, &tree, &[]).unwrap()
}

fn local_refs(state: &StateDir, glob: &str) -> Vec<(String, Oid)> {
    let repo = Repository::open_bare(state.git()).unwrap();
    let mut refs: Vec<(String, Oid)> = repo
        .references_glob(glob)
        .unwrap()
        .map(|r| {
            let r = r.unwrap();
            (r.name().unwrap().to_owned(), r.target().unwrap())
        })
        .collect();
    refs.sort();
    refs
}

#[test]
fn mirrors_this_volumes_inbox_and_prunes() {
    let dir = tempfile::tempdir().unwrap();
    let remote_dir = dir.path().join("remote.git");
    let remote = Repository::init_bare(&remote_dir).unwrap();
    let p1 = commit(&remote, "one");
    let p2 = commit(&remote, "two");
    for (name, oid) in [
        ("refs/agentfs/vol/inbox/0000000001", p1),
        ("refs/agentfs/vol/inbox/0000000002", p2),
        ("refs/agentfs/other/inbox/0000000001", p2),
        ("refs/agentfs/vol/status", p2),
        ("refs/heads/agentfs/vol", p1),
    ] {
        remote.reference(name, oid, true, "t").unwrap();
    }
    let url = format!("file://{}", remote_dir.display());
    let (state, settings) = volume_with_remote(dir.path(), &url);
    let patch_remote = settings.patch_remote().unwrap();

    let outcome = fetch_inbox(&state.git(), patch_remote, "vol", Duration::from_secs(30));
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.remote, "origin");
    assert_eq!(outcome.remote_inbox, 2);
    assert_eq!(outcome.pruned, 0);
    assert_eq!(
        local_refs(&state, "refs/agentfs/*"),
        [
            ("refs/agentfs/vol/inbox/0000000001".to_owned(), p1),
            ("refs/agentfs/vol/inbox/0000000002".to_owned(), p2),
        ]
    );
    // The objects came too.
    let local = Repository::open_bare(state.git()).unwrap();
    assert!(local.find_commit(p2).is_ok());
    assert!(local_refs(&state, "refs/heads/*").is_empty());

    // A replaced patch is forced; a withdrawn one is pruned.
    let p3 = commit(&remote, "three");
    remote
        .reference("refs/agentfs/vol/inbox/0000000001", p3, true, "t")
        .unwrap();
    remote
        .find_reference("refs/agentfs/vol/inbox/0000000002")
        .unwrap()
        .delete()
        .unwrap();
    let outcome = fetch_inbox(&state.git(), patch_remote, "vol", Duration::from_secs(30));
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.remote_inbox, 1);
    assert_eq!(outcome.pruned, 1);
    assert_eq!(
        local_refs(&state, "refs/agentfs/*"),
        [("refs/agentfs/vol/inbox/0000000001".to_owned(), p3)]
    );
}

/// Approvals (bead agentfs-mia.2) come with the inbox, forced, and a
/// withdrawn one is pruned; another volume's never.
#[test]
fn mirrors_this_volumes_approvals_and_prunes() {
    let dir = tempfile::tempdir().unwrap();
    let remote_dir = dir.path().join("remote.git");
    let remote = Repository::init_bare(&remote_dir).unwrap();
    let p1 = commit(&remote, "one");
    let p2 = commit(&remote, "two");
    for (name, oid) in [
        ("refs/agentfs/vol/inbox/0000000001", p1),
        ("refs/agentfs/vol/approved/0000000001", p1),
        ("refs/agentfs/other/approved/0000000001", p2),
    ] {
        remote.reference(name, oid, true, "t").unwrap();
    }
    let url = format!("file://{}", remote_dir.display());
    let (state, settings) = volume_with_remote(dir.path(), &url);
    let patch_remote = settings.patch_remote().unwrap();
    let outcome = fetch_inbox(&state.git(), patch_remote, "vol", Duration::from_secs(30));
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.remote_inbox, 1);
    assert_eq!(
        local_refs(&state, "refs/agentfs/*"),
        [
            ("refs/agentfs/vol/approved/0000000001".to_owned(), p1),
            ("refs/agentfs/vol/inbox/0000000001".to_owned(), p1),
        ]
    );
    // Approved again for another tip: forced; then withdrawn: pruned.
    remote
        .reference("refs/agentfs/vol/approved/0000000001", p2, true, "t")
        .unwrap();
    fetch_inbox(&state.git(), patch_remote, "vol", Duration::from_secs(30));
    assert_eq!(
        local_refs(&state, "refs/agentfs/vol/approved/*"),
        [("refs/agentfs/vol/approved/0000000001".to_owned(), p2)]
    );
    remote
        .find_reference("refs/agentfs/vol/approved/0000000001")
        .unwrap()
        .delete()
        .unwrap();
    let outcome = fetch_inbox(&state.git(), patch_remote, "vol", Duration::from_secs(30));
    assert_eq!(outcome.pruned, 1);
    assert!(local_refs(&state, "refs/agentfs/vol/approved/*").is_empty());
}

#[test]
fn an_unreachable_remote_is_an_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file://{}", dir.path().join("missing.git").display());
    let (state, settings) = volume_with_remote(dir.path(), &url);
    let outcome = fetch_inbox(
        &state.git(),
        settings.patch_remote().unwrap(),
        "vol",
        Duration::from_secs(30),
    );
    assert!(!outcome.ok);
    assert!(outcome.kind.is_some(), "{outcome:?}");
    assert!(outcome.message.is_some());
}

#[test]
fn the_time_limit_is_kept() {
    // Accepts connections (the kernel's backlog) and never answers.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let url = format!("http://127.0.0.1:{port}/repo.git");
    let (state, settings) = volume_with_remote(dir.path(), &url);
    let started = Instant::now();
    let outcome = fetch_inbox(
        &state.git(),
        settings.patch_remote().unwrap(),
        "vol",
        Duration::from_millis(300),
    );
    let took = started.elapsed();
    assert!(!outcome.ok);
    assert_eq!(
        outcome.kind,
        Some(PushFailureKind::Unreachable),
        "{outcome:?}"
    );
    assert!(
        outcome
            .message
            .as_deref()
            .unwrap()
            .contains("patchFetchTimeout"),
        "{outcome:?}"
    );
    assert!(took < Duration::from_secs(5), "took {took:?}");
    drop(listener);
}
