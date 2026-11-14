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

use agentfs_contracts::audit::Principal;
use agentfs_contracts::caps::{LaunchCap, LaunchScope, Minter};
use agentfs_contracts::name::LauncherId;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Event, EventSink, ForkStore, QueueFacts, StoreError};
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

// Slice S5: keys, end and the drop.

fn key(text: &str) -> agentfs_contracts::name::ForkKey {
    agentfs_contracts::name::ForkKey::parse(text).unwrap()
}

/// `name` in `tree` holds `bytes`: one create (or open), write and close.
fn put(recording: &Arc<Recording>, tree: TreeId, name: &str, bytes: &[u8]) {
    let mut actor = recording.actor().unwrap();
    let h = actor.write_open(tree, name, &writer("w"), true).unwrap();
    actor.truncate(h, 0).unwrap();
    actor.write(h, 0, bytes, false).unwrap();
    actor.close(h).unwrap();
}

/// The subject of every commit on `name`, newest first.
fn subjects(dir: &Path, name: &str) -> Vec<String> {
    git(dir, &["log", "--format=%s", name])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn phase(trees: &Service, fork: ForkId) -> ForkPhase {
    trees.facts(fork).expect("a row").phase
}

/// A second create under a key returns its open fork: no record, no second
/// fork, at once and after a restart.
#[test]
fn a_second_create_under_a_key_returns_its_fork() {
    let tmp = tempfile::tempdir().unwrap();
    let recording = serve(tmp.path());
    let mut trees = service(&recording);
    let fork = trees
        .create(ROOT, Some(key("pvc-1")), SETTINGS, &cap())
        .unwrap();
    let appended = recording.actor().unwrap().durability().appended();
    assert_eq!(
        trees.create(ROOT, Some(key("pvc-1")), SETTINGS, &cap()),
        Ok(fork)
    );
    assert_eq!(
        recording.actor().unwrap().durability().appended(),
        appended,
        "nothing recorded"
    );
    drop(trees);
    drop(recording);
    let recording = reopen(tmp.path());
    let mut trees = service(&recording);
    assert_eq!(
        trees.create(ROOT, Some(key("pvc-1")), SETTINGS, &cap()),
        Ok(fork)
    );
    assert_eq!(trees.live(), [fork]);
    assert_eq!(recording.actor().unwrap().trees(), [ROOT, fork.into()]);
}

/// The drop rule: an ended fork stays while its landing is queued; once it
/// is taken, the fork drops in one step. Its branch, its queue's entries and
/// its checkout go; the loser's collision ref, the saves it reaches and the
/// row chain, ending `ForkDropped`, stay.
#[test]
fn an_ended_fork_drops_only_after_its_landing_is_taken() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let recording = serve(dir);
    let events = Events::default();
    let mut trees = service_with(&recording, events.clone());
    put(&recording, ROOT, "a", b"base\n");
    let fork = create(&mut trees);
    let tree = TreeId::from(fork);
    put(&recording, ROOT, "a", b"parent\n");
    put(&recording, tree, "a", b"child\n");
    let losing = recording.actor().unwrap().head(tree).unwrap().to_string();
    trees.end(fork, &cap()).unwrap();
    assert_eq!(phase(&trees, fork), ForkPhase::Ended);
    assert_eq!(trees.settle(), Ok(vec![]), "its landing is queued");
    // The actor refuses the drop itself, whatever its caller asked first.
    let mut store = TreeStore::new(Arc::clone(&recording));
    assert!(matches!(
        ForkStore::drop_fork(
            &mut store,
            fork,
            &Principal::Agentfs,
            &PodContext::default()
        ),
        Err(StoreError::Refused(_))
    ));
    assert_eq!(recording.actor().unwrap().trees(), [ROOT, tree]);
    while recording.take_next(ROOT).unwrap() == agentfs_core::tree_actor::Took::Taken {}
    recording.settle().unwrap();
    let queue = format!("refs/queue/{tree}/");
    assert!(
        !git(dir, &["for-each-ref", &queue]).is_empty(),
        "the landing queued a sync in the fork"
    );
    assert!(git(dir, &["for-each-ref", "refs/collisions/root/"]).contains(&losing));
    assert_eq!(trees.settle(), Ok(vec![fork]));
    assert_eq!(phase(&trees, fork), ForkPhase::Dropped);
    assert!(trees.live().is_empty());
    assert_eq!(recording.actor().unwrap().trees(), [ROOT]);
    assert!(events.0.borrow().contains(&Event::Trees(
        agentfs_contracts::trees::TreesEvent::ForkDropped(fork)
    )));
    recording.settle().unwrap();
    let refs = refs_on_disk(dir);
    assert!(!refs.contains(&format!("refs/heads/{tree}")), "{refs}");
    assert!(!refs.contains(&queue), "{refs}");
    assert!(!dir.join("trees").join(tree.to_string()).exists());
    assert!(git(dir, &["for-each-ref", "refs/collisions/root/"]).contains(&losing));
    let reached = Command::new("git")
        .env("GIT_DIR", dir.join("history.git"))
        .args(["merge-base", "--is-ancestor", &losing, "refs/heads/root"])
        .status()
        .unwrap();
    assert!(reached.success(), "the fork's save stays reachable");
    assert_eq!(
        subjects(dir, &chain(fork)),
        [
            format!("ForkDropped {tree}"),
            format!("ForkEnded {tree}"),
            format!("ForkCreated {tree} of root"),
        ]
    );
    let fsck = Command::new("git")
        .env("GIT_DIR", dir.join("history.git"))
        .args(["fsck", "--strict", "--no-progress"])
        .output()
        .unwrap();
    assert!(fsck.status.success(), "{fsck:?}");
}

/// Principle 1: an open file holds the drop back; once it closes, the next
/// settle drops the fork.
#[test]
fn a_drop_waits_for_its_open_file() {
    let tmp = tempfile::tempdir().unwrap();
    let recording = serve(tmp.path());
    let mut trees = service(&recording);
    put(&recording, ROOT, "a", b"base\n");
    let fork = create(&mut trees);
    let held = recording
        .actor()
        .unwrap()
        .write_open(fork.into(), "a", &writer("w"), false)
        .unwrap();
    trees.end(fork, &cap()).unwrap();
    assert_eq!(phase(&trees, fork), ForkPhase::Ended);
    assert_eq!(trees.settle(), Ok(vec![]), "an open file holds it");
    assert_eq!(
        recording.actor().unwrap().trees(),
        [ROOT, TreeId::from(fork)]
    );
    recording.actor().unwrap().close(held).unwrap();
    while recording.take_next(ROOT).unwrap() == agentfs_core::tree_actor::Took::Taken {}
    assert_eq!(trees.settle(), Ok(vec![fork]));
    assert_eq!(recording.actor().unwrap().trees(), [ROOT]);
}

/// A volume, a fork and its end, which drops it: whichever failpoint the
/// spec names kills it on the way.
#[test]
fn drop_worker() {
    let Some(dir) = std::env::var_os(WORKER) else {
        return;
    };
    let recording = serve(Path::new(&dir));
    let mut trees = service(&recording);
    let fork = create(&mut trees);
    trees.end(fork, &cap()).unwrap();
    recording.settle().unwrap();
    unreachable!("the failpoint kills this process");
}

/// The drop and its row are one record: killed before it, the fork is
/// ended with its branch, and the boot's settle drops it; killed just after
/// its bytes are in the journal, it is dropped with no branch, row
/// `Dropped`, though nothing had built either.
#[cfg(feature = "failpoints")]
#[test]
fn a_drop_and_its_row_are_one_record() {
    let tmp = tempfile::tempdir().unwrap();
    let fork = ForkId(1);
    let tree = TreeId::from(fork);
    // The create's step, the end's, then the drop's.
    let before = tmp.path().join("before");
    run_killed("drop_worker", &before, "tree.before_step=kill@3");
    let recording = reopen(&before);
    let mut trees = service(&recording);
    assert_eq!(phase(&trees, fork), ForkPhase::Ended);
    assert_eq!(recording.actor().unwrap().trees(), [ROOT, tree]);
    assert_eq!(trees.settle(), Ok(vec![fork]), "the boot's settle drops it");
    drop((trees, recording));

    let after = tmp.path().join("after");
    run_killed("drop_worker", &after, "tree.wal_after_payload=kill@3");
    let recording = reopen(&after);
    let trees = service(&recording);
    assert_eq!(phase(&trees, fork), ForkPhase::Dropped);
    assert_eq!(recording.actor().unwrap().trees(), [ROOT]);
    recording.settle().unwrap();
    assert!(!refs_on_disk(&after).contains(&format!("refs/heads/{tree}")));
    assert!(!after.join("trees").join(tree.to_string()).exists());
    assert_eq!(
        subjects(&after, &chain(fork))[0],
        format!("ForkDropped {tree}")
    );
}

/// Principle 5: with the builder failing, the end and the drop answer and
/// the root goes on saving; the restart recovers the fork dropped, and no
/// root save queues anything for it: no branch, queue or checkout.
#[cfg(feature = "failpoints")]
#[test]
fn a_dropped_fork_recovers_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let held = agentfs_core::failpoints::configure("tree.build_before_batch=error@*").unwrap();
    let recording = serve(dir);
    let mut trees = service(&recording);
    let fork = create(&mut trees);
    let tree = TreeId::from(fork);
    put(&recording, ROOT, "before-drop", b"1\n");
    trees.end(fork, &cap()).unwrap();
    assert_eq!(phase(&trees, fork), ForkPhase::Dropped);
    assert_eq!(recording.actor().unwrap().trees(), [ROOT]);
    put(&recording, ROOT, "after-drop", b"2\n");
    assert!(recording.settle().is_err(), "the builder failed");
    drop((trees, recording));
    drop(held);
    let recording = reopen(dir);
    let trees = service(&recording);
    assert_eq!(phase(&trees, fork), ForkPhase::Dropped);
    assert_eq!(recording.actor().unwrap().trees(), [ROOT]);
    put(&recording, ROOT, "after-reopen", b"3\n");
    recording.settle().unwrap();
    let refs = refs_on_disk(dir);
    assert!(!refs.contains(&format!("refs/heads/{tree}")), "{refs}");
    assert!(!refs.contains(&format!("refs/queue/{tree}/")), "{refs}");
    assert!(!dir.join("trees").join(tree.to_string()).exists());
    assert_eq!(
        subjects(dir, &chain(fork))[0],
        format!("ForkDropped {tree}")
    );
    let merges = git(dir, &["config", "--get-regexp", "^branch\\."]);
    assert!(!merges.contains(&tree.to_string()), "{merges}");
}

/// A take of a dropped fork finds its queue gone with it.
#[test]
fn a_take_of_a_dropped_fork_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let recording = serve(tmp.path());
    let mut trees = service(&recording);
    let fork = create(&mut trees);
    put(&recording, ROOT, "a", b"for the fork\n");
    assert!(
        TreeStore::new(Arc::clone(&recording))
            .queued()
            .contains(&fork.into()),
        "a sync is queued in the fork"
    );
    trees.end(fork, &cap()).unwrap();
    assert_eq!(phase(&trees, fork), ForkPhase::Dropped);
    assert_eq!(
        recording.take_next(fork.into()).unwrap(),
        agentfs_core::tree_actor::Took::Empty
    );
    assert!(TreeStore::new(Arc::clone(&recording)).queued().is_empty());
}

/// Trees shared by the test, as the composition root shares it: Clients
/// asks it `facts` while the test dispatches events to it.
#[derive(Clone)]
struct SharedTrees(std::rc::Rc<std::cell::RefCell<Service>>);

impl ForkLookup for SharedTrees {
    fn facts(&self, fork: ForkId) -> Option<agentfs_contracts::trees::ForkFacts> {
        self.0.borrow().facts(fork)
    }
}

type ClientsOf = agentfs_clients::ClientsService<
    TreeStore,
    agentfs_core::session::Sessions,
    SharedTrees,
    Events,
>;

/// Trees and Clients on one store and one event queue, as a mount has them.
fn domains(recording: &Arc<Recording>, events: &Events) -> (SharedTrees, ClientsOf) {
    let store = TreeStore::new(Arc::clone(recording));
    let trees = SharedTrees(std::rc::Rc::new(std::cell::RefCell::new(
        TreesService::restore(store.clone(), store.clone(), events.clone()).unwrap(),
    )));
    let clients = agentfs_clients::ClientsService::restore(
        store,
        agentfs_core::session::Sessions::new(Arc::clone(recording)),
        trees.clone(),
        events.clone(),
        Minter::new(),
    )
    .unwrap();
    (trees, clients)
}

/// The composition root's dispatcher: `LastDetached` to Trees, `ForkDropped`
/// to Clients, until the queue is empty.
fn dispatch(events: &Events, trees: &SharedTrees, clients: &mut ClientsOf) {
    use agentfs_contracts::clients::ClientEvent;
    use agentfs_contracts::trees::TreesEvent;
    let mut handled = 0;
    while handled < events.0.borrow().len() {
        let event = events.0.borrow()[handled].clone();
        handled += 1;
        match event {
            Event::Clients(ClientEvent::LastDetached { tree }) => {
                trees.0.borrow_mut().on_last_detached(tree).unwrap();
            }
            Event::Trees(TreesEvent::ForkDropped(fork)) => clients.on_fork_dropped(fork).unwrap(),
            _ => {}
        }
    }
}

/// Attaches a writer to `tree` and detaches it, dispatching what follows.
fn write_and_leave(
    events: &Events,
    trees: &SharedTrees,
    clients: &mut ClientsOf,
    sessions: &agentfs_core::session::Sessions,
    tree: TreeId,
) {
    use agentfs_contracts::clients::{Clients, Consistency, FileOwner, Grant};
    let owner = FileOwner { uid: 1, gid: 1 };
    let binding = clients
        .attach(
            Grant::live_read_write(tree, Consistency::PerFile, owner),
            agentfs_contracts::name::ClientLabel::parse("c").unwrap(),
            PodContext::default(),
            sessions.open(),
            &cap(),
        )
        .unwrap();
    dispatch(events, trees, clients);
    clients.detach(&binding.client, &cap()).unwrap();
    dispatch(events, trees, clients);
}

/// The last client's leaving ends an unkeyed fork, which drops; a keyed
/// fork stays open: only `end_keyed` ends it.
#[test]
fn last_detached_ends_an_unkeyed_fork_not_a_keyed_one() {
    let tmp = tempfile::tempdir().unwrap();
    let recording = serve(tmp.path());
    let events = Events::default();
    let (trees, mut clients) = domains(&recording, &events);
    let sessions = agentfs_core::session::Sessions::new(Arc::clone(&recording));
    let unkeyed = trees
        .0
        .borrow_mut()
        .create(ROOT, None, SETTINGS, &cap())
        .unwrap();
    let keyed = trees
        .0
        .borrow_mut()
        .create(ROOT, Some(key("k")), SETTINGS, &cap())
        .unwrap();
    write_and_leave(&events, &trees, &mut clients, &sessions, unkeyed.into());
    write_and_leave(&events, &trees, &mut clients, &sessions, keyed.into());
    write_and_leave(&events, &trees, &mut clients, &sessions, ROOT);
    assert_eq!(phase(&trees.0.borrow(), unkeyed), ForkPhase::Dropped);
    assert_eq!(phase(&trees.0.borrow(), keyed), ForkPhase::Open);
    assert_eq!(
        recording.actor().unwrap().trees(),
        [ROOT, TreeId::from(keyed)]
    );
    trees.0.borrow_mut().end_keyed(&key("k"), &cap()).unwrap();
    assert_eq!(phase(&trees.0.borrow(), keyed), ForkPhase::Dropped);
}

/// C4 ends at the drop: the dropped fork's writer claim leaves its Clients
/// row in one step, `ClaimReleased`, carrying the service's pod context.
#[test]
fn a_dropped_forks_claim_is_released() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let recording = serve(dir);
    let events = Events::default();
    let (trees, mut clients) = domains(&recording, &events);
    let sessions = agentfs_core::session::Sessions::new(Arc::clone(&recording));
    let fork = trees
        .0
        .borrow_mut()
        .create(ROOT, None, SETTINGS, &cap())
        .unwrap();
    let tree = TreeId::from(fork);
    let claim = |recording: &Arc<Recording>| {
        let row = recording
            .actor()
            .unwrap()
            .row(Chain::Clients(tree))
            .unwrap();
        serde_json::from_str::<agentfs_contracts::ports::TreeClients>(&row)
            .unwrap()
            .writer_claim
    };
    let binding = {
        use agentfs_contracts::clients::{Clients, Consistency, FileOwner, Grant};
        clients
            .attach(
                Grant::live_read_write(tree, Consistency::PerFile, FileOwner { uid: 1, gid: 1 }),
                agentfs_contracts::name::ClientLabel::parse("c").unwrap(),
                PodContext::default(),
                sessions.open(),
                &cap(),
            )
            .unwrap()
    };
    assert!(claim(&recording).is_some());
    {
        use agentfs_contracts::clients::Clients;
        clients.detach(&binding.client, &cap()).unwrap();
    }
    assert!(claim(&recording).is_some(), "kept after its client ends");
    dispatch(&events, &trees, &mut clients);
    assert_eq!(phase(&trees.0.borrow(), fork), ForkPhase::Dropped);
    assert!(claim(&recording).is_none(), "released at the drop");
    recording.settle().unwrap();
    let clients_chain = format!("refs/agentfs/clients/{tree}");
    assert_eq!(
        subjects(dir, &clients_chain)[0],
        format!("ClaimReleased {}", binding.client)
    );
    let body = git(dir, &["log", "-1", "--format=%B", &clients_chain]);
    assert!(body.contains("Agentfs-By: agentfs"), "{body}");
}
