//! Two fronts and one history service in one process, over a real Unix
//! socket (design §14). No FUSE: fronts record through their `Recorder`
//! exactly as a FUSE mount's boundary layer would.

use std::collections::{BTreeMap, HashMap};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentfs_core::events::{Event, Events, MemorySink, NullSink};
use agentfs_core::history_service::wire::pb;
use agentfs_core::history_service::{
    Address, ClientConfig, ClientError, FrontHooks, FrontInfo, FrontLease, FrontsError,
    HistoryClient, HistoryServer, NoHooks, PROTOCOL_VERSION, PathStamp, SERVICE_NAME,
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
use hyper_util::rt::TokioIo;
use pb::history_client::HistoryClient as RawClient;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Request, Response, Status, Streaming};

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
        views: Vec::new(),
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

// ---- fake services: tonic over the same socket, misbehaving on purpose ----

/// What a fake service saw.
#[derive(Default)]
struct Seen {
    hello: AtomicUsize,
    record: AtomicUsize,
    tracked: AtomicUsize,
    still_last: AtomicUsize,
    /// Each call's `grpc-timeout`, by call.
    deadlines: Mutex<Vec<(&'static str, Option<Duration>)>>,
}

/// What a fake service does with a record.
#[derive(Clone)]
enum OnRecord {
    /// Answers at once, journaled as seq 1.
    Answer,
    /// Never answers.
    Hang,
    /// Drops the first record's connection without answering; answers
    /// any later one.
    DropFirst,
    /// Answers once every one of the barrier's records has arrived.
    Together(Arc<tokio::sync::Barrier>),
}

#[derive(Clone)]
struct Fake {
    branch: String,
    protocol_version: u32,
    features: Vec<String>,
    /// Hello answers RESOURCE_EXHAUSTED.
    busy: bool,
    on_record: OnRecord,
    seen: Arc<Seen>,
}

impl Fake {
    fn new(env: &Env, on_record: OnRecord) -> Self {
        Self {
            branch: branch(env),
            protocol_version: PROTOCOL_VERSION,
            features: vec!["still_last".into()],
            busy: false,
            on_record,
            seen: Arc::default(),
        }
    }

    fn note<T>(&self, call: &'static str, request: &Request<T>) {
        let deadline = request
            .metadata()
            .get("grpc-timeout")
            .and_then(|v| v.to_str().ok())
            .and_then(grpc_timeout);
        self.seen.deadlines.lock().unwrap().push((call, deadline));
    }
}

/// A `grpc-timeout` value: digits and a unit (H M S m u n).
fn grpc_timeout(value: &str) -> Option<Duration> {
    let (digits, unit) = value.split_at(value.len().checked_sub(1)?);
    let n: u64 = digits.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(n * 3600),
        "M" => Duration::from_secs(n * 60),
        "S" => Duration::from_secs(n),
        "m" => Duration::from_millis(n),
        "u" => Duration::from_micros(n),
        "n" => Duration::from_nanos(n),
        _ => return None,
    })
}

#[tonic::async_trait]
impl pb::history_server::History for Fake {
    async fn hello(
        &self,
        request: Request<pb::HelloRequest>,
    ) -> Result<Response<pb::Welcome>, Status> {
        self.note("hello", &request);
        self.seen.hello.fetch_add(1, Ordering::SeqCst);
        if self.busy {
            return Err(Status::resource_exhausted(
                "already serves 1024 front connections",
            ));
        }
        Ok(Response::new(pb::Welcome {
            protocol_version: self.protocol_version,
            volume: "vol".into(),
            branch: self.branch.clone(),
            service_pid: 1,
            features: self.features.clone(),
        }))
    }

    async fn tracked(
        &self,
        request: Request<pb::TrackedRequest>,
    ) -> Result<Response<pb::TrackedReply>, Status> {
        self.note("tracked", &request);
        self.seen.tracked.fetch_add(1, Ordering::SeqCst);
        let tracked = vec![false; request.into_inner().paths.len()];
        Ok(Response::new(pb::TrackedReply { tracked }))
    }

    async fn record(
        &self,
        request: Request<pb::RecordRequest>,
    ) -> Result<Response<pb::RecordReply>, Status> {
        self.note("record", &request);
        self.seen.record.fetch_add(1, Ordering::SeqCst);
        match &self.on_record {
            OnRecord::Answer => {}
            OnRecord::Hang => std::future::pending().await,
            OnRecord::DropFirst => {
                if self.seen.record.load(Ordering::SeqCst) == 1 {
                    let fd = *request.extensions().get::<RawFd>().unwrap();
                    // SAFETY: `fd` is this request's connection, open while
                    // its request is served; shutdown(2) only ends its
                    // traffic, both ways.
                    unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
                    std::future::pending::<()>().await;
                }
            }
            OnRecord::Together(barrier) => {
                barrier.wait().await;
            }
        }
        Ok(Response::new(pb::RecordReply {
            seq: Some(1),
            skipped: Vec::new(),
        }))
    }

    async fn still_last(
        &self,
        request: Request<pb::StillLastRequest>,
    ) -> Result<Response<pb::StillLastReply>, Status> {
        self.note("still_last", &request);
        self.seen.still_last.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(pb::StillLastReply { last: true }))
    }

    type ListenStream = ReceiverStream<Result<pb::FrontCall, Status>>;

    async fn listen(
        &self,
        _: Request<Streaming<pb::FrontAnswer>>,
    ) -> Result<Response<Self::ListenStream>, Status> {
        Err(Status::unimplemented("a fake"))
    }
}

/// A fake service's connection: its descriptor is its connect info, so a
/// handler can drop it.
struct FakeConn(tokio::net::UnixStream);

impl tonic::transport::server::Connected for FakeConn {
    type ConnectInfo = RawFd;

    fn connect_info(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl tokio::io::AsyncRead for FakeConn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for FakeConn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// A fake service on the volume's socket, on its own runtime; dropped,
/// it drops every connection at once.
struct FakeServer {
    kill: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeServer {
    fn start(env: &Env, fake: Fake) -> Self {
        std::fs::create_dir_all(env.state.history_dir()).unwrap();
        let socket = env.state.history_socket();
        let _ = std::fs::remove_file(&socket);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (kill, killed) = std::sync::mpsc::channel();
        let listener = {
            let _runtime = runtime.enter();
            tokio::net::UnixListener::bind(&socket).unwrap()
        };
        runtime.spawn(
            Server::builder()
                .add_service(pb::history_server::HistoryServer::new(fake))
                .serve_with_incoming(UnixListenerStream::new(listener).map(|io| io.map(FakeConn))),
        );
        let thread = std::thread::spawn(move || {
            let _ = killed.recv();
            runtime.shutdown_background();
        });
        Self {
            kill,
            thread: Some(thread),
        }
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        let _ = self.kill.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn branch(env: &Env) -> String {
    agentfs_core::history::History::open(&env.state.git())
        .unwrap()
        .settings()
        .branch_ref
        .clone()
}

/// A front's recorder on `client`.
fn front_on(env: &Env, client: HistoryClient, sink: Events) -> Recorder {
    volume::open_front(
        &env.state,
        &env.tree,
        sink,
        &Telemetry::noop(),
        Arc::new(client),
    )
    .unwrap()
}

/// A raw connection to the volume's socket: no Hello unless the test
/// sends one.
fn raw_channel(runtime: &tokio::runtime::Runtime, env: &Env) -> Channel {
    let socket = env.state.history_socket();
    runtime.block_on(async move {
        Endpoint::from_static("http://agentfs.history")
            .connect_with_connector(tower::service_fn(move |_: tonic::codegen::http::Uri| {
                let socket = socket.clone();
                async move {
                    tokio::net::UnixStream::connect(socket)
                        .await
                        .map(TokioIo::new)
                }
            }))
            .await
            .unwrap()
    })
}

fn hello_of(front: &FrontInfo, version: u32) -> pb::HelloRequest {
    pb::HelloRequest {
        protocol_version: version,
        volume: "vol".into(),
        front: Some(front.into()),
    }
}

fn empty_draft() -> Draft {
    Draft {
        trigger: Trigger::Close,
        caller: CALLER,
        time_ms: 1,
        changes: Vec::new(),
        operation: None,
        atomic: false,
        origin: None,
    }
}

#[test]
fn a_hung_service_times_out() {
    let env = env();
    let lease = FrontLease::acquire(&env.state, "pod=a").unwrap();
    let fake = Fake::new(&env, OnRecord::Hang);
    let _server = FakeServer::start(&env, fake);
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
    let recorder = front_on(&env, client, sink);
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
}

#[test]
fn a_lost_reply_is_never_resent() {
    let env = env();
    let lease = FrontLease::acquire(&env.state, "pod=a").unwrap();
    let fake = Fake::new(&env, OnRecord::DropFirst);
    let seen = fake.seen.clone();
    let server = FakeServer::start(&env, fake);
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
    let recorder = front_on(&env, client, sink);
    let err = close(&env, &recorder, "a", "1").unwrap_err();
    let VolumeError::HistoryService(e) = &err else {
        panic!("{err:?}")
    };
    assert!(matches!(e, ClientError::Lost { sent: true, .. }), "{e:?}");
    assert_eq!(slot_copies(&env, 1), 1, "the copy stays for the service");
    assert_eq!(seen.record.load(Ordering::SeqCst), 1, "never resent");
    drop(server);
}

#[test]
fn a_busy_service_fails_the_boundary_without_an_outage_alarm() {
    let env = env();
    let lease = FrontLease::acquire(&env.state, "pod=a").unwrap();
    // Welcomes the first connection, then drops it; the service behind
    // the socket next is at its connection limit.
    let first = FakeServer::start(&env, Fake::new(&env, OnRecord::Answer));
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
    let recorder = front_on(&env, client, sink.clone());
    drop(first);
    let _busy = FakeServer::start(
        &env,
        Fake {
            busy: true,
            ..Fake::new(&env, OnRecord::Answer)
        },
    );
    // Let the close reach the client's connection.
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
}

#[test]
fn a_service_of_another_protocol_is_refused() {
    let env = env();
    let lease = FrontLease::acquire(&env.state, "pod=a").unwrap();
    let _server = FakeServer::start(
        &env,
        Fake {
            protocol_version: 3,
            ..Fake::new(&env, OnRecord::Answer)
        },
    );
    match connect(
        &env,
        &lease,
        pod("a"),
        "vol",
        Duration::from_secs(5),
        Arc::new(NullSink),
    ) {
        Err(ClientError::Refused { reason, .. }) => {
            assert!(reason.contains("protocol version 3"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_service_without_still_last_is_never_asked() {
    let env = env();
    let lease = FrontLease::acquire(&env.state, "pod=a").unwrap();
    let fake = Fake {
        features: Vec::new(),
        ..Fake::new(&env, OnRecord::Answer)
    };
    let seen = fake.seen.clone();
    let _server = FakeServer::start(&env, fake);
    let client = connect(
        &env,
        &lease,
        pod("a"),
        "vol",
        Duration::from_secs(5),
        Arc::new(NullSink),
    )
    .unwrap();
    assert!(!client.supports("still_last"));
    assert!(!client.still_last(&path("f"), PendingId(1)).unwrap());
    assert_eq!(seen.still_last.load(Ordering::SeqCst), 0);
}

#[test]
fn calls_carry_the_deadline() {
    let env = env();
    let lease = FrontLease::acquire(&env.state, "pod=a").unwrap();
    let fake = Fake::new(&env, OnRecord::Answer);
    let seen = fake.seen.clone();
    let _server = FakeServer::start(&env, fake);
    let timeout = Duration::from_secs(2);
    let client = connect(&env, &lease, pod("a"), "vol", timeout, Arc::new(NullSink)).unwrap();
    client.record(empty_draft(), Vec::new()).unwrap();
    assert_eq!(client.tracked(&[path("x")]).unwrap(), [false]);
    assert!(client.still_last(&path("f"), PendingId(1)).unwrap());
    let deadlines = seen.deadlines.lock().unwrap().clone();
    for call in ["hello", "record", "tracked", "still_last"] {
        let (_, deadline) = deadlines
            .iter()
            .find(|(c, _)| *c == call)
            .unwrap_or_else(|| panic!("no {call}: {deadlines:?}"));
        let deadline = deadline.unwrap_or_else(|| panic!("{call} has no grpc-timeout"));
        assert!(
            deadline > Duration::ZERO && deadline <= timeout,
            "{call}: {deadline:?}"
        );
    }
}

#[test]
fn history_calls_from_many_threads_overlap() {
    // Every record is held until all 8 have arrived: only calls that run
    // at once through the one client finish.
    const THREADS: usize = 8;
    let env = env();
    let lease = FrontLease::acquire(&env.state, "pod=a").unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(THREADS));
    let _server = FakeServer::start(&env, Fake::new(&env, OnRecord::Together(barrier)));
    let client = Arc::new(
        connect(
            &env,
            &lease,
            pod("a"),
            "vol",
            Duration::from_secs(30),
            Arc::new(NullSink),
        )
        .unwrap(),
    );
    let (done, finished) = std::sync::mpsc::channel();
    for _ in 0..THREADS {
        let (client, done) = (client.clone(), done.clone());
        std::thread::spawn(move || {
            let _ = done.send(client.record(empty_draft(), Vec::new()).map(|r| r.seq));
        });
    }
    for n in 0..THREADS {
        let result = finished
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("only {n} of {THREADS} records finished"));
        assert_eq!(result.unwrap(), Some(1));
    }
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

    // Hostile hellos, through the raw generated client.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let other = FrontLease::acquire(&env.state, "pod=b").unwrap();
    let slot = other.slot();
    drop(other);
    let unleased = FrontInfo {
        slot,
        ..front_info(&lease, pod("b"))
    };
    let mut raw = RawClient::new(raw_channel(&runtime, &env));
    let status = runtime
        .block_on(raw.hello(hello_of(&unleased, PROTOCOL_VERSION)))
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status:?}");
    assert!(status.message().contains("not leased"), "{status:?}");

    let status = runtime
        .block_on(raw.hello(hello_of(&front_info(&lease, pod("a")), 1)))
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status:?}");
    assert!(
        status
            .message()
            .contains("protocol version 1 is not supported"),
        "{status:?}"
    );

    // No call before an admitted Hello on the connection.
    let mut fresh = RawClient::new(raw_channel(&runtime, &env));
    let status = runtime
        .block_on(fresh.record(pb::RecordRequest {
            draft: Some((&empty_draft()).into()),
            stamps: Vec::new(),
        }))
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status:?}");
    assert!(status.message().contains("send Hello first"), "{status:?}");
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
fn nfs_silly_renames_are_never_recorded_through_a_front() {
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    let name = ".nfs0000000000a1b2c300000001";
    let recorded = a
        .recorder
        .record(Trigger::Close, CALLER, vec![write(&env, name, "x")])
        .unwrap();
    assert_eq!(recorded.seq, None, "{recorded:?}");
    assert_eq!(recorded.skipped[0].reason, SkipReason::NfsSillyRename);
    service.committer.commit_pending().unwrap();
    assert!(service.head_blob(name).is_none());
    assert!(env.tree.join(name).exists(), "it stays visible");
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

#[test]
fn identical_stamps_from_two_fronts_end_on_the_trees_state() {
    // A coarse timestamp clock (design §14.1): two fronts write the same
    // number of bytes to one path within one tick, so both freezes carry
    // the same stamp; the tree ends on B's "BBBB". Whichever record
    // arrives last, HEAD must end on what the tree holds.
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    let b = front(&env, "pod-b");
    for (n, b_first) in [(0, true), (1, false)] {
        let path = format!("counter-{n}");
        std::fs::write(env.tree.join(&path), "BBBB").unwrap();
        let tick = stamp_of(&env, &path);
        let order: [(&Front, &str); 2] = if b_first {
            [(&b, "BBBB"), (&a, "AAAA")]
        } else {
            [(&a, "AAAA"), (&b, "BBBB")]
        };
        for (front, content) in order {
            let recorded = send_frozen(&env, front, &path, content, tick);
            assert!(recorded.seq.is_some(), "{recorded:?}");
        }
        service.committer.commit_pending().unwrap();
        assert_eq!(
            service.head_blob(&path).unwrap(),
            b"BBBB",
            "B's record first: {b_first}"
        );
    }
    // One front's own re-record of an identical stamp is not resynced.
    std::fs::write(env.tree.join("own"), "1111").unwrap();
    let tick = stamp_of(&env, "own");
    assert!(send_frozen(&env, &a, "own", "1111", tick).seq.is_some());
    std::fs::write(env.tree.join("own"), "2222").unwrap();
    assert!(send_frozen(&env, &a, "own", "1111", tick).seq.is_some());
    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob("own").unwrap(), b"1111");
}

#[test]
fn a_late_removal_of_a_path_made_again_ends_on_the_trees_state() {
    // Front B removed `lock.json`, and its record is late; front A made
    // the path again and recorded it meanwhile. B's removal is journaled,
    // then the service records the path as the tree has it (design §14.1).
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    let b = front(&env, "pod-b");
    close(&env, &a.recorder, "lock.json", "old")
        .unwrap()
        .unwrap();
    std::fs::remove_file(env.tree.join("lock.json")).unwrap();
    close(&env, &a.recorder, "lock.json", "new")
        .unwrap()
        .unwrap();
    let late = b
        .recorder
        .record(
            Trigger::Unlink,
            CALLER,
            vec![Input::Remove {
                path: RepoPath::new("lock.json").unwrap(),
            }],
        )
        .unwrap();
    assert!(late.seq.is_some(), "{late:?}");
    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob("lock.json").unwrap(), b"new");

    // A symlink record whose target the tree no longer has.
    std::os::unix::fs::symlink("new-target", env.tree.join("link")).unwrap();
    let late = b
        .recorder
        .record(
            Trigger::Symlink,
            CALLER,
            vec![Input::Symlink {
                path: RepoPath::new("link").unwrap(),
                target: "old-target".into(),
            }],
        )
        .unwrap();
    assert!(late.seq.is_some(), "{late:?}");
    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob("link").unwrap(), b"new-target");

    // A removal of a path still absent is journaled alone.
    std::fs::remove_file(env.tree.join("lock.json")).unwrap();
    b.recorder
        .record(
            Trigger::Unlink,
            CALLER,
            vec![Input::Remove {
                path: RepoPath::new("lock.json").unwrap(),
            }],
        )
        .unwrap();
    service.committer.commit_pending().unwrap();
    assert!(service.head_blob("lock.json").is_none());
}

#[test]
fn a_backward_clock_step_does_not_supersede_the_files_current_state() {
    // The wall clock, which ctime follows, stepped back after `f` was
    // journaled: later writes have older ctimes than the noted one, but
    // a write that froze the file as it still is must be journaled.
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    let b = front(&env, "pod-b");
    std::fs::write(env.tree.join("f"), "before").unwrap();
    let real = stamp_of(&env, "f");
    let future = FreezeStamp {
        ctime_ns: real.ctime_ns + 60_000_000_000,
        mtime_ns: real.mtime_ns + 60_000_000_000,
        ..real
    };
    assert!(send_frozen(&env, &b, "f", "before", future).seq.is_some());
    std::thread::sleep(TICK);
    close(&env, &a.recorder, "f", "after the step")
        .unwrap()
        .expect("the file's current state is journaled");
    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob("f").unwrap(), b"after the step");
    std::thread::sleep(TICK);
    close(&env, &a.recorder, "f", "and again").unwrap().unwrap();
    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob("f").unwrap(), b"and again");
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
/// with `write_one(n, front)`; after each round HEAD must be what the tree
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
            service.head_blob("shared").unwrap(),
            std::fs::read(env.tree.join("shared")).unwrap(),
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
fn racing_same_size_writes_leave_head_on_the_trees_last_state() {
    // Every state has the same size: only ctime (and, within one tick of a
    // coarse clock, the resync) orders them (design §14.1).
    race(10, |env, recorder, n| {
        let backing = env.tree.join("shared");
        std::fs::write(&backing, format!("{n:08}")).unwrap();
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

#[test]
fn health_follows_the_service() {
    use tonic_health::pb::HealthCheckRequest;
    use tonic_health::pb::health_check_response::ServingStatus;
    use tonic_health::pb::health_client::HealthClient;

    let env = env();
    let mut service = Service::start(&env);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut health = HealthClient::new(raw_channel(&runtime, &env));
    let request = || HealthCheckRequest {
        service: SERVICE_NAME.to_owned(),
    };
    let checked = runtime
        .block_on(health.check(request()))
        .unwrap()
        .into_inner();
    assert_eq!(checked.status, ServingStatus::Serving as i32);
    // Once a connection has seen the service stop it opens no new call:
    // a watch opened before sees the change.
    let mut watch = runtime
        .block_on(health.watch(request()))
        .unwrap()
        .into_inner();
    let first = runtime.block_on(watch.message()).unwrap().unwrap();
    assert_eq!(first.status, ServingStatus::Serving as i32);
    let server = service.server.take().unwrap();
    let stopping = std::thread::spawn(move || server.shutdown(Duration::from_secs(10)));
    let next = runtime.block_on(watch.message()).unwrap().unwrap();
    assert_eq!(next.status, ServingStatus::NotServing as i32);
    drop((watch, health));
    assert!(stopping.join().unwrap(), "the service stopped in time");
}

#[test]
fn a_restarted_service_does_not_fail_the_next_boundary() {
    let env = env();
    let mut service = Service::start(&env);
    let a = front(&env, "pod-a");
    close(&env, &a.recorder, "before", "1").unwrap().unwrap();
    assert!(
        service
            .server
            .take()
            .unwrap()
            .shutdown(Duration::from_secs(5))
    );
    service.listen(&env);
    // The connection the front holds is closed: the record goes, once, on
    // a new one.
    close(&env, &a.recorder, "after", "2").unwrap().unwrap();
    assert!(alarms(&a.sink).is_empty());
    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob("after").unwrap(), b"2");
}

#[test]
fn a_record_over_4_mib_is_journaled() {
    use prost::Message as _;

    const CHANGES: usize = 50_000;
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    let dir = env.tree.join("links");
    std::fs::create_dir_all(&dir).unwrap();
    let name = |n: usize| format!("links/{n:0>90}");
    for n in 0..CHANGES {
        std::os::unix::fs::symlink("target", env.tree.join(name(n))).unwrap();
    }
    let draft = Draft {
        trigger: Trigger::Symlink,
        changes: (0..CHANGES)
            .map(|n| Change::Symlink {
                path: path(&name(n)),
                target: "target".into(),
            })
            .collect(),
        ..empty_draft()
    };
    let encoded = pb::RecordRequest {
        draft: Some((&draft).into()),
        stamps: Vec::new(),
    }
    .encoded_len();
    assert!(encoded > 4 << 20, "only {encoded} bytes");
    let recorded = a
        .recorder
        .history_client()
        .unwrap()
        .record(draft, Vec::new())
        .unwrap();
    assert!(recorded.seq.is_some(), "{recorded:?}");
    assert!(recorded.skipped.is_empty());
    service.committer.commit_pending().unwrap();
    assert_eq!(service.head_blob(&name(CHANGES - 1)).unwrap(), b"target");
}

#[test]
fn the_service_says_whether_a_fronts_copy_is_still_last() {
    // A front's RELEASE skips re-recording a racy close only while the
    // copy it froze is the last state the service journaled at the path
    // (design §3.1).
    let env = env();
    let _service = Service::start(&env);
    let a = front(&env, "a");
    let b = front(&env, "b");
    let client = a.recorder.history_client().unwrap();
    assert!(client.supports("still_last"));
    let recorded = a
        .recorder
        .record(Trigger::Close, CALLER, vec![write(&env, "f", "one")])
        .unwrap();
    let [(path, copy)] = recorded.written.as_slice() else {
        panic!("one write: {recorded:?}");
    };
    assert_eq!(path.as_str(), "f");
    assert!(a.recorder.still_last(path, *copy));
    assert!(!a.recorder.still_last(&RepoPath::new("g").unwrap(), *copy));
    // A frozen copy that is no longer pending can still be read while open.
    let pinned = a.recorder.open_copy(*copy).unwrap();
    // Another front's later write of the path: no longer last.
    std::thread::sleep(TICK);
    close(&env, &b.recorder, "f", "two").unwrap();
    assert!(!a.recorder.still_last(path, *copy));
    assert_eq!(pinned.metadata().unwrap().len(), 3);
    // Nor after a removal of a parent's subtree.
    let recorded = a
        .recorder
        .record(Trigger::Close, CALLER, vec![write(&env, "d/f", "x")])
        .unwrap();
    let (path, copy) = recorded.written[0].clone();
    assert!(a.recorder.still_last(&path, copy));
    std::fs::remove_dir_all(env.tree.join("d")).unwrap();
    b.recorder
        .record(
            Trigger::Unlink,
            CALLER,
            vec![Input::Remove {
                path: RepoPath::new("d").unwrap(),
            }],
        )
        .unwrap();
    assert!(!a.recorder.still_last(&path, copy));
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

// ---- listening fronts: the service calls them for live restores (§15) ----

/// Hooks that note every call; `open` paths answer open for writing.
#[derive(Default)]
struct Noting {
    settles: std::sync::Mutex<usize>,
    open: Vec<RepoPath>,
    invalidated: std::sync::Mutex<Vec<RepoPath>>,
}

impl FrontHooks for Noting {
    fn settle(&self) -> Result<(usize, usize), String> {
        *self.settles.lock().unwrap() += 1;
        Ok((0, 2))
    }

    fn open_for_writing(&self, paths: &[RepoPath]) -> Vec<bool> {
        paths.iter().map(|p| self.open.contains(p)).collect()
    }

    fn invalidate(&self, paths: &[RepoPath]) -> Result<(), String> {
        self.invalidated.lock().unwrap().extend_from_slice(paths);
        Ok(())
    }
}

fn listening_client(env: &Env, lease: &FrontLease, name: &str) -> Arc<HistoryClient> {
    let sink = Arc::new(MemorySink::new());
    Arc::new(connect(env, lease, pod(name), "vol", Duration::from_secs(10), sink).unwrap())
}

fn path(p: &str) -> RepoPath {
    RepoPath::new(p).unwrap()
}

#[test]
fn listening_fronts_answer_the_services_calls() {
    let env = env();
    let service = Service::start(&env);
    let fronts = service.server.as_ref().unwrap().listening_fronts();
    let lease_a = FrontLease::acquire(&env.state, "pod=pod-a").unwrap();
    let lease_b = FrontLease::acquire(&env.state, "pod=pod-b").unwrap();
    let hooks_a = Arc::new(Noting {
        open: vec![path("a")],
        ..Noting::default()
    });
    let hooks_b = Arc::new(Noting {
        open: vec![path("c")],
        ..Noting::default()
    });
    let a = listening_client(&env, &lease_a, "pod-a")
        .listen(hooks_a.clone())
        .unwrap();
    let b = listening_client(&env, &lease_b, "pod-b")
        .listen(hooks_b.clone())
        .unwrap();
    assert!(a.wait_listening(Duration::from_secs(5)));
    assert!(b.wait_listening(Duration::from_secs(5)));

    let mut slots = vec![lease_a.slot(), lease_b.slot()];
    slots.sort_unstable();
    assert_eq!(fronts.require_all(Duration::from_secs(5)).unwrap(), slots);
    // Paths still waiting for a repair, over both fronts.
    assert_eq!(fronts.settle().unwrap(), 4);
    assert_eq!(*hooks_a.settles.lock().unwrap(), 1);
    assert_eq!(*hooks_b.settles.lock().unwrap(), 1);
    assert_eq!(
        fronts
            .open_for_writing(&[path("a"), path("b"), path("c")])
            .unwrap(),
        [true, false, true]
    );
    // More paths than one call carries: split, and still in order.
    let many: Vec<RepoPath> = (0..5000).map(|n| path(&format!("d/{n}"))).collect();
    let mut asked = many.clone();
    asked.push(path("a"));
    let open = fronts.open_for_writing(&asked).unwrap();
    assert_eq!(open.len(), 5001);
    assert!(open[..5000].iter().all(|o| !o) && open[5000]);
    fronts.invalidate(&[path("a"), path("x/y")]).unwrap();
    assert_eq!(
        *hooks_a.invalidated.lock().unwrap(),
        [path("a"), path("x/y")]
    );
    assert_eq!(
        *hooks_b.invalidated.lock().unwrap(),
        [path("a"), path("x/y")]
    );
    a.stop();
    b.stop();
}

#[test]
fn a_live_front_that_does_not_listen_holds_live_restores_back() {
    let env = env();
    let service = Service::start(&env);
    let fronts = service.server.as_ref().unwrap().listening_fronts();
    // No fronts: nothing to wait for.
    assert_eq!(
        fronts.require_all(Duration::ZERO).unwrap(),
        Vec::<u16>::new()
    );
    let lease = FrontLease::acquire(&env.state, "pod=old-front").unwrap();
    let started = Instant::now();
    match fronts.require_all(Duration::from_millis(300)) {
        Err(FrontsError::NotListening(slots)) => assert_eq!(slots, [lease.slot()]),
        other => panic!("{other:?}"),
    }
    assert!(started.elapsed() >= Duration::from_millis(300));
    let message = FrontsError::NotListening(vec![lease.slot()]).to_string();
    assert!(message.contains("not listening"), "{message}");

    // It starts listening within the grace: accepted.
    let client = listening_client(&env, &lease, "old-front");
    let listener = std::thread::spawn({
        let client = client.clone();
        move || {
            std::thread::sleep(Duration::from_millis(200));
            client.listen(Arc::new(NoHooks)).unwrap()
        }
    });
    assert_eq!(
        fronts.require_all(Duration::from_secs(5)).unwrap(),
        [lease.slot()]
    );
    let listener = listener.join().unwrap();

    // A front that is gone (its lease released) is no longer required,
    // and its link is dropped.
    listener.stop();
    drop(lease);
    assert_eq!(
        fronts.require_all(Duration::ZERO).unwrap(),
        Vec::<u16>::new()
    );
    assert!(fronts.listening().is_empty());
}

#[test]
fn a_front_whose_listener_stopped_loses_its_link() {
    let env = env();
    let service = Service::start(&env);
    let fronts = service.server.as_ref().unwrap().listening_fronts();
    let lease = FrontLease::acquire(&env.state, "pod=pod-a").unwrap();
    let listener = listening_client(&env, &lease, "pod-a")
        .listen(Arc::new(NoHooks))
        .unwrap();
    assert!(listener.wait_listening(Duration::from_secs(5)));
    fronts.require_all(Duration::from_secs(5)).unwrap();
    listener.stop();
    // The lease is still held: the call fails, and the front is then
    // required but not listening.
    match fronts.settle() {
        Err(FrontsError::Call { slot, pod, .. }) => {
            assert_eq!((slot, pod.as_str()), (lease.slot(), "pod-a"));
        }
        other => panic!("{other:?}"),
    }
    assert!(fronts.listening().is_empty());
    assert!(matches!(
        fronts.require_all(Duration::from_millis(100)),
        Err(FrontsError::NotListening(_))
    ));

    // A front whose answer is not to the call it was asked loses its link
    // too, however good the answer.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut raw = RawClient::new(raw_channel(&runtime, &env));
    runtime
        .block_on(raw.hello(hello_of(
            &front_info(&lease, pod("pod-a")),
            PROTOCOL_VERSION,
        )))
        .unwrap();
    let (answers, outbound) = tokio::sync::mpsc::channel(4);
    let mut calls = runtime
        .block_on(raw.listen(ReceiverStream::new(outbound)))
        .unwrap()
        .into_inner();
    runtime.spawn(async move {
        while let Ok(Some(call)) = calls.message().await {
            let answer = pb::FrontAnswer {
                id: call.id + 1,
                answer: Some(pb::front_answer::Answer::Settled(pb::Settled {
                    recorded: 0,
                    failed: 0,
                })),
            };
            if answers.send(answer).await.is_err() {
                break;
            }
        }
    });
    assert_eq!(
        fronts.require_all(Duration::from_secs(5)).unwrap(),
        [lease.slot()]
    );
    match fronts.settle() {
        Err(FrontsError::Call { message, .. }) => {
            assert!(message.contains("answered call"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(fronts.listening().is_empty());
}

#[test]
fn a_listener_comes_back_after_the_service_restarts() {
    let env = env();
    let mut service = Service::start(&env);
    let lease = FrontLease::acquire(&env.state, "pod=pod-a").unwrap();
    let hooks = Arc::new(Noting::default());
    let listener = listening_client(&env, &lease, "pod-a")
        .listen(hooks.clone())
        .unwrap();
    assert!(listener.wait_listening(Duration::from_secs(5)));
    service
        .server
        .take()
        .unwrap()
        .shutdown(Duration::from_secs(1));
    service.listen(&env);
    let fronts = service.server.as_ref().unwrap().listening_fronts();
    assert_eq!(
        fronts.require_all(Duration::from_secs(10)).unwrap(),
        [lease.slot()]
    );
    fronts.settle().unwrap();
    assert_eq!(*hooks.settles.lock().unwrap(), 1);
    listener.stop();
}

#[test]
fn a_fronts_directory_move_is_one_record_reconciled_by_the_service() {
    // A front journals a directory rename as one move; the service
    // reconciles the tree beneath it before committing it (design §3.1).
    let env = env();
    let service = Service::start(&env);
    let a = front(&env, "pod-a");
    assert!(a.recorder.history_client().unwrap().supports("move"));
    close(&env, &a.recorder, "d/x", "x").unwrap().unwrap();
    close(&env, &a.recorder, "d/e/y", "y").unwrap().unwrap();
    service.committer.commit_pending().unwrap();
    std::fs::write(env.tree.join("d/untracked"), "u").unwrap();
    std::fs::rename(env.tree.join("d"), env.tree.join("m")).unwrap();
    let outcome = a
        .recorder
        .record_move(
            Trigger::Rename,
            CALLER,
            RepoPath::new("d").unwrap(),
            RepoPath::new("m").unwrap(),
        )
        .unwrap();
    assert!(matches!(outcome, volume::MoveOutcome::Journaled(_)));
    // The service tracks the moved paths at once.
    let tracked = service.recorder.tracked_query(&[
        RepoPath::new("m/e/y").unwrap(),
        RepoPath::new("d/x").unwrap(),
    ]);
    assert_eq!(tracked, [true, false]);
    let before = service.commits().len();
    let report = service.committer.commit_pending().unwrap();
    assert_eq!(report.commits, 1);
    assert_eq!(service.commits().len(), before + 1);
    assert_eq!(
        service.commits().last().unwrap().1.as_deref(),
        Some("pod-a")
    );
    assert_eq!(service.head_blob("m/x").unwrap(), b"x");
    assert_eq!(service.head_blob("m/e/y").unwrap(), b"y");
    assert_eq!(service.head_blob("m/untracked").unwrap(), b"u");
    assert_eq!(service.head_blob("d/x"), None);
}
