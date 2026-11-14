//! The store adapter over a real tree actor: a fork's row is one journal
//! record with its branch, durable before the store returns, its event commit
//! built behind it on its own chain (docs/trees.md, "Rows and event commits").
#![allow(
    clippy::disallowed_methods,
    reason = "a test is its own composition root and reads the store History writes"
)]
#![cfg_attr(
    not(feature = "failpoints"),
    allow(unused_imports, dead_code, reason = "most tests need failpoints")
)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use agentfs_contracts::caps::{LaunchCap, LaunchScope, Minter};
use agentfs_contracts::name::LauncherId;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Event, EventSink, ForkStore, QueueFacts};
use agentfs_contracts::tree::ForkId;
use agentfs_contracts::trees::{ForkLookup, ForkPhase, ForkSettings, Landing, Trees};
use agentfs_core::store::TreeStore;
use agentfs_core::tree_actor::{Chain, Recording, RowDomain, TreeId};
use agentfs_trees::TreesService;
use trees::{identity, writer};

#[path = "support/trees.rs"]
mod trees;

const ROOT: TreeId = TreeId::Root;
const WORKER: &str = "AGENTFS_TREE_STORE_WORKER";

/// The events published, shared by every clone.
#[derive(Clone, Default)]
struct Events(std::rc::Rc<std::cell::RefCell<Vec<Event>>>);

impl EventSink for Events {
    fn publish(&mut self, event: Event) {
        self.0.borrow_mut().push(event);
    }
}

type Service = TreesService<TreeStore, TreeStore, Events>;

fn serve(dir: &Path) -> Arc<Recording> {
    Recording::create(dir, &identity(), &PodContext::default()).expect("creates")
}

fn reopen(dir: &Path) -> Arc<Recording> {
    Recording::open(dir, &PodContext::default()).expect("reopens")
}

fn service(recording: &Arc<Recording>) -> Service {
    service_with(recording, Events::default())
}

fn service_with(recording: &Arc<Recording>, events: Events) -> Service {
    let store = TreeStore::new(Arc::clone(recording));
    TreesService::restore(store.clone(), store, events).expect("restores")
}

fn cap() -> LaunchCap {
    LaunchCap::mint(
        &Minter::new(),
        LauncherId::parse("cli").unwrap(),
        LaunchScope::Root,
    )
}

const SETTINGS: ForkSettings = ForkSettings {
    landing: Landing::Automatic,
};

fn create(trees: &mut Service) -> ForkId {
    trees.create(ROOT, None, SETTINGS, &cap()).expect("creates")
}

fn chain(fork: ForkId) -> String {
    format!("refs/agentfs/trees/fork/{}", fork.0)
}

/// `git` in the state area's store; its stdout, trimmed of its last newline
/// only. An empty string when git refuses (a ref that is not there).
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env("GIT_DIR", dir.join("history.git"))
        .args(args)
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    text.strip_suffix('\n').unwrap_or(&text).to_owned()
}

/// Every ref on disk, as `for-each-ref` lists it.
fn refs_on_disk(dir: &Path) -> String {
    git(dir, &["for-each-ref", "--format=%(refname)"])
}

/// Runs `worker` in a child process with the failpoint `spec`, which kills it;
/// returns once the state area's lease is free.
fn run_killed(worker: &str, dir: &Path, spec: &str) {
    use std::os::unix::process::ExitStatusExt;
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", worker, "--nocapture"])
        .env(WORKER, dir)
        .env(agentfs_core::failpoints::ENV_VAR, spec)
        .output()
        .unwrap();
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGKILL),
        "{spec} did not kill the worker: {output:?}"
    );
    trees::wait_for_lease(dir);
}

/// A volume and one fork through Trees, then history waited for: whichever
/// failpoint the spec names kills it on the way.
#[test]
fn fork_worker() {
    let Some(dir) = std::env::var_os(WORKER) else {
        return;
    };
    let recording = serve(Path::new(&dir));
    create(&mut service(&recording));
    recording.settle().unwrap();
    unreachable!("the failpoint kills this process");
}

/// The branch and the row are one record: killed before it, neither is there
/// at the restart; killed just after its bytes are in the journal, both are,
/// though nothing had built the row yet.
#[cfg(feature = "failpoints")]
#[test]
fn a_fork_and_its_row_are_one_record() {
    let tmp = tempfile::tempdir().unwrap();
    let before = tmp.path().join("before");
    run_killed("fork_worker", &before, "tree.before_step=kill@1");
    let recording = reopen(&before);
    let trees = service(&recording);
    assert_eq!(recording.actor().unwrap().trees(), [ROOT]);
    assert!(trees.live().is_empty());
    assert!(recording.actor().unwrap().rows(RowDomain::Trees).is_empty());
    recording.settle().unwrap();
    assert!(!refs_on_disk(&before).contains("refs/agentfs/"));
    drop((trees, recording));

    let after = tmp.path().join("after");
    run_killed("fork_worker", &after, "tree.wal_after_payload=kill@1");
    // Read before the reopen builds anything: nothing built the row.
    assert!(!refs_on_disk(&after).contains("refs/agentfs/"));
    assert!(!refs_on_disk(&after).contains("refs/heads/fork/"));
    let recording = reopen(&after);
    let trees = service(&recording);
    let fork = ForkId(1);
    assert_eq!(
        recording.actor().unwrap().trees(),
        [ROOT, TreeId::from(fork)]
    );
    assert_eq!(trees.live(), [fork]);
    assert_eq!(trees.facts(fork).unwrap().phase, ForkPhase::Open);
    recording.settle().unwrap();
    assert!(refs_on_disk(&after).contains(&chain(fork)));
}

/// The fork's record is durable when the store returns, though the builder,
/// failing, never made it so.
#[cfg(feature = "failpoints")]
#[test]
fn an_event_is_durable_before_create_returns() {
    let tmp = tempfile::tempdir().unwrap();
    let held = agentfs_core::failpoints::configure("tree.build_before_batch=error@*").unwrap();
    let recording = serve(tmp.path());
    let mut trees = service(&recording);
    create(&mut trees);
    {
        let actor = recording.actor().unwrap();
        let durability = actor.durability();
        assert!(
            durability.mark() >= durability.appended(),
            "the record is durable: mark {:?}, appended {:?}",
            durability.mark(),
            durability.appended()
        );
    }
    drop(held);
}

/// The fork's row is an event commit on its own chain: a root commit whose
/// message is `ForkCreated` with every trailer, numbered by the fork's step,
/// and whose tree is the row the actor answers.
#[test]
fn a_row_chain_is_a_commit_chain_of_events() {
    let tmp = tempfile::tempdir().unwrap();
    let recording = serve(tmp.path());
    let events = Events::default();
    let mut trees = service_with(&recording, events.clone());
    let fork = create(&mut trees);
    let other = create(&mut trees);
    assert_eq!(
        *events.0.borrow(),
        [
            Event::Trees(agentfs_contracts::trees::TreesEvent::ForkCreated(fork)),
            Event::Trees(agentfs_contracts::trees::TreesEvent::ForkCreated(other)),
        ]
    );
    recording.settle().unwrap();
    let dir = tmp.path();
    for fork in [fork, other] {
        let tip = chain(fork);
        assert_eq!(git(dir, &["rev-list", "--count", &tip]), "1");
        let commit = git(dir, &["cat-file", "commit", &tip]);
        let (header, message) = commit.split_once("\n\n").unwrap();
        assert!(!header.contains("\nparent "), "a root commit: {header}");
        assert_eq!(
            format!("{message}\n"),
            format!(
                "ForkCreated fork/{n} of root\n\nAgentfs-Kind: ForkCreated\nAgentfs-By: launcher cli\nAgentfs-Tree: fork/{n}\nAgentfs-Event: {n}\nAgentfs-Volume: test\nAgentfs-Volume-Id: 00000000-0000-0000-0000-000000000001\nAgentfs-Format: row-1\n",
                n = fork.0
            )
        );
        assert_eq!(git(dir, &["ls-tree", "--name-only", &tip]), "row.json");
        let row = recording.actor().unwrap().row(Chain::Trees(fork)).unwrap();
        assert_eq!(
            git(dir, &["cat-file", "blob", &format!("{tip}:row.json")]),
            row
        );
    }
    // The chains are no tree's history.
    let files = git(dir, &["rev-list", "refs/heads/root"]);
    assert!(!files.contains(&git(dir, &["rev-parse", &chain(fork)])));
    let fsck = Command::new("git")
        .env("GIT_DIR", dir.join("history.git"))
        .args(["fsck", "--strict", "--no-progress"])
        .output()
        .unwrap();
    assert!(fsck.status.success(), "{fsck:?}");
}

/// A failing builder holds no fork back (principle 5): the create, its row
/// and a restore of Trees all answer; once it builds again, the row is built.
#[cfg(feature = "failpoints")]
#[test]
fn a_failed_builder_stops_no_event() {
    let tmp = tempfile::tempdir().unwrap();
    let held = agentfs_core::failpoints::configure("tree.build_before_batch=error@*").unwrap();
    let recording = serve(tmp.path());
    let mut trees = service(&recording);
    let fork = create(&mut trees);
    assert!(recording.settle().is_err(), "the builder failed");
    assert!(recording.actor().unwrap().row(Chain::Trees(fork)).is_some());
    let store = TreeStore::new(Arc::clone(&recording));
    assert_eq!(ForkStore::load(&store).unwrap().len(), 1);
    let restored = service(&recording);
    assert_eq!(restored.live(), [fork]);
    assert!(!refs_on_disk(tmp.path()).contains("refs/agentfs/"));
    drop(held);
    recording.settle().unwrap();
    assert!(refs_on_disk(tmp.path()).contains(&chain(fork)));
}

/// A row recorded but not built when the process died: the restart answers
/// it at once, from the journal, and builds its one commit; a second restart
/// builds nothing again.
#[cfg(feature = "failpoints")]
#[test]
fn a_restart_builds_the_rows_it_had_not_built() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    run_killed("fork_worker", &dir, "tree.build_before_batch=kill@1");
    assert!(!refs_on_disk(&dir).contains("refs/agentfs/"));
    let fork = ForkId(1);
    let recording = reopen(&dir);
    let row = recording.actor().unwrap().row(Chain::Trees(fork));
    assert!(row.is_some(), "answered before history is built");
    recording.settle().unwrap();
    let tip = git(&dir, &["rev-parse", &chain(fork)]);
    assert_eq!(git(&dir, &["rev-list", "--count", &chain(fork)]), "1");
    assert_eq!(
        git(&dir, &["cat-file", "blob", &format!("{tip}:row.json")]),
        row.unwrap()
    );
    drop(recording);
    let recording = reopen(&dir);
    recording.settle().unwrap();
    assert_eq!(git(&dir, &["rev-parse", &chain(fork)]), tip);
    assert_eq!(git(&dir, &["rev-list", "--count", &chain(fork)]), "1");
    assert_eq!(service(&recording).live(), [fork]);
}

/// The queue facts are the journal's: a fork's landing is pending while the
/// builder fails, still after a restart from history, and gone once taken.
#[cfg(feature = "failpoints")]
#[test]
fn queue_facts_come_from_the_journal() {
    let tmp = tempfile::tempdir().unwrap();
    let held = agentfs_core::failpoints::configure("tree.build_before_batch=error@*").unwrap();
    let recording = serve(tmp.path());
    let fork = create(&mut service(&recording));
    let store = TreeStore::new(Arc::clone(&recording));
    assert!(!store.pending_from(fork));
    assert!(store.queued().is_empty());
    {
        let mut actor = recording.actor().unwrap();
        let h = actor
            .write_open(fork.into(), "f", &writer("w"), true)
            .unwrap();
        actor.write(h, 0, b"landing\n", false).unwrap();
        actor.close(h).unwrap();
    }
    assert!(store.pending_from(fork), "pending, nothing built");
    assert_eq!(store.queued(), [ROOT]);
    assert!(!store.has_child(fork));
    drop(held);
    recording.settle().unwrap();
    drop(store);
    drop(recording);
    let recording = reopen(tmp.path());
    let store = TreeStore::new(Arc::clone(&recording));
    assert!(store.pending_from(fork), "pending, read from history");
    while recording.take_next(ROOT).unwrap() == agentfs_core::tree_actor::Took::Taken {}
    assert!(!store.pending_from(fork), "taken");
    // The landing queued a sync in the fork; the root's queue is empty.
    assert_eq!(store.queued(), [TreeId::from(fork)]);
}
