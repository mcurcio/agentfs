//! The history service's socket (design §14): `agentfs.history.v2.History`
//! and `grpc.health.v1.Health`, served by tonic on the node-local Unix
//! socket.
//!
//! Each connection is one front's. Its first call is `Hello`, which admits
//! the front and binds its identity to the connection for every later call
//! on it, until the connection ends. A front is admitted only if:
//!
//! - its peer's `SO_PEERCRED` uid is root or the service's own
//!   ([`local_socket::allowed`]);
//! - it speaks [`PROTOCOL_VERSION`] and names this volume;
//! - it holds the lease on the slot it names ([`FrontLease`]), so its
//!   frozen copies are its own;
//! - the volume's identity templates render for its pod (design §9), so
//!   its commits can carry its committer;
//! - fewer than [`MAX_CONNECTIONS`] connections are admitted.
//!
//! Concurrent records meet in the journal, whose group commit makes them
//! durable together. A record is answered only once
//! [`Recorder::sequence`] has made it durable. Every `Recorder` call runs
//! in `spawn_blocking`, never on a runtime worker.

use std::collections::{HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::transport::server::Connected;
use tonic::{Request, Response, Status, Streaming};
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

use super::bridge::Bridge;
use super::fronts::{Fronts, Link};
use super::lease::{FrontLease, MAX_SLOT};
use super::wire::{WireError, pb, repo_paths};
use super::{Address, FEATURES, FrontInfo, MAX_MESSAGE, PROTOCOL_VERSION, PathStamp, SERVICE_NAME};
use crate::config::Settings;
use crate::identity::{IdentityRenderer, Templates};
use crate::local_socket::{self, Peer};
use crate::pending::PendingId;
use crate::record::{Draft, Origin, Trigger};
use crate::telemetry::Telemetry;
use crate::volume::{Recorder, StateDir};

/// The most front connections admitted at once: one per front process.
pub const MAX_CONNECTIONS: usize = 1024;

/// How many calls to a listening front wait in its stream.
const LINK_CALLS: usize = 2;

/// Who is on a connection: what tonic puts in each request's extensions.
#[derive(Debug, Clone, Copy)]
struct ConnInfo {
    id: u64,
    peer: Peer,
}

/// The connections open now and the front each admitted.
#[derive(Default)]
struct Registry {
    next_id: AtomicU64,
    conns: Mutex<HashMap<u64, Option<FrontInfo>>>,
}

impl Registry {
    fn conns(&self) -> MutexGuard<'_, HashMap<u64, Option<FrontInfo>>> {
        self.conns.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn open(&self) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.conns().insert(id, None);
        id
    }

    fn close(&self, id: u64) {
        self.conns().remove(&id);
    }

    /// Binds `front` to connection `id`.
    fn admit(&self, id: u64, front: FrontInfo) -> Result<(), Status> {
        let mut conns = self.conns();
        let admitted = conns.values().filter(|f| f.is_some()).count();
        match conns.get_mut(&id) {
            None => Err(Status::cancelled("the connection is closed")),
            Some(None) if admitted >= MAX_CONNECTIONS => Err(Status::resource_exhausted(format!(
                "the history service already serves {MAX_CONNECTIONS} front connections"
            ))),
            Some(slot) => {
                *slot = Some(front);
                Ok(())
            }
        }
    }

    fn front(&self, id: u64) -> Option<FrontInfo> {
        self.conns().get(&id).cloned().flatten()
    }

    /// The distinct fronts admitted now.
    fn fronts(&self) -> usize {
        let conns = self.conns();
        let instances: HashSet<u64> = conns.values().flatten().map(|f| f.instance).collect();
        instances.len()
    }
}

/// A connection, as tonic serves it. Dropped when the connection ends,
/// which releases the identity it was admitted with.
struct Conn {
    io: tokio::net::UnixStream,
    info: ConnInfo,
    registry: Arc<Registry>,
}

impl Conn {
    fn accept(io: tokio::net::UnixStream, registry: &Arc<Registry>) -> Option<Self> {
        let cred = io.peer_cred().ok()?;
        let peer = Peer {
            uid: cred.uid(),
            gid: cred.gid(),
            pid: cred.pid().and_then(|p| u32::try_from(p).ok()).unwrap_or(0),
        };
        Some(Self {
            io,
            info: ConnInfo {
                id: registry.open(),
                peer,
            },
            registry: registry.clone(),
        })
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.registry.close(self.info.id);
    }
}

impl Connected for Conn {
    type ConnectInfo = ConnInfo;

    fn connect_info(&self) -> ConnInfo {
        self.info
    }
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// What a front's `Hello` is checked against.
struct Admission {
    volume: String,
    identity: Templates,
    state: StateDir,
    euid: u32,
}

/// The front `hello` from `peer` names, if the service serves it.
fn admit(admission: &Admission, peer: Peer, hello: pb::HelloRequest) -> Result<FrontInfo, Status> {
    if !local_socket::allowed(peer.uid, admission.euid) {
        return Err(Status::permission_denied(format!(
            "uid {} may not use this history service (root or uid {} only)",
            peer.uid, admission.euid
        )));
    }
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(Status::failed_precondition(format!(
            "protocol version {} is not supported (this history service speaks \
             {PROTOCOL_VERSION}); run the same agentfs version in fronts and service",
            hello.protocol_version
        )));
    }
    let front = FrontInfo::try_from(hello.front.ok_or(WireError::Missing {
        field: "hello.front",
    })?)?;
    if hello.volume != admission.volume {
        return Err(Status::failed_precondition(format!(
            "this history service serves volume {:?}, not {:?}",
            admission.volume, hello.volume
        )));
    }
    if !(1..=MAX_SLOT).contains(&front.slot) {
        return Err(Status::failed_precondition(format!(
            "slot {} is not a front slot",
            front.slot
        )));
    }
    match FrontLease::live(&admission.state) {
        Ok(live) if live.contains(&front.slot) => {}
        Ok(_) => {
            return Err(Status::failed_precondition(format!(
                "slot {} is not leased; a front leases its slot before it connects",
                front.slot
            )));
        }
        Err(e) => {
            return Err(Status::failed_precondition(format!(
                "checking the lease of slot {}: {e}",
                front.slot
            )));
        }
    }
    IdentityRenderer::new(
        admission.identity.clone(),
        &front.origin.context(&admission.volume),
    )
    .map_err(|e| {
        Status::failed_precondition(format!(
            "the volume's identity templates do not render for pod {:?} (design §9): {e}",
            front.origin.pod
        ))
    })?;
    Ok(front)
}

/// What the service's handlers share.
struct Shared {
    recorder: Recorder,
    admission: Admission,
    branch: String,
    registry: Arc<Registry>,
    /// Front instances whose slot was reclaimed when they first connected.
    reclaimed: Mutex<HashSet<u64>>,
    /// Fronts listening for the service's calls (design §15).
    fronts: Fronts,
}

impl Shared {
    /// At a front instance's first connection, removes the frozen copies a
    /// previous holder of its slot left and never got journaled (design
    /// §14). A failure only leaves them for the slot's next holder.
    fn reclaim(&self, front: &FrontInfo) {
        let first = self
            .reclaimed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(front.instance);
        if !first {
            return;
        }
        if let Ok(leased_at) = FrontLease::leased_at(&self.admission.state, front.slot) {
            let _ = self.recorder.reclaim_slot(front.slot, leased_at);
        }
    }

    /// The origin a front's record is journaled with: the front's own, or,
    /// for a sweep record that names another (a repair the front inherited
    /// from a dead predecessor in its slot, design §14.1), that one, if the
    /// volume's identity templates render for it. A record of any other
    /// trigger that names another origin is journaled as the front's own.
    fn record_origin(&self, front: &FrontInfo, draft: &Draft) -> Origin {
        let Some(claimed) = draft.origin.as_ref().filter(|o| **o != front.origin) else {
            return front.origin.clone();
        };
        if draft.trigger != Trigger::Sweep {
            eprintln!(
                "history service: a {} record from {} names origin {}; only sweep records may \
                 (repairs a front inherited): recorded as {}",
                draft.trigger.as_str(),
                front.origin.replica,
                claimed.replica,
                front.origin.replica
            );
            return front.origin.clone();
        }
        let volume = &self.admission.volume;
        match IdentityRenderer::new(self.admission.identity.clone(), &claimed.context(volume)) {
            Ok(_) => claimed.clone(),
            Err(e) => {
                eprintln!(
                    "history service: a repair from {} names origin {}, whose committer does \
                     not render ({e}): recorded as {}",
                    front.origin.replica, claimed.replica, front.origin.replica
                );
                front.origin.clone()
            }
        }
    }
}

/// Runs `work` off the runtime's workers.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Status::aborted(format!("the history service's worker failed: {e}")))
}

/// The `History` service.
struct Svc {
    shared: Arc<Shared>,
}

impl Svc {
    fn conn<T>(request: &Request<T>) -> Result<ConnInfo, Status> {
        request
            .extensions()
            .get::<ConnInfo>()
            .copied()
            .ok_or_else(|| Status::internal("a request without its connection"))
    }

    /// The front admitted on `request`'s connection.
    fn front<T>(&self, request: &Request<T>) -> Result<FrontInfo, Status> {
        self.shared
            .registry
            .front(Self::conn(request)?.id)
            .ok_or_else(|| Status::failed_precondition("send Hello first"))
    }
}

#[tonic::async_trait]
impl pb::history_server::History for Svc {
    async fn hello(
        &self,
        request: Request<pb::HelloRequest>,
    ) -> Result<Response<pb::Welcome>, Status> {
        let conn = Self::conn(&request)?;
        let hello = request.into_inner();
        let shared = self.shared.clone();
        let front = blocking(move || admit(&shared.admission, conn.peer, hello)).await??;
        self.shared.registry.admit(conn.id, front.clone())?;
        let (shared, admitted) = (self.shared.clone(), front.clone());
        blocking(move || shared.reclaim(&admitted)).await?;
        // Its workloads' views of the tree, for nested repositories (design §5).
        crate::nested::add_views(front.views.iter().cloned());
        Ok(Response::new(pb::Welcome {
            protocol_version: PROTOCOL_VERSION,
            volume: self.shared.admission.volume.clone(),
            branch: self.shared.branch.clone(),
            service_pid: std::process::id(),
            features: FEATURES.iter().map(|f| (*f).to_owned()).collect(),
        }))
    }

    async fn tracked(
        &self,
        request: Request<pb::TrackedRequest>,
    ) -> Result<Response<pb::TrackedReply>, Status> {
        self.front(&request)?;
        let paths = repo_paths("tracked.paths", request.into_inner().paths)?;
        let shared = self.shared.clone();
        let tracked = blocking(move || shared.recorder.tracked_query(&paths)).await?;
        Ok(Response::new(pb::TrackedReply { tracked }))
    }

    async fn record(
        &self,
        request: Request<pb::RecordRequest>,
    ) -> Result<Response<pb::RecordReply>, Status> {
        let front = self.front(&request)?;
        let pb::RecordRequest { draft, stamps } = request.into_inner();
        let draft = Draft::try_from(draft.ok_or(WireError::Missing {
            field: "record.draft",
        })?)?;
        let stamps = stamps
            .into_iter()
            .map(PathStamp::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let shared = self.shared.clone();
        let recorded = blocking(move || {
            let origin = shared.record_origin(&front, &draft);
            shared
                .recorder
                .sequence(&origin, front.slot, draft, &stamps)
                .and_then(|recorded| {
                    crate::failpoints::hit("service.after_record")?;
                    Ok(recorded)
                })
        })
        .await?
        // Nothing was journaled: the front may drop its copies.
        .map_err(|e| Status::aborted(e.to_string()))?;
        Ok(Response::new(pb::RecordReply {
            seq: recorded.seq,
            skipped: recorded.skipped.iter().map(Into::into).collect(),
        }))
    }

    async fn still_last(
        &self,
        request: Request<pb::StillLastRequest>,
    ) -> Result<Response<pb::StillLastReply>, Status> {
        self.front(&request)?;
        let pb::StillLastRequest { path, copy } = request.into_inner();
        let mut paths = repo_paths("still_last.path", vec![path])?;
        let path = paths.pop().ok_or(WireError::Missing {
            field: "still_last.path",
        })?;
        let shared = self.shared.clone();
        let last = blocking(move || shared.recorder.last_write_is(&path, PendingId(copy))).await?;
        Ok(Response::new(pb::StillLastReply { last }))
    }

    type ListenStream = ReceiverStream<Result<pb::FrontCall, Status>>;

    async fn listen(
        &self,
        request: Request<Streaming<pb::FrontAnswer>>,
    ) -> Result<Response<Self::ListenStream>, Status> {
        let front = self.front(&request)?;
        let mut inbound = request.into_inner();
        let (calls, outbound) = tokio::sync::mpsc::channel(LINK_CALLS);
        let (answers, answered) = std::sync::mpsc::channel();
        // Feeds the front's answers to the thread calling it; ends with
        // the front's stream, which disconnects the link.
        tokio::spawn(async move {
            while let Ok(Some(answer)) = inbound.message().await {
                if answers.send(answer).is_err() {
                    break;
                }
            }
        });
        // The stream is the front's link from now on: the service calls,
        // the front answers (design §15).
        self.shared.fronts.add(front, Link::new(calls, answered));
        Ok(Response::new(ReceiverStream::new(outbound)))
    }
}

/// The server while it runs.
struct Running {
    bridge: Bridge,
    health: HealthReporter,
    stop: tokio::sync::oneshot::Sender<()>,
    serve: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

/// A running history service socket.
pub struct HistoryServer {
    address: Address,
    dir: OwnedFd,
    name: String,
    shared: Arc<Shared>,
    running: Option<Running>,
    /// Kept alive for the `agentfs.history_service.fronts` gauge.
    _gauge: Arc<dyn Fn() -> usize + Send + Sync>,
}

impl HistoryServer {
    /// Listens at `address` and serves fronts of the volume `recorder`
    /// records for: its local recorder, as [`crate::volume::open`] returns
    /// it, with `settings` and `state` its own. The default address's
    /// directory is made `0700`; another directory is left as it is. The
    /// socket is `0600`. Fails if another service is listening there.
    pub fn start(
        address: &Address,
        recorder: Recorder,
        state: &StateDir,
        settings: &Settings,
        telemetry: &Telemetry,
    ) -> std::io::Result<Self> {
        let Some((dir, name)) = address.unix_parts() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{address} is not a socket path"),
            ));
        };
        if recorder.is_front() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a front cannot serve history",
            ));
        }
        let (dir_fd, listener) = if dir == state.history_dir() {
            local_socket::bind_private(dir, name)?
        } else {
            std::fs::create_dir_all(dir)?;
            local_socket::bind(dir, name)?
        };
        let registry = Arc::new(Registry::default());
        let shared = Arc::new(Shared {
            recorder,
            admission: Admission {
                volume: settings.volume.clone(),
                identity: settings.identity.clone(),
                state: state.clone(),
                euid: rustix::process::geteuid().as_raw(),
            },
            branch: settings.branch_ref.clone(),
            registry: registry.clone(),
            reclaimed: Mutex::new(HashSet::new()),
            fronts: Fronts::new(state, settings.commit_window * 20),
        });
        let gauge: Arc<dyn Fn() -> usize + Send + Sync> = {
            let registry = Arc::downgrade(&registry);
            Arc::new(move || registry.upgrade().map_or(0, |r| r.fronts()))
        };
        telemetry.observe_fronts(Arc::downgrade(&gauge));

        let bridge = Bridge::new("agentfs-history", 2)?;
        let (health, health_service) = tonic_health::server::health_reporter();
        bridge.block_on(health.set_service_status(SERVICE_NAME, ServingStatus::Serving));
        listener.set_nonblocking(true)?;
        let listener = {
            let _runtime = bridge.handle().enter();
            tokio::net::UnixListener::from_std(listener)?
        };
        let incoming = UnixListenerStream::new(listener).filter_map(move |accepted| {
            let conn = Conn::accept(accepted.ok()?, &registry)?;
            Some(Ok::<_, std::io::Error>(conn))
        });
        let history = pb::history_server::HistoryServer::new(Svc {
            shared: shared.clone(),
        })
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serve = bridge.handle().spawn(
            tonic::transport::Server::builder()
                .add_service(health_service)
                .add_service(history)
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = stopped.await;
                }),
        );
        Ok(Self {
            address: address.clone(),
            dir: dir_fd,
            name: name.to_owned(),
            shared,
            running: Some(Running {
                bridge,
                health,
                stop,
                serve,
            }),
            _gauge: gauge,
        })
    }

    pub fn address(&self) -> &Address {
        &self.address
    }

    /// The fronts listening for the service's calls, for restores served
    /// live (design §15).
    pub fn listening_fronts(&self) -> Fronts {
        self.shared.fronts.clone()
    }

    /// The distinct fronts connected now.
    pub fn fronts(&self) -> usize {
        self.shared.registry.fronts()
    }

    /// Reports `NOT_SERVING`, stops accepting and asks every connection to
    /// finish (HTTP/2 `GOAWAY`), ends every listening stream, waits up to
    /// `grace` for the requests in flight, then drops what is left and
    /// removes the socket. Returns whether everything ended in time.
    pub fn shutdown(mut self, grace: Duration) -> bool {
        let Some(running) = self.running.take() else {
            return true;
        };
        let Running {
            bridge,
            health,
            stop,
            serve,
        } = running;
        bridge.block_on(health.set_service_status(SERVICE_NAME, ServingStatus::NotServing));
        let _ = stop.send(());
        self.shared.fronts.close_all();
        let done = bridge.block_on(async { tokio::time::timeout(grace, serve).await.is_ok() });
        // Cancels every connection left; waits for `Recorder` calls in
        // flight, which no cancellation can stop halfway.
        drop(bridge);
        let _ = local_socket::unlink(&self.dir, &self.name);
        done
    }
}

impl Drop for HistoryServer {
    /// A server dropped without [`HistoryServer::shutdown`] (a test, a
    /// panic) stops at once: like a crash, except the socket is removed.
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            self.shared.fronts.close_all();
            drop(running);
            let _ = local_socket::unlink(&self.dir, &self.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Origin;

    fn hello(slot: u32, version: u32) -> pb::HelloRequest {
        pb::HelloRequest {
            protocol_version: version,
            volume: "vol".into(),
            front: Some(pb::FrontInfo {
                origin: Some(
                    (&Origin {
                        namespace: "ns".into(),
                        pod: "p".into(),
                        pod_uid: "u".into(),
                        node: "n".into(),
                        replica: "p".into(),
                    })
                        .into(),
                ),
                slot,
                instance: 1,
                pid: 1,
                version: String::new(),
                views: Vec::new(),
            }),
        }
    }

    #[test]
    fn admission_checks_the_peer_and_the_hello() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        crate::volume::init(&state, "vol", None).unwrap();
        let admission = Admission {
            volume: "vol".into(),
            identity: Templates::default(),
            state: state.clone(),
            euid: 1000,
        };
        let peer = |uid| Peer {
            uid,
            gid: uid,
            pid: 1,
        };
        let lease = FrontLease::acquire(&state, "pod=p").unwrap();
        let slot = u32::from(lease.slot());

        let err = admit(&admission, peer(1001), hello(slot, PROTOCOL_VERSION)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "{err:?}");
        assert!(err.message().contains("uid 1001 may not use"), "{err:?}");
        for uid in [0, 1000] {
            let front = admit(&admission, peer(uid), hello(slot, PROTOCOL_VERSION)).unwrap();
            assert_eq!(u32::from(front.slot), slot);
        }

        let err = admit(&admission, peer(0), hello(slot, 1)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message()
                .contains("protocol version 1 is not supported")
        );

        let err = admit(&admission, peer(0), hello(slot + 1, PROTOCOL_VERSION)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("not leased"), "{err:?}");

        let err = admit(&admission, peer(0), hello(0, PROTOCOL_VERSION)).unwrap_err();
        assert!(err.message().contains("not a front slot"), "{err:?}");

        let err = admit(&admission, peer(0), hello(1 << 20, PROTOCOL_VERSION)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

        let mut other = hello(slot, PROTOCOL_VERSION);
        other.volume = "other".into();
        let err = admit(&admission, peer(0), other).unwrap_err();
        assert!(err.message().contains("serves volume \"vol\""), "{err:?}");

        let mut empty = hello(slot, PROTOCOL_VERSION);
        empty.front = None;
        let err = admit(&admission, peer(0), empty).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    }

    #[test]
    fn the_registry_admits_up_to_its_limit_and_forgets_closed_connections() {
        let registry = Registry::default();
        let front = |instance| FrontInfo {
            origin: Origin {
                namespace: String::new(),
                pod: String::new(),
                pod_uid: String::new(),
                node: String::new(),
                replica: "r".into(),
            },
            slot: 1,
            instance,
            pid: 1,
            version: String::new(),
            views: Vec::new(),
        };
        let ids: Vec<u64> = (0..=MAX_CONNECTIONS).map(|_| registry.open()).collect();
        for (n, id) in ids[..MAX_CONNECTIONS].iter().enumerate() {
            registry.admit(*id, front(n as u64 % 3)).unwrap();
        }
        assert_eq!(registry.fronts(), 3);
        let last = ids[MAX_CONNECTIONS];
        let err = registry.admit(last, front(9)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
        // An admitted connection may say hello again.
        registry.admit(ids[0], front(0)).unwrap();
        registry.close(ids[0]);
        registry.admit(last, front(9)).unwrap();
        assert_eq!(registry.front(last).unwrap().instance, 9);
        assert!(registry.front(ids[0]).is_none());
        assert_eq!(
            registry.admit(ids[0], front(0)).unwrap_err().code(),
            tonic::Code::Cancelled
        );
    }
}
