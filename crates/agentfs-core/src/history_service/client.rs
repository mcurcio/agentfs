//! A front's connection to the history service (design §14, §3.5).
//!
//! One HTTP/2 connection per front, multiplexed: concurrent boundaries of
//! the front reach the service together and share its journal's group
//! commit, and the listening stream and the health probe ride on it too.
//! Each connection starts with `Hello`, which the service admits and binds
//! to the connection. Calls run on the client's [`Bridge`] from the
//! calling thread.
//!
//! Every call has a deadline (`grpc-timeout`, and the client's own timer).
//! Nothing is ever resent: a request that may have reached the service
//! fails ([`ClientError::maybe_journaled`]), since resending a record could
//! journal one frozen copy twice. The one exception is a request hyper
//! hands back unsent because its connection had closed (the service
//! restarted): it is sent once on a new connection, within the same
//! deadline, so a restart does not fail the next boundary. Outages raise
//! [`Event::HistoryServiceUnavailable`] once, at the first failure after a
//! success, and [`Event::HistoryServiceRestored`] at the next success.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper::client::conn::http2::SendRequest;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio_stream::wrappers::ReceiverStream;
use tonic::body::Body;
use tonic::codegen::{Service, http};
use tonic::{Code, Status};
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;

use super::bridge::Bridge;
use super::wire::pb;
use super::wire::pb::history_client::HistoryClient as Grpc;
use super::{Address, FrontInfo, MAX_MESSAGE, PROTOCOL_VERSION, PathStamp, SERVICE_NAME};
use crate::events::{Event, Events};
use crate::local_socket;
use crate::paths::RepoPath;
use crate::pending::PendingId;
use crate::record::Draft;
use crate::telemetry::Telemetry;
use crate::volume::{Recorded, Skipped};

/// How to reach the service.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub address: Address,
    /// How long a request may wait for its reply (`--history-timeout`).
    pub timeout: Duration,
    /// How long [`HistoryClient::connect`] keeps trying to reach a service
    /// that is not up yet (`--history-wait`).
    pub connect_wait: Duration,
}

/// How often [`HistoryClient::connect`] retries.
const CONNECT_RETRY: Duration = Duration::from_millis(100);

/// The `:authority` of every request; the socket is what is reached.
const ORIGIN: &str = "http://agentfs.history";

/// How many answers a listening stream buffers.
const ANSWERS: usize = 4;

/// Why a request got no answer.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No connection: no socket, nobody listening, or the handshake failed
    /// on the way. Nothing was sent.
    #[error("history service {address} is unreachable: {error}")]
    Unreachable { address: String, error: String },
    /// No reply within the timeout. If `sent`, the service may still
    /// journal the request.
    #[error("history service {address} did not answer within {timeout:?}")]
    Timeout {
        address: String,
        timeout: Duration,
        sent: bool,
    },
    /// The connection broke. If `sent`, the service may have journaled the
    /// request before it went.
    #[error("lost the connection to history service {address}: {error}")]
    Lost {
        address: String,
        sent: bool,
        error: String,
    },
    /// The service will not serve this front (another volume, another
    /// version, an identity it cannot render).
    #[error("history service {address} refused this front: {reason}")]
    Refused { address: String, reason: String },
    /// The service serves as many fronts as it can. Nothing was sent; not
    /// an outage.
    #[error("history service {address} is busy: {reason}")]
    Busy { address: String, reason: String },
    /// The service answered that the request failed; nothing was journaled.
    #[error("history service {address} could not record: {message}")]
    Service { address: String, message: String },
    /// The service answered something this front does not understand. The
    /// request may have been journaled.
    #[error("history service {address} broke the protocol: {message}")]
    Protocol { address: String, message: String },
}

impl ClientError {
    /// Whether the service may have journaled the request: if so, its
    /// frozen copies must stay for the service to commit.
    pub fn maybe_journaled(&self) -> bool {
        match self {
            Self::Timeout { sent, .. } | Self::Lost { sent, .. } => *sent,
            Self::Protocol { .. } => true,
            Self::Unreachable { .. }
            | Self::Refused { .. }
            | Self::Busy { .. }
            | Self::Service { .. } => false,
        }
    }

    /// The `stage` of `agentfs.front.service_failures`.
    fn stage(&self) -> &'static str {
        match self {
            Self::Unreachable { .. } => "connect",
            Self::Timeout { .. } => "timeout",
            Self::Lost { .. } | Self::Protocol { .. } => "request",
            Self::Refused { .. } => "refused",
            Self::Busy { .. } => "busy",
            Self::Service { .. } => "service",
        }
    }

    /// Whether this says the service is not doing its job, as opposed to
    /// answering that one request failed.
    fn is_outage(&self) -> bool {
        !matches!(self, Self::Service { .. } | Self::Busy { .. })
    }
}

/// Whether hyper handed a request to its connection: the one fact that
/// decides whether a failed request may go again.
#[derive(Debug, thiserror::Error)]
enum Dispatch {
    /// Handed back unsent: the connection had closed.
    #[error("not sent: {0}")]
    NotSent(#[source] hyper::Error),
    #[error("{0}")]
    Sent(#[source] hyper::Error),
}

/// The tower service tonic calls through: hyper's HTTP/2 sender, telling
/// [`Dispatch::NotSent`] from [`Dispatch::Sent`].
#[derive(Clone)]
struct Wire {
    send: SendRequest<Body>,
}

impl Service<http::Request<Body>> for Wire {
    type Response = http::Response<hyper::body::Incoming>;
    type Error = Dispatch;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Dispatch>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Dispatch>> {
        // A closed connection hands the request back from the call.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let sent = self.send.try_send_request(request);
        Box::pin(async move {
            sent.await.map_err(|mut e| {
                if e.take_message().is_some() {
                    Dispatch::NotSent(e.into_error())
                } else {
                    Dispatch::Sent(e.into_error())
                }
            })
        })
    }
}

/// The [`Dispatch`] behind a failed call, if a transport error failed it.
fn dispatch_of(status: &Status) -> Option<&Dispatch> {
    let mut source = std::error::Error::source(status);
    while let Some(e) = source {
        if let Some(dispatch) = e.downcast_ref::<Dispatch>() {
            return Some(dispatch);
        }
        source = e.source();
    }
    None
}

/// One admitted connection.
#[derive(Clone)]
struct Conn {
    id: u64,
    wire: Wire,
}

/// A front's handle on the history service. Safe to share across threads.
pub struct HistoryClient {
    config: ClientConfig,
    address: String,
    volume: String,
    branch: String,
    front: FrontInfo,
    /// The connection in use; a new one is opened when it is found closed.
    conn: tokio::sync::Mutex<Option<Conn>>,
    next_conn: AtomicU64,
    down: AtomicBool,
    /// `Welcome.features` of the service, as of the last connection made.
    features: Mutex<Vec<String>>,
    events: Events,
    telemetry: Telemetry,
    bridge: Bridge,
}

impl HistoryClient {
    /// Connects to the service at `config.address` as `front` of `volume`
    /// on `branch`, retrying for `config.connect_wait` while the service is
    /// not up. Fails at once if the service refuses the front or serves
    /// another volume or branch.
    pub fn connect(
        config: ClientConfig,
        volume: &str,
        branch: &str,
        front: FrontInfo,
        events: Events,
        telemetry: Telemetry,
    ) -> Result<Self, ClientError> {
        let address = config.address.to_string();
        let bridge = Bridge::new("agentfs-grpc", 1).map_err(|e| ClientError::Unreachable {
            address: address.clone(),
            error: format!("starting the client's runtime: {e}"),
        })?;
        let client = Self {
            address,
            config,
            volume: volume.to_owned(),
            branch: branch.to_owned(),
            front,
            conn: tokio::sync::Mutex::new(None),
            next_conn: AtomicU64::new(1),
            down: AtomicBool::new(false),
            features: Mutex::new(Vec::new()),
            events,
            telemetry,
            bridge,
        };
        let deadline = Instant::now() + client.config.connect_wait;
        loop {
            let opened = client.bridge.block_on(async {
                let deadline = tokio::time::Instant::now() + client.timeout();
                client.connection_until(deadline, None).await
            });
            match opened {
                Ok(_) => return Ok(client),
                Err(
                    e @ (ClientError::Unreachable { .. }
                    | ClientError::Timeout { .. }
                    | ClientError::Busy { .. }),
                ) if Instant::now() < deadline => {
                    drop(e);
                    std::thread::sleep(CONNECT_RETRY);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The service's address.
    pub fn address(&self) -> &Address {
        &self.config.address
    }

    /// The front this client speaks for.
    pub fn front(&self) -> &FrontInfo {
        &self.front
    }

    /// Whether each of `paths` is tracked now (design §4).
    pub fn tracked(&self, paths: &[RepoPath]) -> Result<Vec<bool>, ClientError> {
        let request = pb::TrackedRequest {
            paths: paths.iter().map(|p| p.as_str().to_owned()).collect(),
        };
        let reply = self.call(
            "tracked",
            |mut grpc, request| async move { grpc.tracked(request).await },
            request,
        )?;
        if reply.tracked.len() != paths.len() {
            return Err(self.protocol(format!(
                "{} answers for {} paths",
                reply.tracked.len(),
                paths.len()
            )));
        }
        Ok(reply.tracked)
    }

    /// Hands `draft` to the service and waits until it is durable there.
    /// The service decides each change again, skips writes older than the
    /// state it last journaled at their path (by `stamps`, design §14.1),
    /// and stamps the record with this front's origin (or, for a sweep
    /// record, the origin the draft carries, if it can render it).
    pub fn record(&self, draft: Draft, stamps: Vec<PathStamp>) -> Result<Recorded, ClientError> {
        let request = pb::RecordRequest {
            draft: Some((&draft).into()),
            stamps: stamps.iter().map(Into::into).collect(),
        };
        drop((draft, stamps));
        let reply = self.call(
            "record",
            |mut grpc, request| async move { grpc.record(request).await },
            request,
        )?;
        if let Err(e) = crate::failpoints::hit("front.after_reply") {
            return Err(ClientError::Lost {
                address: self.address.clone(),
                sent: true,
                error: e.to_string(),
            });
        }
        Ok(Recorded {
            seq: reply.seq,
            skipped: reply
                .skipped
                .into_iter()
                .map(Skipped::try_from)
                .collect::<Result<_, _>>()
                .map_err(|e| self.protocol(e.to_string()))?,
            written: Vec::new(),
        })
    }

    /// Whether the service supports `feature` (`Welcome.features`), as of
    /// the last connection this client made.
    pub fn supports(&self, feature: &str) -> bool {
        self.features
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|f| f == feature)
    }

    /// Whether the last change the service journaled at `path` is still
    /// the write of this front's frozen copy `copy` (design §3.1). False,
    /// without asking, from a service without the `still_last` feature.
    pub fn still_last(&self, path: &RepoPath, copy: PendingId) -> Result<bool, ClientError> {
        if !self.supports("still_last") {
            return Ok(false);
        }
        let request = pb::StillLastRequest {
            path: path.as_str().to_owned(),
            copy: copy.0,
        };
        let reply = self.call(
            "still_last",
            |mut grpc, request| async move { grpc.still_last(request).await },
            request,
        )?;
        Ok(reply.last)
    }

    /// One `grpc.health.v1` Check of [`SERVICE_NAME`]: liveness and the
    /// round-trip time. A service that is not serving is unreachable.
    pub fn ping(&self) -> Result<Duration, ClientError> {
        let start = Instant::now();
        let result = self.bridge.block_on(self.unary(
            |wire, request| async move {
                HealthClient::with_origin(wire, origin())
                    .check(request)
                    .await
            },
            HealthCheckRequest {
                service: SERVICE_NAME.to_owned(),
            },
        ));
        let result = result.and_then(|reply| {
            if reply.status == ServingStatus::Serving as i32 {
                Ok(start.elapsed())
            } else {
                Err(ClientError::Unreachable {
                    address: self.address.clone(),
                    error: format!(
                        "it is not serving ({})",
                        ServingStatus::try_from(reply.status)
                            .map_or("an unknown status", |s| s.as_str_name())
                    ),
                })
            }
        });
        self.observe("ping", start, result)
    }

    /// Opens the listening stream on the current connection (design §15):
    /// the sender of the front's answers and the service's calls. Returns
    /// once the service has answered with its headers: it is listening.
    pub(super) async fn open_listen(&self) -> Result<Listening, ClientError> {
        let deadline = tokio::time::Instant::now() + self.timeout();
        let mut stale = None;
        loop {
            let conn = self.connection_until(deadline, stale).await?;
            let (answers, outbound) = tokio::sync::mpsc::channel(ANSWERS);
            let mut grpc = self.grpc(&conn);
            // No grpc-timeout: the stream lasts as long as the front.
            let request = tonic::Request::new(ReceiverStream::new(outbound));
            match tokio::time::timeout_at(deadline, grpc.listen(request)).await {
                Err(_) => return Err(self.timed_out(true)),
                Ok(Ok(calls)) => {
                    return Ok(Listening {
                        answers,
                        calls: calls.into_inner(),
                    });
                }
                Ok(Err(status)) => match self.request_error(&status, stale.is_none()) {
                    Retry::Fresh => stale = Some(conn.id),
                    Retry::No(e) => return Err(e),
                },
            }
        }
    }

    /// The runtime the listener drives its stream on.
    pub(super) fn bridge(&self) -> &Bridge {
        &self.bridge
    }

    /// One request and its reply, with outage alarms and telemetry.
    fn call<Req, Rep, F, Fut>(
        &self,
        name: &'static str,
        rpc: F,
        request: Req,
    ) -> Result<Rep, ClientError>
    where
        Req: Clone,
        F: Fn(Grpc<Wire>, tonic::Request<Req>) -> Fut,
        Fut: Future<Output = Result<tonic::Response<Rep>, Status>>,
    {
        let start = Instant::now();
        let result = self
            .bridge
            .block_on(self.unary(|wire, request| rpc(self.grpc_on(wire), request), request));
        self.observe(name, start, result)
    }

    /// Notes the outcome of a request: its round trip, an outage beginning
    /// or ending.
    fn observe<T>(
        &self,
        name: &'static str,
        start: Instant,
        result: Result<T, ClientError>,
    ) -> Result<T, ClientError> {
        match &result {
            Ok(_) => {
                self.telemetry.front_round_trip(name, start.elapsed());
                if self.down.swap(false, Ordering::SeqCst) {
                    self.events.emit(Event::HistoryServiceRestored {
                        address: self.address.clone(),
                    });
                }
            }
            // The service answered: it is up, and this request failed.
            Err(ClientError::Service { .. }) => {}
            Err(e) => {
                self.telemetry.front_service_failure(e.stage());
                if e.is_outage() && !self.down.swap(true, Ordering::SeqCst) {
                    self.events.emit(Event::HistoryServiceUnavailable {
                        address: self.address.clone(),
                        error: e.to_string(),
                    });
                }
            }
        }
        result
    }

    /// A unary call under the deadline, on the current connection; once
    /// more on a new one if hyper handed it back unsent.
    async fn unary<Req, Rep, F, Fut>(&self, rpc: F, message: Req) -> Result<Rep, ClientError>
    where
        Req: Clone,
        F: Fn(Wire, tonic::Request<Req>) -> Fut,
        Fut: Future<Output = Result<tonic::Response<Rep>, Status>>,
    {
        let deadline = tokio::time::Instant::now() + self.timeout();
        let mut stale = None;
        loop {
            let conn = self.connection_until(deadline, stale).await?;
            let mut request = tonic::Request::new(message.clone());
            request.set_timeout(self.timeout());
            match tokio::time::timeout_at(deadline, rpc(conn.wire.clone(), request)).await {
                Err(_) => return Err(self.timed_out(true)),
                Ok(Ok(reply)) => return Ok(reply.into_inner()),
                Ok(Err(status)) => match self.request_error(&status, stale.is_none()) {
                    Retry::Fresh => stale = Some(conn.id),
                    Retry::No(e) => return Err(e),
                },
            }
        }
    }

    /// The current connection, within `deadline`. Nothing is sent yet.
    async fn connection_until(
        &self,
        deadline: tokio::time::Instant,
        stale: Option<u64>,
    ) -> Result<Conn, ClientError> {
        tokio::time::timeout_at(deadline, self.connection(stale))
            .await
            .unwrap_or_else(|_| Err(self.timed_out(false)))
    }

    /// The connection in use, or a new one if there is none or it is
    /// `stale` (found closed).
    async fn connection(&self, stale: Option<u64>) -> Result<Conn, ClientError> {
        let mut current = self.conn.lock().await;
        if let Some(conn) = current.as_ref()
            && Some(conn.id) != stale
        {
            return Ok(conn.clone());
        }
        *current = None;
        let conn = self.open().await?;
        *current = Some(conn.clone());
        Ok(conn)
    }

    /// A new connection, admitted by the service's `Hello`.
    async fn open(&self) -> Result<Conn, ClientError> {
        let unreachable = |error: String| ClientError::Unreachable {
            address: self.address.clone(),
            error,
        };
        let Address::Unix(path) = &self.config.address;
        let (dir, name) = self
            .config
            .address
            .unix_parts()
            .ok_or_else(|| unreachable(format!("{} is not a socket path", path.display())))?;
        let stream = local_socket::connect(dir, name).map_err(|e| unreachable(e.to_string()))?;
        // Only a service running as root or as this front's own user may
        // acknowledge records: anyone else listening there (a squatter on
        // a shared directory) could answer for records never journaled.
        let euid = rustix::process::geteuid().as_raw();
        match local_socket::peer(&stream) {
            Ok(peer) if local_socket::allowed(peer.uid, euid) => {}
            Ok(peer) => {
                return Err(ClientError::Refused {
                    address: self.address.clone(),
                    reason: format!(
                        "the socket is served by uid {}, not root or this front's uid {euid}; \
                         refusing to trust it",
                        peer.uid
                    ),
                });
            }
            Err(e) => return Err(unreachable(format!("peer credentials: {e}"))),
        }
        let stream = stream
            .set_nonblocking(true)
            .and_then(|()| tokio::net::UnixStream::from_std(stream))
            .map_err(|e| unreachable(e.to_string()))?;
        let (send, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake(TokioIo::new(stream))
            .await
            .map_err(|e| unreachable(format!("HTTP/2 handshake: {e}")))?;
        // The connection's task holds the socket, and nothing else.
        self.bridge.handle().spawn(async move {
            let _ = connection.await;
        });
        let conn = Conn {
            id: self.next_conn.fetch_add(1, Ordering::Relaxed),
            wire: Wire { send },
        };
        let mut hello = tonic::Request::new(pb::HelloRequest {
            protocol_version: PROTOCOL_VERSION,
            volume: self.volume.clone(),
            front: Some((&self.front).into()),
        });
        hello.set_timeout(self.timeout());
        let welcome = match self.grpc(&conn).hello(hello).await {
            Ok(welcome) => welcome.into_inner(),
            Err(status) => return Err(self.hello_error(&status)),
        };
        if welcome.protocol_version != PROTOCOL_VERSION {
            return Err(ClientError::Refused {
                address: self.address.clone(),
                reason: format!(
                    "it speaks protocol version {}, this front {PROTOCOL_VERSION}; run the same \
                     agentfs version in fronts and service",
                    welcome.protocol_version
                ),
            });
        }
        if welcome.volume != self.volume || welcome.branch != self.branch {
            return Err(ClientError::Refused {
                address: self.address.clone(),
                reason: format!(
                    "it serves volume {:?} on {}, not {:?} on {}",
                    welcome.volume, welcome.branch, self.volume, self.branch
                ),
            });
        }
        *self
            .features
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = welcome.features;
        Ok(conn)
    }

    fn grpc(&self, conn: &Conn) -> Grpc<Wire> {
        self.grpc_on(conn.wire.clone())
    }

    fn grpc_on(&self, wire: Wire) -> Grpc<Wire> {
        Grpc::with_origin(wire, origin())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
    }

    fn timeout(&self) -> Duration {
        self.config.timeout.max(Duration::from_millis(1))
    }

    fn timed_out(&self, sent: bool) -> ClientError {
        ClientError::Timeout {
            address: self.address.clone(),
            timeout: self.config.timeout,
            sent,
        }
    }

    fn protocol(&self, message: String) -> ClientError {
        ClientError::Protocol {
            address: self.address.clone(),
            message,
        }
    }

    /// Why `Hello` failed. Nothing was journaled, whatever happened.
    fn hello_error(&self, status: &Status) -> ClientError {
        let address = self.address.clone();
        let reason = status.message().to_owned();
        if let Some(dispatch) = dispatch_of(status) {
            return ClientError::Unreachable {
                address,
                error: dispatch.to_string(),
            };
        }
        match status.code() {
            Code::PermissionDenied | Code::FailedPrecondition | Code::InvalidArgument => {
                ClientError::Refused { address, reason }
            }
            Code::ResourceExhausted => ClientError::Busy { address, reason },
            Code::DeadlineExceeded | Code::Cancelled => self.timed_out(false),
            _ => ClientError::Unreachable {
                address,
                error: status.to_string(),
            },
        }
    }

    /// What a failed request means, and whether it goes once more on a
    /// new connection: only on the `first` attempt, and only if hyper
    /// handed it back unsent.
    fn request_error(&self, status: &Status, first: bool) -> Retry {
        let address = self.address.clone();
        let message = status.message().to_owned();
        Retry::No(match dispatch_of(status) {
            Some(Dispatch::NotSent(_)) if first => return Retry::Fresh,
            Some(Dispatch::NotSent(e)) => ClientError::Unreachable {
                address,
                error: e.to_string(),
            },
            Some(Dispatch::Sent(e)) => ClientError::Lost {
                address,
                sent: true,
                error: e.to_string(),
            },
            None => match status.code() {
                // The service failed it, or rejected it before journaling.
                Code::Aborted | Code::InvalidArgument => ClientError::Service { address, message },
                // "send Hello first".
                Code::FailedPrecondition => ClientError::Protocol { address, message },
                Code::DeadlineExceeded | Code::Cancelled => self.timed_out(true),
                _ => ClientError::Lost {
                    address,
                    sent: true,
                    error: status.to_string(),
                },
            },
        })
    }
}

/// An open listening stream (design §15).
pub(super) struct Listening {
    /// The front's answers, in the request stream.
    pub(super) answers: tokio::sync::mpsc::Sender<pb::FrontAnswer>,
    /// The service's calls, in the response stream.
    pub(super) calls: tonic::Streaming<pb::FrontCall>,
}

/// Whether a failed request goes once more.
enum Retry {
    /// It was never sent: once more, on a new connection.
    Fresh,
    No(ClientError),
}

fn origin() -> http::Uri {
    http::Uri::from_static(ORIGIN)
}

impl std::fmt::Debug for HistoryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryClient")
            .field("address", &self.address)
            .field("slot", &self.front.slot)
            .finish_non_exhaustive()
    }
}
