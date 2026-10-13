//! Two fronts and one history service in one process, over a real Unix
//! socket (design §14). No FUSE: fronts record through their `Recorder`
//! exactly as a FUSE mount's boundary layer would.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agentfs_core::events::{Event, Events, MemorySink, NullSink};
use agentfs_core::history_service::protocol::{
    FrontInfo, PathStamp, Reply, read_frame, read_frame_bytes, write_frame,
};
use agentfs_core::history_service::{
    Address, ClientConfig, ClientError, FrontLease, HistoryClient, HistoryServer,
};
use agentfs_core::identity::Context;
use agentfs_core::lfs::Store;
use agentfs_core::paths::RepoPath;
use agentfs_core::pending::PendingId;
use agentfs_core::record::{Caller, Change, Draft, Origin, Trigger};
use agentfs_core::rules::SkipReason;
use agentfs_core::stamps::FreezeStamp;
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self, Committer, Input, Recorded, Recorder, StateDir, VolumeError};

const CALLER: Caller = Caller {
    uid: 1000,
    gid: 1000,
    pid: 1,
};

struct Env {
    _dir: tempfile::TempDir,
    state: StateDir,
    tree: PathBuf,
    address: Address,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let state = StateDir::new(dir.path().join("state"));
    let tree = dir.path().join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    volume::init(&state, "vol", None).unwrap();
    let address = Address::Unix(state.history_socket());
    Env {
        _dir: dir,
        state,
        tree,
        address,
    }
}

fn service_context() -> Context {
    Context {
        volume: "vol".into(),
        replica: "service".into(),
        pod_uid: "service-uid".into(),
        ..Context::default()
    }
}

struct Service {
    recorder: Recorder,
    committer: Committer,
    server: Option<HistoryServer>,
}

impl Service {
    fn start(env: &Env) -> Self {
        let (mut recorder, committer, _) = volume::open(
            &env.state,
            &env.tree,
            service_context(),
            Arc::new(NullSink),
            &Telemetry::noop(),
        )
        .unwrap();
        // As `serve-history` does (design §14.1).
        recorder.enable_stamps();
        let mut service = Self {
            recorder,
            committer,
            server: None,
        };
        service.listen(env);
        service
    }

    fn listen(&mut self, env: &Env) {
        let settings = self.committer.history().settings().clone();
        self.server = Some(
            HistoryServer::start(
                &env.address,
                self.recorder.clone(),
                &env.state,
                &settings,
                &Telemetry::noop(),
            )
            .unwrap(),
        );
    }

    /// Every commit, oldest first: committer name and the `Agentfs-Pod`
    /// trailer.
    fn commits(&self) -> Vec<(String, Option<String>)> {
        let repo = self.committer.history().repo();
        let Some(head) = self.committer.history().head().unwrap() else {
            return Vec::new();
        };
        let mut walk = repo.revwalk().unwrap();
        walk.push(head.id()).unwrap();
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
            .unwrap();
        walk.map(|id| {
            let commit = repo.find_commit(id.unwrap()).unwrap();
            let message = commit.message().unwrap().to_owned();
            let pod = message
                .lines()
                .find_map(|l| l.strip_prefix("Agentfs-Pod: "))
                .map(str::to_owned);
            (commit.committer().name().unwrap().to_owned(), pod)
        })
        .collect()
    }

    fn head_blob(&self, path: &str) -> Option<Vec<u8>> {
        let repo = self.committer.history().repo();
        let head = self.committer.history().head().unwrap()?;
        let entry = head.tree().unwrap().get_path(Path::new(path)).ok()?;
        Some(repo.find_blob(entry.id()).unwrap().content().to_vec())
    }
}

fn pod(name: &str) -> Origin {
    Origin {
        namespace: "agents".into(),
        pod: name.into(),
        pod_uid: format!("{name}-uid"),
        node: "node-1".into(),
        replica: name.into(),
    }
}

struct Front {
    // Held for the front's life.
    _lease: FrontLease,
    recorder: Recorder,
    sink: Arc<MemorySink>,
}

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

fn front_info(lease: &FrontLease, origin: Origin) -> FrontInfo {
    FrontInfo {
        origin,
        slot: lease.slot(),
        // Distinct for every front, as a real front's is.
        instance: u64::from(std::process::id()) << 32
            | NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
        pid: std::process::id(),
        version: "test".into(),
    }
}

fn connect(
    env: &Env,
    lease: &FrontLease,
    origin: Origin,
    volume: &str,
    timeout: Duration,
    events: Events,
) -> Result<HistoryClient, ClientError> {
    let branch = agentfs_core::history::History::open(&env.state.git())
        .unwrap()
        .settings()
        .branch_ref
        .clone();
    HistoryClient::connect(
        ClientConfig {
            address: env.address.clone(),
            timeout,
            connect_wait: Duration::from_secs(5),
        },
        volume,
        &branch,
        front_info(lease, origin),
        events,
        Telemetry::noop(),
    )
}

fn front(env: &Env, name: &str) -> Front {
    front_with_timeout(env, name, Duration::from_secs(10))
}

fn front_with_timeout(env: &Env, name: &str, timeout: Duration) -> Front {
    let lease = FrontLease::acquire(&env.state, &format!("pod={name}")).unwrap();
    let sink = Arc::new(MemorySink::new());
    let client = connect(env, &lease, pod(name), "vol", timeout, sink.clone()).unwrap();
    let recorder = volume::open_front(
        &env.state,
        &env.tree,
        sink.clone(),
        &Telemetry::noop(),
        Arc::new(client),
    )
    .unwrap();
    Front {
        _lease: lease,
        recorder,
        sink,
    }
}

/// Writes `content` at `name` in the backing tree and returns the input a
/// close of it gives.
fn write(env: &Env, name: &str, content: &str) -> Input {
    let backing = env.tree.join(name);
    if let Some(parent) = backing.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&backing, content).unwrap();
    Input::Write {
        path: RepoPath::new(name).unwrap(),
        backing,
        executable: false,
    }
}

fn close(
    env: &Env,
    front: &Recorder,
    name: &str,
    content: &str,
) -> Result<Option<u64>, VolumeError> {
    front
        .record(Trigger::Close, CALLER, vec![write(env, name, content)])
        .map(|r| r.seq)
}

fn slot_copies(env: &Env, slot: u16) -> usize {
    std::fs::read_dir(env.state.pending())
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            u64::from_str_radix(&name, 16).ok()
        })
        .filter(|id| (id >> 48) as u16 == slot)
        .count()
}

#[test]
fn concurrent_fronts_commit_with_their_own_committers() {
    let env = env();
    let service = Service::start(&env);
    let fronts = [front(&env, "pod-a"), front(&env, "pod-b")];
    assert_eq!(service.server.as_ref().unwrap().fronts(), 2);

    const THREADS: usize = 4;
    const PER_THREAD: usize = 25;
    // Writes of the shared path are serialized (write, then record), as
    // two boundaries a moment apart: two at the same instant race between
    // freeze and journal (design §14), which this test does not cover.
    let shared_lock = std::sync::Mutex::new(());
    // Each record of the shared path, by the sequence number it got.
    let shared: BTreeMap<u64, String> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        for (f, front) in fronts.iter().enumerate() {
            for t in 0..THREADS {
                let env = &env;
                let shared_lock = &shared_lock;
                handles.push(s.spawn(move || {
                    let mut shared = Vec::new();
                    for i in 0..PER_THREAD {
                        let name = format!("f{f}/t{t}/{i}");
                        close(env, &front.recorder, &name, &name).unwrap().unwrap();
                        if t == 0 && i % 5 == 0 {
                            // Both fronts write the shared path; the
                            // backing tree keeps whichever wrote last.
                            let _turn = shared_lock.lock().unwrap();
                            let content = format!("{f}-{i}");
                            let input = write(env, "shared", &content);
                            let seq = front
                                .recorder
                                .record(Trigger::Close, CALLER, vec![input])
                                .unwrap()
                                .seq
                                .unwrap();
                            shared.push((seq, content));
                        }
                    }
                    shared
                }));
            }
        }
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });

    let report = service.committer.commit_pending().unwrap();
    assert_eq!(report.records, 2 * THREADS * PER_THREAD + shared.len());
    let commits = service.commits();
    let mut by_pod = HashMap::new();
    for (committer, pod) in &commits {
        // Every commit is one front's, and carries its committer.
        assert_eq!(Some(committer), pod.as_ref(), "{commits:?}");
        *by_pod.entry(committer.clone()).or_insert(0) += 1;
    }
    assert_eq!(by_pod.len(), 2, "{by_pod:?}");
    for f in 0..2 {
        for t in 0..THREADS {
            for i in 0..PER_THREAD {
                let name = format!("f{f}/t{t}/{i}");
                assert_eq!(service.head_blob(&name).unwrap(), name.as_bytes());
            }
        }
    }
    // History keeps the boundary received last: the highest sequence
    // number, which is also what the backing tree holds.
    let (_, last) = shared.last_key_value().unwrap();
    assert_eq!(service.head_blob("shared").unwrap(), last.as_bytes());
    assert_eq!(
        std::fs::read_to_string(env.tree.join("shared")).unwrap(),
        *last
    );
    for front in &fronts {
        let events = front.sink.take();
        assert!(events.iter().all(|e| !e.is_alarm()), "{events:?}");
    }
}

#[test]
fn a_write_whose_path_another_front_removed_meanwhile_is_skipped() {
    // Front A froze `log` from its open descriptor; before its record
    // reaches the service, front B removed `log` from the tree (and
    // records the removal itself). A's late write must not bring the path
    // back into HEAD (design §14).
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    close(&env, &a.recorder, "log", "1").unwrap().unwrap();
    std::fs::remove_file(env.tree.join("log")).unwrap();
    let elsewhere = env.tree.parent().unwrap().join("open-descriptor");
    std::fs::write(&elsewhere, "2").unwrap();
    let recorded = a
        .recorder
        .record(
            Trigger::Close,
            CALLER,
            vec![Input::Write {
                path: RepoPath::new("log").unwrap(),
                backing: elsewhere.clone(),
                executable: false,
            }],
        )
        .unwrap();
    assert_eq!(recorded.seq, None);
    assert_eq!(recorded.skipped.len(), 1);
    assert_eq!(recorded.skipped[0].reason, SkipReason::Gone);
    // The same when the path's directory became a symlink.
    std::fs::create_dir(env.tree.join("real")).unwrap();
    std::os::unix::fs::symlink("real", env.tree.join("d")).unwrap();
    std::fs::write(env.tree.join("real/f"), "x").unwrap();
    let recorded = a
        .recorder
        .record(
            Trigger::Close,
            CALLER,
            vec![Input::Write {
                path: RepoPath::new("d/f").unwrap(),
                backing: elsewhere,
                executable: false,
            }],
        )
        .unwrap();
    assert_eq!(recorded.seq, None);
    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob("log").unwrap(), b"1");
    assert_eq!(service.head_blob("d/f"), None);
    // No frozen copy is left behind.
    assert_eq!(slot_copies(&env, a._lease.slot()), 0);
}

#[test]
fn fronts_ask_the_service_whether_ignored_paths_are_tracked() {
    let env = env();
    {
        let service = Service::start(&env);
        let local = &service.recorder;
        local
            .record(Trigger::Close, CALLER, vec![write(&env, "kept.log", "0")])
            .unwrap();
        service.committer.commit_pending().unwrap();
    }
    std::fs::write(env.state.git().join("info/exclude"), "*.log\n").unwrap();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");

    let new = a
        .recorder
        .record(Trigger::Close, CALLER, vec![write(&env, "new.log", "1")])
        .unwrap();
    assert_eq!(new.seq, None);
    assert_eq!(new.skipped.len(), 1);
    assert_eq!(slot_copies(&env, 1), 0, "an ignored path costs no copy");

    let kept = a
        .recorder
        .record(Trigger::Close, CALLER, vec![write(&env, "kept.log", "2")])
        .unwrap();
    assert!(kept.seq.is_some(), "{kept:?}");
    assert!(
        a.recorder
            .would_record(&RepoPath::new("kept.log").unwrap(), None)
            .unwrap()
            .is_none()
    );
    assert!(
        a.recorder
            .would_record(&RepoPath::new("other.log").unwrap(), None)
            .unwrap()
            .is_some()
    );

    // A removal of nothing tracked is the service's to skip.
    let removed = a
        .recorder
        .record(
            Trigger::Unlink,
            CALLER,
            vec![Input::Remove {
                path: RepoPath::new("never").unwrap(),
            }],
        )
        .unwrap();
    assert_eq!(removed.seq, None);
    assert_eq!(removed.skipped.len(), 1);

    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob("kept.log").unwrap(), b"2");
    assert_eq!(service.head_blob("new.log"), None);
}

fn alarms(sink: &MemorySink) -> Vec<Event> {
    sink.take()
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                Event::RecordFailed { .. }
                    | Event::HistoryServiceUnavailable { .. }
                    | Event::HistoryServiceRestored { .. }
            )
        })
        .collect()
}

#[test]
fn a_service_outage_fails_boundaries_until_it_is_back() {
    let env = env();
    let mut service = Service::start(&env);
    let a = front(&env, "pod-a");
    close(&env, &a.recorder, "before", "1").unwrap();

    drop(service.server.take());
    for name in ["during-1", "during-2"] {
        let err = close(&env, &a.recorder, name, "x").unwrap_err();
        assert!(
            matches!(&err, VolumeError::HistoryService(e) if !e.maybe_journaled()),
            "{err:?}"
        );
    }
    let events = alarms(&a.sink);
    let unavailable = events
        .iter()
        .filter(|e| matches!(e, Event::HistoryServiceUnavailable { .. }))
        .count();
    let failed = events
        .iter()
        .filter(|e| matches!(e, Event::RecordFailed { .. }))
        .count();
    assert_eq!((unavailable, failed), (1, 2), "{events:?}");
    assert!(events.iter().all(Event::is_alarm));
    // Never sent, so its copies are gone.
    assert_eq!(slot_copies(&env, 1), 1, "only the committed-to-be copy");

    service.listen(&env);
    close(&env, &a.recorder, "after", "2").unwrap().unwrap();
    let events = alarms(&a.sink);
    assert!(
        matches!(&events[..], [Event::HistoryServiceRestored { .. }]),
        "{events:?}"
    );
    service.committer.commit_pending().unwrap();
    assert!(service.head_blob("before").is_some());
    assert!(service.head_blob("after").is_some());
    assert!(service.head_blob("during-1").is_none());
}

#[test]
fn a_hung_service_times_out() {
    let env = env();
    let lease = FrontLease::acquire(&env.state, "pod=a").unwrap();
    let dir = env.state.history_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let listener = UnixListener::bind(env.state.history_socket()).unwrap();
    let branch = agentfs_core::history::History::open(&env.state.git())
        .unwrap()
        .settings()
        .branch_ref
        .clone();
    let fake = {
        let branch = branch.clone();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_frame_bytes(&mut stream).unwrap();
            write_frame(
                &mut stream,
                &Reply::Welcome {
                    volume: "vol".into(),
                    branch,
                    service_pid: 1,
                    features: Vec::new(),
                },
            )
            .unwrap();
            // Reads the request and never answers.
            let _ = read_frame_bytes(&mut stream);
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
        })
    };
    let sink = Arc::new(MemorySink::new());
    let client = connect(
        &env,
        &lease,
        pod("pod-a"),
        "vol",
        Duration::from_millis(300),
        sink.clone(),
    )
    .unwrap();
    let recorder = volume::open_front(
        &env.state,
        &env.tree,
        sink.clone(),
        &Telemetry::noop(),
        Arc::new(client),
    )
    .unwrap();
    let start = Instant::now();
    let err = close(&env, &recorder, "a", "1").unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(5));
    let VolumeError::HistoryService(e) = &err else {
        panic!("{err:?}")
    };
    assert!(
        matches!(e, ClientError::Timeout { sent: true, .. }),
        "{e:?}"
    );
    assert!(e.maybe_journaled());
    assert_eq!(
        slot_copies(&env, 1),
        1,
        "a request that may be journaled keeps its copy"
    );
    drop(recorder);
    fake.join().unwrap();
}

#[test]
fn a_busy_service_fails_the_boundary_without_an_outage_alarm() {
    let env = env();
    let lease = FrontLease::acquire(&env.state, "pod=a").unwrap();
    std::fs::create_dir_all(env.state.history_dir()).unwrap();
    let listener = UnixListener::bind(env.state.history_socket()).unwrap();
    let branch = agentfs_core::history::History::open(&env.state.git())
        .unwrap()
        .settings()
        .branch_ref
        .clone();
    let fake = std::thread::spawn(move || {
        // Welcomes the first connection and closes it, as an idle timeout
        // would; then is at its connection limit.
        let (mut stream, _) = listener.accept().unwrap();
        read_frame_bytes(&mut stream).unwrap();
        write_frame(
            &mut stream,
            &Reply::Welcome {
                volume: "vol".into(),
                branch,
                service_pid: 1,
                features: Vec::new(),
            },
        )
        .unwrap();
        drop(stream);
        let (mut stream, _) = listener.accept().unwrap();
        write_frame(
            &mut stream,
            &Reply::Busy {
                reason: "already serves 1024 connections".into(),
            },
        )
        .unwrap();
    });
    let sink = Arc::new(MemorySink::new());
    let client = connect(
        &env,
        &lease,
        pod("pod-a"),
        "vol",
        Duration::from_secs(5),
        sink.clone(),
    )
    .unwrap();
    let recorder = volume::open_front(
        &env.state,
        &env.tree,
        sink.clone(),
        &Telemetry::noop(),
        Arc::new(client),
    )
    .unwrap();
    // Let the close reach the client's pool.
    std::thread::sleep(Duration::from_millis(100));
    let err = close(&env, &recorder, "a", "1").unwrap_err();
    let VolumeError::HistoryService(e) = &err else {
        panic!("{err:?}")
    };
    assert!(matches!(e, ClientError::Busy { .. }), "{e:?}");
    assert!(!e.maybe_journaled());
    assert_eq!(slot_copies(&env, 1), 0, "nothing was sent");
    let events = sink.take();
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, Event::HistoryServiceUnavailable { .. })),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::RecordFailed { .. })),
        "{events:?}"
    );
    fake.join().unwrap();
}

#[test]
fn the_service_refuses_fronts_it_cannot_serve() {
    let env = env();
    let _service = Service::start(&env);
    let lease = FrontLease::acquire(&env.state, "pod=a").unwrap();
    let null: Events = Arc::new(NullSink);
    let refused = |result: Result<HistoryClient, ClientError>| match result {
        Err(ClientError::Refused { reason, .. }) => reason,
        Err(e) => panic!("{e:?}"),
        Ok(_) => panic!("admitted"),
    };
    let timeout = Duration::from_secs(5);

    let reason = refused(connect(
        &env,
        &lease,
        pod("a"),
        "other",
        timeout,
        null.clone(),
    ));
    assert!(reason.contains("serves volume \"vol\""), "{reason}");

    let evil = Origin {
        replica: "evil <x>".into(),
        ..pod("evil")
    };
    let reason = refused(connect(&env, &lease, evil, "vol", timeout, null.clone()));
    assert!(reason.contains("identity templates"), "{reason}");

    let other = FrontLease::acquire(&env.state, "pod=b").unwrap();
    let slot = other.slot();
    drop(other);
    let unleased = FrontInfo {
        slot,
        ..front_info(&lease, pod("b"))
    };
    let mut stream =
        agentfs_core::local_socket::connect(&env.state.history_dir(), "history.sock").unwrap();
    let hello = serde_json::json!({"v": 1, "volume": "vol", "front": unleased});
    write_frame(&mut stream, &hello).unwrap();
    match read_frame::<Reply>(&mut stream).unwrap() {
        Reply::Refused { reason } => assert!(reason.contains("not leased"), "{reason}"),
        other => panic!("{other:?}"),
    }

    let mut stream =
        agentfs_core::local_socket::connect(&env.state.history_dir(), "history.sock").unwrap();
    write_frame(&mut stream, &serde_json::json!({"v": 2, "anything": true})).unwrap();
    match read_frame::<Reply>(&mut stream).unwrap() {
        Reply::Refused { reason } => {
            assert!(reason.contains("protocol version 2"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    let mut buf = [0u8; 1];
    assert_eq!(stream.read(&mut buf).unwrap(), 0, "and closes");
    let _ = stream.write_all(b"x");
}

#[test]
fn records_journaled_through_a_service_survive_its_crash() {
    let env = env();
    {
        let service = Service::start(&env);
        let a = front(&env, "pod-a");
        let b = front(&env, "pod-b");
        close(&env, &a.recorder, "a", "1").unwrap().unwrap();
        close(&env, &b.recorder, "b", "2").unwrap().unwrap();
        // A crash: nothing committed.
        drop((a, b, service));
    }
    let service = Service::start(&env);
    let report = service.committer.commit_pending().unwrap();
    assert_eq!(report.records, 2);
    let committers: Vec<String> = service.commits().into_iter().map(|(c, _)| c).collect();
    assert_eq!(committers, ["pod-a", "pod-b"]);
    assert_eq!(service.head_blob("a").unwrap(), b"1");
}

#[test]
fn a_dead_fronts_orphaned_copies_are_reclaimed_when_its_slot_is_leased_again() {
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    let slot = a._lease.slot();
    // Journaled, not committed yet: its copy must survive.
    close(&env, &a.recorder, "kept", "1").unwrap().unwrap();
    // Frozen but never journaled: the front died between freeze and send,
    // or the service died before journaling the request.
    let orphan = env
        .state
        .pending()
        .join(format!("{:016x}", (u64::from(slot) << 48) | 0xffff));
    std::fs::write(&orphan, "lost").unwrap();
    assert_eq!(slot_copies(&env, slot), 2);
    drop(a);
    // Timestamps are coarse: the next lease is clearly later.
    std::thread::sleep(Duration::from_millis(50));
    let c = front(&env, "pod-c");
    assert_eq!(c._lease.slot(), slot);
    assert!(!orphan.exists(), "the orphan is reclaimed");
    // The journaled copy is still there and commits; the new front's own
    // copies are untouched.
    close(&env, &c.recorder, "new", "2").unwrap().unwrap();
    let report = service.committer.commit_pending().unwrap();
    assert_eq!(report.records, 2);
    assert_eq!(service.head_blob("kept").unwrap(), b"1");
    assert_eq!(service.head_blob("new").unwrap(), b"2");
    assert_eq!(slot_copies(&env, slot), 0);
}

/// A write of `path` frozen from `backing` (outside the tree: a front's
/// open descriptor, frozen earlier than it is recorded).
fn write_from(path: &str, backing: &Path) -> Input {
    Input::Write {
        path: RepoPath::new(path).unwrap(),
        backing: backing.to_owned(),
        executable: false,
    }
}

/// Long enough for the coarse clock behind `ctime` to move.
const TICK: Duration = Duration::from_millis(30);

static NEXT_COPY: AtomicU64 = AtomicU64::new(0xff00);

/// Sends `front`'s record of a write of `path` with `content`, frozen with
/// `stamp`: a front whose freeze happened at `stamp`'s time and whose
/// record arrives now.
fn send_frozen(
    env: &Env,
    front: &Front,
    path: &str,
    content: &str,
    stamp: FreezeStamp,
) -> Recorded {
    let id = PendingId::in_slot(
        front._lease.slot(),
        NEXT_COPY.fetch_add(1, Ordering::Relaxed),
    );
    std::fs::write(env.state.pending().join(format!("{:016x}", id.0)), content).unwrap();
    let path = RepoPath::new(path).unwrap();
    let draft = Draft {
        trigger: Trigger::Close,
        caller: CALLER,
        time_ms: 1,
        changes: vec![Change::Write {
            path: path.clone(),
            pending: id,
            executable: false,
            store: Store::Blob,
        }],
        operation: None,
        atomic: false,
        origin: None,
    };
    front
        .recorder
        .history_client()
        .unwrap()
        .record(draft, vec![PathStamp { path, stamp }])
        .unwrap()
}

fn stamp_of(env: &Env, path: &str) -> FreezeStamp {
    FreezeStamp::from_metadata(&std::fs::metadata(env.tree.join(path)).unwrap())
}

#[test]
fn a_write_frozen_before_a_newer_journaled_one_is_superseded() {
    // Front A froze `shared` and its record is late; front B wrote and
    // recorded a newer state meanwhile. A's late record must not put HEAD
    // back on A's state (design §14.1).
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    let b = front(&env, "pod-b");
    close(&env, &b.recorder, "shared", "bb").unwrap().unwrap();
    let journaled = stamp_of(&env, "shared");
    let older = FreezeStamp {
        ctime_ns: journaled.ctime_ns - 1_000_000,
        size: 1,
        ..journaled
    };
    let late = send_frozen(&env, &a, "shared", "a", older);
    assert_eq!(late.seq, None, "{late:?}");
    assert_eq!(late.skipped[0].reason, SkipReason::Superseded);
    // The same tick, another state: the file still has the journaled
    // stamp, so the incoming one is older.
    let tie = FreezeStamp {
        size: 7,
        ..journaled
    };
    let late = send_frozen(&env, &a, "shared", "a", tie);
    assert_eq!(late.skipped[0].reason, SkipReason::Superseded);
    // The identical stamp (the racy-timestamp re-record) is journaled.
    assert!(
        send_frozen(&env, &a, "shared", "bb", journaled)
            .seq
            .is_some()
    );
    assert_eq!(
        slot_copies(&env, a._lease.slot()),
        1,
        "skipped copies are dropped"
    );
    // A's next write is newer, and is journaled.
    std::thread::sleep(TICK);
    close(&env, &a.recorder, "shared", "ccc").unwrap().unwrap();
    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob("shared").unwrap(), b"ccc");

    // In the other order (the older first), both are journaled, the newer
    // last.
    std::fs::write(env.tree.join("other"), "d").unwrap();
    let first = stamp_of(&env, "other");
    std::thread::sleep(TICK);
    assert!(send_frozen(&env, &a, "other", "d", first).seq.is_some());
    close(&env, &b.recorder, "other", "ee").unwrap().unwrap();
    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob("other").unwrap(), b"ee");
}

#[test]
fn a_write_of_a_file_renamed_over_meanwhile_is_skipped() {
    // Front A froze `f` from its descriptor; front B renamed a new file
    // over `f` before A's record arrived: A's file is no longer at the
    // path (design §14.1).
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    let b = front(&env, "pod-b");
    std::fs::write(env.tree.join("f"), "old").unwrap();
    let held = env.tree.parent().unwrap().join("held");
    std::fs::hard_link(env.tree.join("f"), &held).unwrap();
    std::fs::write(env.tree.join("f.tmp"), "new").unwrap();
    std::fs::rename(env.tree.join("f.tmp"), env.tree.join("f")).unwrap();
    close(&env, &b.recorder, "f", "new").unwrap().unwrap();
    let late = a
        .recorder
        .record(Trigger::Close, CALLER, vec![write_from("f", &held)])
        .unwrap();
    assert_eq!(late.seq, None, "{late:?}");
    assert_eq!(late.skipped[0].reason, SkipReason::Gone);
    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob("f").unwrap(), b"new");
}

#[test]
fn the_services_own_writes_supersede_older_front_writes() {
    // A restore the service applies records through its local recorder:
    // a front's record frozen before it is older (design §14.1, §15).
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    service
        .recorder
        .record(Trigger::Restore, CALLER, vec![write(&env, "f", "restored")])
        .unwrap();
    let restored = stamp_of(&env, "f");
    let before = FreezeStamp {
        ctime_ns: restored.ctime_ns - 1_000_000,
        size: 8,
        ..restored
    };
    let late = send_frozen(&env, &a, "f", "workload", before);
    assert_eq!(late.seq, None, "{late:?}");
    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob("f").unwrap(), b"restored");
}

#[test]
fn atomic_groups_are_never_superseded() {
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    let b = front(&env, "pod-b");
    let stale = env.tree.parent().unwrap().join("a-descriptor");
    std::fs::write(&stale, "old").unwrap();
    std::thread::sleep(TICK);
    close(&env, &b.recorder, "g", "new").unwrap().unwrap();
    let group = a
        .recorder
        .record_group(Trigger::Fsync, CALLER, vec![write_from("g", &stale)])
        .unwrap();
    assert!(group.seq.is_some(), "a group is recorded whole: {group:?}");
    drop(service);
}

/// How many rounds the race tests run (`AGENTFS_RACE_ITERS`).
fn race_iterations() -> usize {
    std::env::var("AGENTFS_RACE_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25)
}

/// A well-mixed pseudo-random number from `x`.
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Two fronts, two threads each, write `shared` `writes` times per thread
/// with `write_one(n, front)`, a different length each time so no two
/// states share a stamp; after each round HEAD must be what the tree
/// holds, whatever order the records reached the service in (design
/// §14.1).
fn race(writes: usize, write_one: impl Fn(&Env, &Recorder, usize) + Sync) {
    let env = env();
    let service = Service::start(&env);
    let fronts = [front(&env, "pod-a"), front(&env, "pod-b")];
    let next = AtomicU64::new(1);
    for round in 0..race_iterations() {
        std::thread::scope(|s| {
            for front in fronts.iter().chain(&fronts) {
                let (env, next, write_one) = (&env, &next, &write_one);
                s.spawn(move || {
                    for _ in 0..writes {
                        let n = next.fetch_add(1, Ordering::Relaxed) as usize;
                        write_one(env, &front.recorder, n);
                    }
                });
            }
        });
        service.committer.commit_pending().unwrap();
        assert_eq!(
            service.head_blob("shared").unwrap().len(),
            std::fs::read(env.tree.join("shared")).unwrap().len(),
            "round {round}: HEAD is not the tree's last state"
        );
    }
    for front in &fronts {
        assert_eq!(slot_copies(&env, front._lease.slot()), 0);
    }
}

#[test]
fn racing_in_place_writes_leave_head_on_the_trees_last_state() {
    race(10, |env, recorder, n| {
        let backing = env.tree.join("shared");
        std::fs::write(&backing, "x".repeat(n)).unwrap();
        recorder
            .record(Trigger::Close, CALLER, vec![write_from("shared", &backing)])
            .unwrap();
    });
}

#[test]
fn racing_renames_over_a_path_leave_head_on_the_trees_last_state() {
    // Each writer renames a new file over `shared` (as editors and atomic
    // writers do), then records that file from its own descriptor after a
    // random delay: a late record of a file already replaced, or older
    // than one journaled, must not reach HEAD.
    let scratch = tempfile::tempdir().unwrap();
    race(2, |env, recorder, n| {
        let tmp = env.tree.join(format!(".tmp-{n}"));
        let held = scratch.path().join(format!("held-{n}"));
        std::fs::write(&tmp, "x".repeat(n)).unwrap();
        // The writer's open descriptor, as a path to the same inode.
        std::fs::hard_link(&tmp, &held).unwrap();
        std::fs::rename(&tmp, env.tree.join("shared")).unwrap();
        let delay = splitmix(n as u64) % 30_000;
        std::thread::sleep(Duration::from_micros(delay));
        recorder
            .record(Trigger::Rename, CALLER, vec![write_from("shared", &held)])
            .unwrap();
        std::fs::remove_file(&held).unwrap();
    });
}

#[test]
fn an_inherited_repair_commits_as_the_pod_that_failed_it() {
    let env = env();
    let service = Service::start(&env);
    let b = front(&env, "pod-b");
    // A sweep record naming its predecessor's pod: committed as that pod.
    b.recorder
        .record_as(
            Trigger::Sweep,
            CALLER,
            vec![write(&env, "inherited", "1")],
            Some(pod("pod-a")),
        )
        .unwrap();
    service.committer.commit_pending().unwrap();
    // Any other trigger cannot claim another pod.
    b.recorder
        .record_as(
            Trigger::Close,
            CALLER,
            vec![write(&env, "own", "2")],
            Some(pod("pod-a")),
        )
        .unwrap();
    service.committer.commit_pending().unwrap();
    // Its own origin, named, is its own.
    b.recorder
        .record_as(
            Trigger::Sweep,
            CALLER,
            vec![write(&env, "own-sweep", "3")],
            Some(pod("pod-b")),
        )
        .unwrap();
    service.committer.commit_pending().unwrap();
    let committers: Vec<String> = service.commits().into_iter().map(|(c, _)| c).collect();
    assert_eq!(committers, ["pod-a", "pod-b", "pod-b"]);
}

#[test]
fn ping_measures_a_round_trip() {
    let env = env();
    let _service = Service::start(&env);
    let lease = FrontLease::acquire(&env.state, "pod=a").unwrap();
    let client = connect(
        &env,
        &lease,
        pod("a"),
        "vol",
        Duration::from_secs(5),
        Arc::new(NullSink),
    )
    .unwrap();
    assert!(client.ping().unwrap() < Duration::from_secs(5));
    assert_eq!(client.address(), &env.address);
}

fn percentiles(mut samples: Vec<Duration>) -> String {
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize].as_secs_f64() * 1e6;
    let mean = samples.iter().map(Duration::as_secs_f64).sum::<f64>() / samples.len() as f64 * 1e6;
    format!(
        "p50 {:>8.1} µs  p90 {:>8.1} µs  p99 {:>8.1} µs  mean {:>8.1} µs  (n={})",
        at(0.5),
        at(0.9),
        at(0.99),
        mean,
        samples.len()
    )
}

fn time_records(
    env: &Env,
    recorder: &Recorder,
    prefix: &str,
    threads: usize,
    n: usize,
) -> Vec<Duration> {
    let content = "x".repeat(4096);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let content = &content;
                s.spawn(move || {
                    (0..n / threads)
                        .map(|i| {
                            let input = write(env, &format!("{prefix}/{t}/{i}"), content);
                            let start = Instant::now();
                            recorder
                                .record(Trigger::Close, CALLER, vec![input])
                                .unwrap();
                            start.elapsed()
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    })
}

/// The cost of a boundary through a front against single-process mode
/// (design §14, §19.2). Prints numbers; asserts nothing about them.
#[test]
#[ignore = "benchmark: run with --release --ignored --nocapture"]
fn round_trip_cost() {
    const N: usize = 2000;
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    let lease = FrontLease::acquire(&env.state, "pod=bench").unwrap();
    let client = connect(
        &env,
        &lease,
        pod("bench"),
        "vol",
        Duration::from_secs(10),
        Arc::new(NullSink),
    )
    .unwrap();

    println!("### Boundary cost, 4 KiB close, {N} records (design §14)");
    println!();
    println!("```");
    let pings: Vec<Duration> = (0..N).map(|_| client.ping().unwrap()).collect();
    println!("{:<34} {}", "ping round trip", percentiles(pings));
    for threads in [1, 8] {
        let local = time_records(
            &env,
            &service.recorder,
            &format!("local{threads}"),
            threads,
            N,
        );
        println!(
            "{:<34} {}",
            format!("single-process record, {threads} thr"),
            percentiles(local)
        );
        let front = time_records(&env, &a.recorder, &format!("front{threads}"), threads, N);
        println!(
            "{:<34} {}",
            format!("front record, {threads} thr"),
            percentiles(front)
        );
    }
    println!("```");
    println!();
    println!(
        "Environment: {} on {}",
        std::env::consts::ARCH,
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim()
    );
}
