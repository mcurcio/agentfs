//! Two fronts and one history service in one process, over a real Unix
//! socket (design §14). No FUSE: fronts record through their `Recorder`
//! exactly as a FUSE mount's boundary layer would.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentfs_core::events::{Event, Events, MemorySink, NullSink};
use agentfs_core::history_service::protocol::{
    FrontInfo, Reply, read_frame, read_frame_bytes, write_frame,
};
use agentfs_core::history_service::{
    Address, ClientConfig, ClientError, FrontLease, HistoryClient, HistoryServer,
};
use agentfs_core::identity::Context;
use agentfs_core::paths::RepoPath;
use agentfs_core::record::{Caller, Origin, Trigger};
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self, Committer, Input, Recorder, StateDir, VolumeError};

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
        let (recorder, committer, _) = volume::open(
            &env.state,
            &env.tree,
            service_context(),
            Arc::new(NullSink),
            &Telemetry::noop(),
        )
        .unwrap();
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

fn front_info(lease: &FrontLease, origin: Origin) -> FrontInfo {
    FrontInfo {
        origin,
        slot: lease.slot(),
        instance: u64::from(std::process::id()) << 16 | u64::from(lease.slot()),
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
