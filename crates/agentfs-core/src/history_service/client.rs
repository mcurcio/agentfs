//! A front's connection to the history service (design §14, §3.5).
//!
//! The client keeps a pool of synchronous connections, one request in
//! flight on each, so concurrent boundaries of one front reach the service
//! together and share its journal's group commit. Each new connection does
//! its own handshake. A pooled connection the service has closed (it
//! restarted) is noticed before reuse and dropped, so a restart does not
//! fail the next boundary.
//!
//! Every request has a timeout. Nothing is ever resent: a request that may
//! have reached the service fails ([`ClientError::maybe_journaled`]), since
//! resending a record could journal one frozen copy twice. Outages raise
//! [`Event::HistoryServiceUnavailable`] once, at the first failure after a
//! success, and [`Event::HistoryServiceRestored`] at the next success.

use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::Address;
use super::protocol::{
    FrameError, FrontInfo, Hello, PROTOCOL_VERSION, PathStamp, Reply, Request, read_frame,
    write_frame,
};
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

/// The most idle connections kept for reuse.
const MAX_IDLE: usize = 64;

/// How long an idle connection is kept for reuse: well under the service's
/// [`super::IDLE_TIMEOUT`], so a pooled connection is never one the service
/// is closing, and an idle front holds none of the service's threads.
const MAX_IDLE_AGE: Duration = Duration::from_secs(20);

/// How often [`HistoryClient::connect`] retries.
const CONNECT_RETRY: Duration = Duration::from_millis(100);

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
    /// The service serves as many connections as it can. Nothing was sent;
    /// not an outage.
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

/// A front's handle on the history service. Safe to share across threads.
pub struct HistoryClient {
    config: ClientConfig,
    address: String,
    hello: Hello,
    branch: String,
    /// Idle connections and when each went idle.
    idle: Mutex<Vec<(UnixStream, Instant)>>,
    down: AtomicBool,
    /// `Welcome.features` of the service, as of the last connection made.
    features: Mutex<Vec<String>>,
    events: Events,
    telemetry: Telemetry,
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
        let client = Self {
            address: config.address.to_string(),
            config,
            hello: Hello {
                v: PROTOCOL_VERSION,
                volume: volume.to_owned(),
                front,
            },
            branch: branch.to_owned(),
            idle: Mutex::new(Vec::new()),
            down: AtomicBool::new(false),
            features: Mutex::new(Vec::new()),
            events,
            telemetry,
        };
        let deadline = Instant::now() + client.config.connect_wait;
        loop {
            match client.open() {
                Ok(stream) => {
                    client.check_in(stream);
                    return Ok(client);
                }
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
        &self.hello.front
    }

    /// Whether each of `paths` is tracked now (design §4).
    pub fn tracked(&self, paths: &[RepoPath]) -> Result<Vec<bool>, ClientError> {
        let request = Request::Tracked {
            paths: paths.to_vec(),
        };
        match self.call("tracked", &request)? {
            Reply::Tracked { tracked } if tracked.len() == paths.len() => Ok(tracked),
            other => Err(self.unexpected(&other)),
        }
    }

    /// Hands `draft` to the service and waits until it is durable there.
    /// The service decides each change again, skips writes older than the
    /// state it last journaled at their path (by `stamps`, design §14.1),
    /// and stamps the record with this front's origin (or, for a sweep
    /// record, the origin the draft carries, if it can render it).
    pub fn record(&self, draft: Draft, stamps: Vec<PathStamp>) -> Result<Recorded, ClientError> {
        let request = Request::Record {
            draft: Box::new(draft),
            stamps,
        };
        let reply = self.call("record", &request)?;
        if let Err(e) = crate::failpoints::hit("front.after_reply") {
            return Err(ClientError::Lost {
                address: self.address.clone(),
                sent: true,
                error: e.to_string(),
            });
        }
        match reply {
            Reply::Recorded { seq, skipped } => Ok(Recorded {
                seq,
                skipped: skipped
                    .into_iter()
                    .map(|s| Skipped {
                        path: s.path,
                        reason: s.reason.into_reason(),
                    })
                    .collect(),
                written: Vec::new(),
            }),
            other => Err(self.unexpected(&other)),
        }
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
        let request = Request::StillLast {
            path: path.clone(),
            copy,
        };
        match self.call("still_last", &request)? {
            Reply::StillLast { last } => Ok(last),
            other => Err(self.unexpected(&other)),
        }
    }

    /// One round trip with nothing to do, for liveness and benchmarks.
    pub fn ping(&self) -> Result<Duration, ClientError> {
        let start = Instant::now();
        match self.call("ping", &Request::Ping)? {
            Reply::Pong => Ok(start.elapsed()),
            other => Err(self.unexpected(&other)),
        }
    }

    fn unexpected(&self, reply: &Reply) -> ClientError {
        match reply {
            Reply::Error { message } => ClientError::Service {
                address: self.address.clone(),
                message: message.clone(),
            },
            Reply::Refused { reason } => ClientError::Refused {
                address: self.address.clone(),
                reason: reason.clone(),
            },
            Reply::Busy { reason } => ClientError::Busy {
                address: self.address.clone(),
                reason: reason.clone(),
            },
            other => ClientError::Protocol {
                address: self.address.clone(),
                message: format!("unexpected reply {other:?}"),
            },
        }
    }

    /// One request and its reply, with outage alarms and telemetry.
    fn call(&self, name: &'static str, request: &Request) -> Result<Reply, ClientError> {
        let start = Instant::now();
        let result = self.exchange(request);
        match &result {
            Ok(Reply::Error { .. } | Reply::Refused { .. } | Reply::Busy { .. }) => {}
            Ok(_) => {
                self.telemetry.front_round_trip(name, start.elapsed());
                if self.down.swap(false, Ordering::SeqCst) {
                    self.events.emit(Event::HistoryServiceRestored {
                        address: self.address.clone(),
                    });
                }
            }
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

    fn exchange(&self, request: &Request) -> Result<Reply, ClientError> {
        let mut stream = self.check_out()?;
        if let Err(e) = write_frame(&mut stream, request) {
            // A frame cut short is never journaled: the service reads it
            // whole or not at all.
            return Err(ClientError::Lost {
                address: self.address.clone(),
                sent: false,
                error: e.to_string(),
            });
        }
        match read_frame::<Reply>(&mut stream) {
            Ok(reply) => {
                self.check_in(stream);
                Ok(reply)
            }
            Err(e) => Err(self.read_error(e, true)),
        }
    }

    fn read_error(&self, e: FrameError, sent: bool) -> ClientError {
        let address = self.address.clone();
        if e.is_timeout() {
            return ClientError::Timeout {
                address,
                timeout: self.config.timeout,
                sent,
            };
        }
        match e {
            FrameError::Closed | FrameError::Io(_) => ClientError::Lost {
                address,
                sent,
                error: e.to_string(),
            },
            FrameError::TooLarge(_) | FrameError::Malformed(_) => ClientError::Protocol {
                address,
                message: e.to_string(),
            },
        }
    }

    /// An idle connection the service still holds, or a new one.
    fn check_out(&self) -> Result<UnixStream, ClientError> {
        loop {
            let pooled = self
                .idle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop();
            match pooled {
                Some((_, since)) if since.elapsed() >= MAX_IDLE_AGE => continue,
                Some((stream, _)) if local_socket::is_closed(&stream) => continue,
                Some((stream, _)) => return Ok(stream),
                None => return self.open(),
            }
        }
    }

    fn check_in(&self, stream: UnixStream) {
        let mut idle = self
            .idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The oldest are first: drop those idle too long.
        idle.retain(|(_, since)| since.elapsed() < MAX_IDLE_AGE);
        if idle.len() < MAX_IDLE {
            idle.push((stream, Instant::now()));
        }
    }

    /// A new connection, past the handshake.
    pub(super) fn open(&self) -> Result<UnixStream, ClientError> {
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
        let mut stream =
            local_socket::connect(dir, name).map_err(|e| unreachable(e.to_string()))?;
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
        let timeout = Some(self.config.timeout.max(Duration::from_millis(1)));
        stream
            .set_read_timeout(timeout)
            .and_then(|()| stream.set_write_timeout(timeout))
            .map_err(|e| unreachable(e.to_string()))?;
        if let Err(e) = write_frame(&mut stream, &self.hello) {
            // A service at its connection limit answers `Busy` and closes
            // without reading the hello: its reply may be waiting.
            return match read_frame::<Reply>(&mut stream) {
                Ok(reply @ (Reply::Busy { .. } | Reply::Refused { .. })) => {
                    Err(self.unexpected(&reply))
                }
                _ => Err(unreachable(e.to_string())),
            };
        }
        let reply =
            read_frame::<Reply>(&mut stream).map_err(|e| match self.read_error(e, false) {
                // The service went away during the handshake: still nothing
                // sent, so the next try may well work.
                ClientError::Lost { error, .. } => unreachable(error),
                other => other,
            })?;
        match reply {
            Reply::Welcome {
                volume,
                branch,
                features,
                ..
            } => {
                if volume != self.hello.volume || branch != self.branch {
                    return Err(ClientError::Refused {
                        address: self.address.clone(),
                        reason: format!(
                            "it serves volume {volume:?} on {branch}, not {:?} on {}",
                            self.hello.volume, self.branch
                        ),
                    });
                }
                *self
                    .features
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = features;
                Ok(stream)
            }
            other => Err(self.unexpected(&other)),
        }
    }
}

impl std::fmt::Debug for HistoryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryClient")
            .field("address", &self.address)
            .field("slot", &self.hello.front.slot)
            .finish_non_exhaustive()
    }
}
