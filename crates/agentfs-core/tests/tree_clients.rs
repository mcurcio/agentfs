//! Clients over the store adapter and sessions on a real tree actor
//! (docs/trees.md, "Clients"): a save names its client, a fork has one writer
//! for life, a restored writer is rebound with its id, an ended client's opens
//! are saved, and none of it waits for history.
#![allow(
    clippy::disallowed_methods,
    reason = "a test is its own composition root and reads the store History writes"
)]
#![cfg_attr(
    not(feature = "failpoints"),
    allow(unused_imports, dead_code, reason = "one test needs failpoints")
)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use agentfs_clients::ClientsService;
use agentfs_contracts::caps::{LaunchCap, LaunchScope, Minter};
use agentfs_contracts::clients::{
    Access, AttachedLookup, AttachmentEndReason, Binding, ClientError, ClientEvent, ClientId,
    ClientSet, Clients, Consistency, FileOwner, Grant,
};
use agentfs_contracts::name::{ClientLabel, Label, LauncherId};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Event, EventSink};
use agentfs_contracts::trees::{ForkSettings, Landing, Trees};
use agentfs_core::session::{Sessions, TreeSession};
use agentfs_core::store::TreeStore;
use agentfs_core::tree_actor::{Error, Recording, Saved, TreeId};
use agentfs_trees::TreesService;
use trees::identity;

#[path = "support/trees.rs"]
mod trees;

const ROOT: TreeId = TreeId::Root;
const OWNER: FileOwner = FileOwner { uid: 1, gid: 1 };

/// The events published, shared by every clone.
#[derive(Clone, Default)]
struct Events(std::rc::Rc<std::cell::RefCell<Vec<Event>>>);

impl EventSink for Events {
    fn publish(&mut self, event: Event) {
        self.0.borrow_mut().push(event);
    }
}

type Forks = TreesService<TreeStore, TreeStore, Events>;
type Service = ClientsService<TreeStore, Sessions, Forks, Events>;

/// One run of a volume: its actor, sessions and Clients.
struct Run {
    sessions: Sessions,
    clients: Service,
    events: Events,
}

fn serve(dir: &Path) -> Arc<Recording> {
    Recording::create(dir, &identity(), &PodContext::default()).expect("creates")
}

fn reopen(dir: &Path) -> Arc<Recording> {
    Recording::open(dir, &PodContext::default()).expect("reopens")
}

/// Trees and Clients restored over `recording`, as a mount restores them.
fn run(recording: &Arc<Recording>) -> Run {
    let store = TreeStore::new(Arc::clone(recording));
    let events = Events::default();
    let forks = TreesService::restore(store.clone(), store.clone(), events.clone()).unwrap();
    let sessions = Sessions::new(Arc::clone(recording));
    let clients = ClientsService::restore(
        store,
        sessions.clone(),
        forks,
        events.clone(),
        Minter::new(),
    )
    .unwrap();
    Run {
        sessions,
        clients,
        events,
    }
}

fn cap() -> LaunchCap {
    LaunchCap::mint(
        &Minter::new(),
        LauncherId::parse("cli").unwrap(),
        LaunchScope::Root,
    )
}

fn fork(recording: &Arc<Recording>) -> TreeId {
    let store = TreeStore::new(Arc::clone(recording));
    let mut trees = TreesService::restore(store.clone(), store, Events::default()).unwrap();
    let settings = ForkSettings {
        landing: Landing::Automatic,
    };
    trees.create(ROOT, None, settings, &cap()).unwrap().into()
}

fn pod(name: &str) -> PodContext {
    PodContext {
        pod: Some(Label::parse(name).unwrap()),
        ..PodContext::default()
    }
}

fn grant(tree: TreeId, access: Access) -> Grant {
    match access {
        Access::ReadWrite => Grant::live_read_write(tree, Consistency::PerFile, OWNER),
        Access::ReadOnly => Grant::live_read_only(tree, Consistency::PerFile, OWNER),
    }
}

impl Run {
    fn attach(
        &mut self,
        tree: TreeId,
        label: &str,
        access: Access,
    ) -> Result<Binding, ClientError> {
        let session = self.sessions.open();
        self.clients.attach(
            grant(tree, access),
            ClientLabel::parse(label).unwrap(),
            pod("p-1"),
            session,
            &cap(),
        )
    }

    fn session(&self, binding: &Binding) -> TreeSession {
        self.sessions.session(binding.session).expect("bound")
    }
}

/// `path` written whole and closed through `session`: one save.
fn save(session: &TreeSession, path: &str, bytes: &[u8]) -> Result<Saved, Error> {
    let mut guard = session.lock()?;
    let handle = guard.write_open(path, true)?;
    guard.write(handle, 0, bytes, false)?;
    guard.close(handle)
}

/// `git` in the state area's store; its stdout, trimmed of its last newline.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env("GIT_DIR", dir.join("history.git"))
        .args(args)
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    text.strip_suffix('\n').unwrap_or(&text).to_owned()
}

/// C4: a fork has one read-write client for life: a second is refused while
/// the first is attached, after it detached cleanly, and after a reopen. A
/// read-only client still attaches.
#[test]
fn a_second_writer_on_a_fork_is_fork_taken() {
    let tmp = tempfile::tempdir().unwrap();
    let recording = serve(tmp.path());
    let fork = fork(&recording);
    let mut one = run(&recording);
    let writer = one.attach(fork, "w", Access::ReadWrite).unwrap();
    assert_eq!(
        one.attach(fork, "w2", Access::ReadWrite),
        Err(ClientError::ForkTaken),
        "live"
    );
    one.clients.detach(&writer.client, &cap()).unwrap();
    assert_eq!(
        one.attach(fork, "w", Access::ReadWrite),
        Err(ClientError::ForkTaken),
        "after a clean detach"
    );
    drop(one);
    drop(recording);
    let recording = reopen(tmp.path());
    let mut two = run(&recording);
    assert_eq!(
        two.attach(fork, "w", Access::ReadWrite),
        Err(ClientError::ForkTaken),
        "after a reopen"
    );
    two.attach(fork, "r", Access::ReadOnly).unwrap();
    // The root has no claim.
    two.attach(ROOT, "a", Access::ReadWrite).unwrap();
    two.attach(ROOT, "b", Access::ReadWrite).unwrap();
}

/// A writer the process left attached is restored at the reopen and rebound
/// by its launcher: the same id, its new pod context on its next save; a
/// second rebind this run is refused.
#[test]
fn a_restored_writer_is_rebound_with_its_id() {
    let tmp = tempfile::tempdir().unwrap();
    let recording = serve(tmp.path());
    let fork = fork(&recording);
    let mut one = run(&recording);
    let writer = one.attach(fork, "w", Access::ReadWrite).unwrap();
    save(&one.session(&writer), "f", b"before\n").unwrap();
    drop(one);
    drop(recording);
    let recording = reopen(tmp.path());
    let mut two = run(&recording);
    let label = ClientLabel::parse("w").unwrap();
    let restored = two
        .clients
        .resumable(&cap(), fork, &label, Access::ReadWrite)
        .expect("restored");
    assert_eq!(restored, writer.client);
    let session = two.sessions.open();
    let rebound = two
        .clients
        .rebind(&restored, pod("p-2"), session, &cap())
        .unwrap();
    assert_eq!(rebound.client, writer.client, "same id");
    assert_eq!(
        two.clients.attached(fork),
        ClientSet::from([writer.client.clone()])
    );
    save(&two.session(&rebound), "f", b"after\n").unwrap();
    assert_eq!(
        two.clients
            .rebind(&restored, pod("p-3"), two.sessions.open(), &cap()),
        Err(ClientError::NotPermitted)
    );
    assert_eq!(
        two.clients
            .resumable(&cap(), fork, &label, Access::ReadWrite),
        None
    );
    recording.settle().unwrap();
    let message = git(
        tmp.path(),
        &["log", "-1", "--format=%B", &format!("refs/heads/{fork}")],
    );
    assert!(
        message.contains(&format!("Agentfs-Writer: {}\n", writer.client)),
        "{message}"
    );
    assert!(message.contains("Agentfs-Pod: p-2\n"), "{message}");
    let rebound_event = git(
        tmp.path(),
        &[
            "log",
            "-1",
            "--format=%B",
            &format!("refs/agentfs/clients/{fork}"),
        ],
    );
    assert!(
        rebound_event.starts_with(&format!("Rebound {}", writer.client)),
        "{rebound_event}"
    );
    assert!(
        rebound_event.contains("Agentfs-Pod: p-2\n"),
        "{rebound_event}"
    );
}

/// C6 at boot: three clients a killed process left attached, two on the root
/// and one on a fork, all end at the first reconcile; `LastDetached` once per
/// tree, after the ends, in one step.
#[test]
fn three_clients_killed_all_end_at_boot() {
    let tmp = tempfile::tempdir().unwrap();
    let recording = serve(tmp.path());
    let fork = fork(&recording);
    let mut one = run(&recording);
    let a = one.attach(ROOT, "a", Access::ReadWrite).unwrap().client;
    let b = one.attach(ROOT, "b", Access::ReadOnly).unwrap().client;
    let c = one.attach(fork, "c", Access::ReadWrite).unwrap().client;
    drop(one);
    drop(recording);
    let recording = reopen(tmp.path());
    let mut two = run(&recording);
    assert_eq!(
        two.clients.attached_any(),
        ClientSet::from([a.clone(), b.clone(), c.clone()])
    );
    let appended = recording.actor().unwrap().durability().appended();
    let mut ended = two.clients.reconcile(&ClientSet::new(), &cap());
    ended.sort();
    let mut expected = vec![a.clone(), b.clone(), c.clone()];
    expected.sort();
    assert_eq!(ended, expected);
    assert_eq!(
        recording.actor().unwrap().durability().appended().get(),
        appended.get() + 1,
        "one step"
    );
    let events = two.events.0.borrow().clone();
    let reconciled = |client: &ClientId| {
        Event::Clients(ClientEvent::AttachmentEnded {
            client: client.clone(),
            why: AttachmentEndReason::Reconciled,
        })
    };
    let mut expected: Vec<Event> = ended.iter().map(reconciled).collect();
    expected.push(Event::Clients(ClientEvent::LastDetached { tree: ROOT }));
    expected.push(Event::Clients(ClientEvent::LastDetached { tree: fork }));
    assert_eq!(events, expected);
    assert!(two.clients.attached_any().is_empty());
    assert!(two.clients.reconcile(&ClientSet::new(), &cap()).is_empty());
    assert_eq!(two.events.0.borrow().len(), 5, "nothing more ended");
}

/// C5: a save names its session's client and pod context; the view's calls
/// name neither.
#[test]
fn a_commit_names_its_sessions_client() {
    let tmp = tempfile::tempdir().unwrap();
    let recording = serve(tmp.path());
    let mut one = run(&recording);
    let binding = one.attach(ROOT, "root-client", Access::ReadWrite).unwrap();
    assert_eq!(
        binding.client.to_string(),
        format!("root-client#{}@cli/root", binding.client.n.0)
    );
    save(&one.session(&binding), "f", b"named\n").unwrap();
    recording.settle().unwrap();
    let message = git(tmp.path(), &["log", "-1", "--format=%B", "refs/heads/root"]);
    assert!(
        message.starts_with(&format!("Create by {}", binding.client))
            || message.starts_with(&format!("Direct by {}", binding.client)),
        "{message}"
    );
    assert!(
        message.contains(&format!("Agentfs-Writer: {}\n", binding.client)),
        "{message}"
    );
    assert!(message.contains("Agentfs-Pod: p-1\n"), "{message}");
}

/// C3: a read-only client's session reads its tree and writes nothing.
#[test]
fn a_read_only_session_cannot_write() {
    let tmp = tempfile::tempdir().unwrap();
    let recording = serve(tmp.path());
    let mut one = run(&recording);
    let writer = one.attach(ROOT, "w", Access::ReadWrite).unwrap();
    save(&one.session(&writer), "f", b"seen\n").unwrap();
    let reader = one.attach(ROOT, "r", Access::ReadOnly).unwrap();
    let session = one.session(&reader);
    let mut guard = session.lock().unwrap();
    assert_eq!(guard.read("f").unwrap(), b"seen\n");
    assert!(!guard.writable());
    for refused in [
        guard.write_open("f", false).map(drop),
        guard.write_open("g", true).map(drop),
        guard.write_open_truncating("f", false).map(drop),
        guard.anonymous_open().map(drop),
        guard.rename("f", "g"),
        guard.unlink("f"),
    ] {
        let error = refused.unwrap_err();
        assert!(matches!(error, Error::ReadOnly), "{error}");
        assert_eq!(error.errno(), libc::EROFS);
    }
    drop(guard);
    assert_eq!(
        recording.actor().unwrap().read(ROOT, "f").unwrap(),
        b"seen\n"
    );
}

/// A session reaches its own client's opens only: another client's handle,
/// on its tree or not, is no handle to it.
#[test]
fn a_session_reaches_only_its_own_handles() {
    let tmp = tempfile::tempdir().unwrap();
    let recording = serve(tmp.path());
    let mut one = run(&recording);
    let mine = one.attach(ROOT, "mine", Access::ReadWrite).unwrap();
    let other = one.attach(ROOT, "other", Access::ReadWrite).unwrap();
    let reader = one.attach(ROOT, "reader", Access::ReadOnly).unwrap();
    let mine = one.session(&mine);
    let handle = {
        let mut guard = mine.lock().unwrap();
        let handle = guard.write_open("f", true).unwrap();
        guard.write(handle, 0, b"mine\n", false).unwrap();
        handle
    };
    for session in [one.session(&other), one.session(&reader)] {
        let mut guard = session.lock().unwrap();
        for refused in [
            guard.write(handle, 0, b"theirs", false),
            guard.truncate(handle, 0),
            guard.page_writeback(handle, 0, b"theirs"),
            guard.open_object(handle).map(drop),
            guard.read_open(handle).map(drop),
            guard.close(handle).map(drop),
        ] {
            assert!(matches!(refused, Err(Error::NotFound)), "{refused:?}");
        }
    }
    assert!(matches!(
        mine.lock().unwrap().close(handle).unwrap(),
        Saved::Commit(_)
    ));
    assert_eq!(
        recording.actor().unwrap().read(ROOT, "f").unwrap(),
        b"mine\n"
    );
}

/// Principle 1: a client detached with a file open has its bytes saved and
/// durable before `detach` returns; its session then writes nothing.
#[test]
fn a_detached_clients_open_file_is_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let recording = serve(tmp.path());
    let mut one = run(&recording);
    let binding = one.attach(ROOT, "w", Access::ReadWrite).unwrap();
    let session = one.session(&binding);
    let handle = {
        let mut guard = session.lock().unwrap();
        let handle = guard.write_open("open", true).unwrap();
        guard.write(handle, 0, b"unsaved\n", false).unwrap();
        handle
    };
    assert_eq!(recording.actor().unwrap().read(ROOT, "open").unwrap(), b"");
    one.clients.detach(&binding.client, &cap()).unwrap();
    {
        let actor = recording.actor().unwrap();
        assert_eq!(actor.read(ROOT, "open").unwrap(), b"unsaved\n");
        let durability = actor.durability();
        assert!(
            durability.mark() >= durability.appended(),
            "durable: mark {:?}, appended {:?}",
            durability.mark(),
            durability.appended()
        );
    }
    assert!(one.sessions.session(binding.session).is_none(), "unbound");
    let mut guard = session.lock().unwrap();
    assert!(matches!(
        guard.write(handle, 0, b"late", false),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        guard.write_open("open", false),
        Err(Error::ReadOnly)
    ));
    drop(guard);
    drop((session, one, recording));
    let recording = reopen(tmp.path());
    assert_eq!(
        recording.actor().unwrap().read(ROOT, "open").unwrap(),
        b"unsaved\n"
    );
}

/// Principle 5: attach, rebind and detach answer while the builder fails;
/// once it builds again, their event commits are built.
#[cfg(feature = "failpoints")]
#[test]
fn clients_go_on_while_the_builder_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let held = agentfs_core::failpoints::configure("tree.build_before_batch=error@*").unwrap();
    let recording = serve(tmp.path());
    let fork = fork(&recording);
    let mut one = run(&recording);
    let writer = one.attach(fork, "w", Access::ReadWrite).unwrap();
    assert!(recording.settle().is_err(), "the builder failed");
    // A second run on the same volume: the writer is restored, not bound.
    let mut two = run(&recording);
    let rebound = two
        .clients
        .rebind(&writer.client, pod("p-2"), two.sessions.open(), &cap())
        .unwrap();
    save(&two.session(&rebound), "f", b"while failing\n").unwrap();
    two.clients.detach(&writer.client, &cap()).unwrap();
    assert!(!git(tmp.path(), &["for-each-ref"]).contains("refs/agentfs/clients/"));
    drop(held);
    recording.settle().unwrap();
    let kinds = git(
        tmp.path(),
        &[
            "log",
            "--format=%(trailers:key=Agentfs-Kind,valueonly)",
            &format!("refs/agentfs/clients/{fork}"),
        ],
    );
    let kinds: Vec<&str> = kinds.lines().filter(|l| !l.is_empty()).collect();
    // Newest first: the end's two events, in their order within the step.
    assert_eq!(
        kinds,
        ["LastDetached", "AttachmentEnded", "Rebound", "Attached"]
    );
}
