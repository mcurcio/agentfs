//! The history service's socket (design §14).
//!
//! One thread accepts on each address (a Unix socket on this node, and
//! with `history-tls` a TLS address other nodes reach), and one thread
//! serves each connection, one request at a time ([`MAX_CONNECTIONS`] at
//! most). Concurrent records from many connections meet in the journal,
//! whose group commit makes them durable together. A connection is served
//! only after:
//!
//! - on the Unix socket, its peer's `SO_PEERCRED` uid is root or the
//!   service's own ([`local_socket::allowed`]); over TLS, the handshake
//!   (within [`HELLO_TIMEOUT`]) proved the peer holds the volume's
//!   certificate (design §14.1);
//! - its [`Hello`] arrives within [`HELLO_TIMEOUT`], speaks
//!   [`PROTOCOL_VERSION`] and names this volume;
//! - the front holds the lease on the slot it names ([`FrontLease`]), so
//!   its frozen copies are its own;
//! - the volume's identity templates render for its pod (design §9), so
//!   its commits can carry its committer.
//!
//! [`Hello`]: super::protocol::Hello
//! [`PROTOCOL_VERSION`]: super::protocol::PROTOCOL_VERSION

use std::collections::{HashMap, HashSet};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::Address;
use super::fronts::Fronts;
use super::lease::{FrontLease, MAX_SLOT};
use super::protocol::{
    FEATURES, FrontInfo, Reply, Request, WireSkip, WireSkipReason, parse_hello, read_frame,
    read_frame_bytes, write_frame,
};
use super::transport::{Accepted, Handle, Listener, PeerInfo, Stream};
use crate::config::Settings;
use crate::identity::{IdentityRenderer, Templates};
use crate::local_socket;
use crate::record::{Draft, Origin, Trigger};
use crate::telemetry::Telemetry;
use crate::volume::{Recorder, Skipped, StateDir};

/// The most connections served at once.
/// One thread each; a front holds at most as many as it has boundaries in
/// flight at once (its FUSE threads and its sweep), and none once idle for
/// [`IDLE_TIMEOUT`].
pub const MAX_CONNECTIONS: usize = 1024;

/// How long a connection may wait for its next request before the service
/// closes it. A front drops pooled connections well before this
/// (`client::MAX_IDLE_AGE`), so an idle front holds no service thread.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a new connection has to send its hello.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a reply may take to write before the front is given up on.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the connection threads share.
struct Shared {
    recorder: Recorder,
    state: StateDir,
    volume: String,
    branch: String,
    identity: Templates,
    euid: u32,
    stop: AtomicBool,
    next_id: AtomicU64,
    conns: Mutex<HashMap<u64, Conn>>,
    /// Front instances whose slot was reclaimed when they first connected.
    reclaimed: Mutex<HashSet<u64>>,
    gone: Condvar,
    /// Fronts listening for the service's calls (design §15).
    fronts: Fronts,
}

struct Conn {
    /// A handle to shut the connection's read side at shutdown.
    stream: Handle,
    /// The front's instance, once its hello is accepted.
    instance: Option<u64>,
}

impl Shared {
    fn conns(&self) -> MutexGuard<'_, HashMap<u64, Conn>> {
        self.conns.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fronts(&self) -> usize {
        let conns = self.conns();
        let mut instances: Vec<u64> = conns.values().filter_map(|c| c.instance).collect();
        instances.sort_unstable();
        instances.dedup();
        instances.len()
    }
}

/// One address the service listens on.
struct Bound {
    address: Address,
    /// A Unix socket's directory and name, removed at shutdown.
    unix: Option<(OwnedFd, String)>,
    /// A TCP listener's clone, to wake its accept loop.
    tcp: Option<std::net::TcpListener>,
    accept: Option<JoinHandle<()>>,
}

impl Bound {
    fn stop_accepting(&mut self) {
        if let Some(accept) = self.accept.take() {
            // Wakes the accept loop, which then sees `stop`.
            if let Some((dir, name)) = &self.unix {
                let _ = UnixStream::connect(local_socket::socket_address(dir, name));
            }
            if let Some(tcp) = &self.tcp {
                super::transport::wake_tcp(tcp);
            }
            let _ = accept.join();
        }
    }

    fn remove(&self) {
        if let Some((dir, name)) = &self.unix {
            let _ = local_socket::unlink(dir, name);
        }
    }
}

/// A running history service.
pub struct HistoryServer {
    bound: Vec<Bound>,
    shared: Arc<Shared>,
    /// Kept alive for the `agentfs.history_service.fronts` gauge.
    _gauge: Arc<dyn Fn() -> usize + Send + Sync>,
}

impl HistoryServer {
    /// Listens at each of `addresses` and serves fronts of the volume
    /// `recorder` records for: its local recorder, as
    /// [`crate::volume::open`] returns it, with `settings` and `state` its
    /// own. A Unix socket in the default directory makes it `0700`; another
    /// directory is left as it is; the socket is `0600`. A TLS address
    /// (`history-tls`) with port 0 gets a free port ([`HistoryServer::bound`]
    /// says which), and authenticates with the volume's certificate, which
    /// the caller has made sure of ([`super::tls::VolumeIdentity::ensure`]).
    /// Fails if another service is listening at one of them.
    pub fn start(
        addresses: &[Address],
        recorder: Recorder,
        state: &StateDir,
        settings: &Settings,
        telemetry: &Telemetry,
    ) -> std::io::Result<Self> {
        let invalid = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
        if recorder.is_front() {
            return Err(invalid("a front cannot serve history".into()));
        }
        if addresses.is_empty() {
            return Err(invalid("no address to listen on".into()));
        }
        let mut listeners = Vec::new();
        for address in addresses {
            listeners.push(bind(address, state)?);
        }
        let shared = Arc::new(Shared {
            recorder,
            state: state.clone(),
            volume: settings.volume.clone(),
            branch: settings.branch_ref.clone(),
            identity: settings.identity.clone(),
            euid: rustix::process::geteuid().as_raw(),
            stop: AtomicBool::new(false),
            next_id: AtomicU64::new(0),
            conns: Mutex::new(HashMap::new()),
            reclaimed: Mutex::new(HashSet::new()),
            gone: Condvar::new(),
            fronts: Fronts::new(state, settings.commit_window * 20),
        });
        let gauge: Arc<dyn Fn() -> usize + Send + Sync> = {
            let shared = Arc::downgrade(&shared);
            Arc::new(move || shared.upgrade().map_or(0, |s| s.fronts()))
        };
        telemetry.observe_fronts(Arc::downgrade(&gauge));
        let mut bound = Vec::new();
        for (mut b, listener) in listeners {
            let worker = shared.clone();
            let spawned = std::thread::Builder::new()
                .name("agentfs-history".into())
                .spawn(move || accept_loop(&listener, &worker));
            match spawned {
                Ok(accept) => {
                    b.accept = Some(accept);
                    bound.push(b);
                }
                Err(e) => {
                    let mut server = Self {
                        bound,
                        shared: shared.clone(),
                        _gauge: gauge,
                    };
                    server.stop_accepting();
                    b.remove();
                    return Err(e);
                }
            }
        }
        Ok(Self {
            bound,
            shared,
            _gauge: gauge,
        })
    }

    /// The first address it listens on.
    pub fn address(&self) -> &Address {
        &self.bound[0].address
    }

    /// Every address it listens on, a TLS address with its actual port.
    pub fn bound(&self) -> Vec<Address> {
        self.bound.iter().map(|b| b.address.clone()).collect()
    }

    /// The fronts listening for the service's calls, for restores served
    /// live (design §15).
    pub fn listening_fronts(&self) -> Fronts {
        self.shared.fronts.clone()
    }

    /// The distinct fronts connected now.
    pub fn fronts(&self) -> usize {
        self.shared.fronts()
    }

    /// Stops accepting, lets each connection finish the request it is
    /// serving (up to `grace`), closes them and removes the socket.
    /// Returns whether every connection was done in time.
    pub fn shutdown(mut self, grace: Duration) -> bool {
        self.stop_accepting();
        // Wake every connection waiting for its next request; one in the
        // middle of a request still writes its reply.
        for conn in self.shared.conns().values() {
            let _ = conn.stream.shutdown(Shutdown::Read);
        }
        let deadline = Instant::now() + grace;
        let mut conns = self.shared.conns();
        while !conns.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            conns = self
                .shared
                .gone
                .wait_timeout(conns, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let idle = conns.is_empty();
        for conn in conns.values() {
            let _ = conn.stream.shutdown(Shutdown::Both);
        }
        drop(conns);
        self.shared.fronts.close_all();
        for b in &self.bound {
            b.remove();
        }
        idle
    }

    fn stop_accepting(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for b in &mut self.bound {
            b.stop_accepting();
        }
    }
}

impl Drop for HistoryServer {
    /// A server dropped without [`HistoryServer::shutdown`] (a test, a
    /// panic) stops at once: like a crash, except the socket is removed.
    fn drop(&mut self) {
        if self.bound.iter().any(|b| b.accept.is_some()) {
            self.stop_accepting();
            for conn in self.shared.conns().values() {
                let _ = conn.stream.shutdown(Shutdown::Both);
            }
            self.shared.fronts.close_all();
            for b in &self.bound {
                b.remove();
            }
        }
    }
}

/// Binds one address.
fn bind(address: &Address, state: &StateDir) -> std::io::Result<(Bound, Listener)> {
    match address {
        Address::Unix(_) => {
            let Some((dir, name)) = address.unix_parts() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{address} is not a socket path"),
                ));
            };
            let (dir_fd, listener) = if dir == state.history_dir() {
                local_socket::bind_private(dir, name)?
            } else {
                std::fs::create_dir_all(dir)?;
                local_socket::bind(dir, name)?
            };
            Ok((
                Bound {
                    address: address.clone(),
                    unix: Some((dir_fd, name.to_owned())),
                    tcp: None,
                    accept: None,
                },
                Listener::Unix(listener),
            ))
        }
        #[cfg(feature = "history-tls")]
        Address::Tls(addr) => {
            let identity = super::tls::IdentitySource::load(state)?;
            let listener = std::net::TcpListener::bind(addr)?;
            let actual = listener.local_addr()?;
            Ok((
                Bound {
                    address: Address::Tls(actual),
                    unix: None,
                    tcp: Some(listener.try_clone()?),
                    accept: None,
                },
                Listener::Tls { listener, identity },
            ))
        }
        #[cfg(not(feature = "history-tls"))]
        Address::Tls(_) => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("{address}: this agentfs is built without history-tls (design §14.1)"),
        )),
        Address::State => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a service listens on a socket or TLS address, not `state`",
        )),
    }
}

fn accept_loop(listener: &Listener, shared: &Arc<Shared>) {
    loop {
        let accepted = listener.accept();
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(mut accepted) = accepted else {
            // EMFILE and the like: do not spin.
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut conns = shared.conns();
            if conns.len() >= MAX_CONNECTIONS {
                drop(conns);
                // A TLS connection is dropped before its handshake: the
                // front sees it unreachable, nothing sent, and retries.
                if let Some(stream) = accepted.as_unix() {
                    busy(
                        stream,
                        format!("the history service already serves {MAX_CONNECTIONS} connections"),
                    );
                }
                continue;
            }
            let Ok(handle) = accepted.handle() else {
                continue;
            };
            conns.insert(
                id,
                Conn {
                    stream: handle,
                    instance: None,
                },
            );
        }
        let worker = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("agentfs-front".into())
            .spawn(move || {
                serve(&worker, accepted, id);
                worker.conns().remove(&id);
                worker.gone.notify_all();
            });
        if spawned.is_err() {
            shared.conns().remove(&id);
            shared.gone.notify_all();
        }
    }
}

/// At the connection limit: not a refusal of the front, which may retry.
fn busy(stream: &mut UnixStream, reason: String) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = write_frame(stream, &Reply::Busy { reason });
}

fn refuse(stream: &mut Stream, reason: String) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = write_frame(stream, &Reply::Refused { reason });
}

/// Checks a front's hello; the reason it is refused, if it is.
fn admit(shared: &Shared, front: &FrontInfo, volume: &str) -> Result<(), String> {
    if volume != shared.volume {
        return Err(format!(
            "this history service serves volume {:?}, not {volume:?}",
            shared.volume
        ));
    }
    if !(1..=MAX_SLOT).contains(&front.slot) {
        return Err(format!("slot {} is not a front slot", front.slot));
    }
    match FrontLease::live(&shared.state) {
        Ok(live) if live.contains(&front.slot) => {}
        Ok(_) => {
            return Err(format!(
                "slot {} is not leased; a front leases its slot before it connects",
                front.slot
            ));
        }
        Err(e) => return Err(format!("checking the lease of slot {}: {e}", front.slot)),
    }
    IdentityRenderer::new(
        shared.identity.clone(),
        &front.origin.context(&shared.volume),
    )
    .map_err(|e| {
        format!(
            "the volume's identity templates do not render for pod {:?} (design §9): {e}",
            front.origin.pod
        )
    })?;
    Ok(())
}

/// At a front instance's first connection, removes the frozen copies a
/// previous holder of its slot left and never got journaled (design §14).
/// A failure only leaves them for the slot's next holder.
fn reclaim(shared: &Shared, front: &FrontInfo) {
    let first = shared
        .reclaimed
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(front.instance);
    if !first {
        return;
    }
    if let Ok(leased_at) = FrontLease::leased_at(&shared.state, front.slot) {
        let _ = shared.recorder.reclaim_slot(front.slot, leased_at);
    }
}

fn serve(shared: &Shared, accepted: Accepted, id: u64) {
    // A TLS handshake proves the peer holds the volume's certificate; one
    // that fails or stalls past the hello timeout is closed.
    let Ok(mut stream) = accepted.establish(HELLO_TIMEOUT) else {
        return;
    };
    match stream.peer() {
        Ok(PeerInfo::Local(peer)) if local_socket::allowed(peer.uid, shared.euid) => {}
        Ok(PeerInfo::Local(peer)) => {
            return refuse(
                &mut stream,
                format!(
                    "uid {} may not use this history service (root or uid {} only)",
                    peer.uid, shared.euid
                ),
            );
        }
        Ok(PeerInfo::Remote(_)) => {}
        Err(e) => return refuse(&mut stream, format!("peer credentials: {e}")),
    }
    let _ = stream.set_read_timeout(Some(HELLO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let hello = match read_frame_bytes(&mut stream) {
        Ok(body) => match parse_hello(&body) {
            Ok(hello) => hello,
            Err(reason) => return refuse(&mut stream, reason),
        },
        Err(_) => return,
    };
    if let Err(reason) = admit(shared, &hello.front, &hello.volume) {
        return refuse(&mut stream, reason);
    }
    reclaim(shared, &hello.front);
    // Its workloads' views of the tree, for nested repositories (design §5).
    crate::nested::add_views(hello.front.views.iter().cloned());
    let welcome = Reply::Welcome {
        volume: shared.volume.clone(),
        branch: shared.branch.clone(),
        service_pid: std::process::id(),
        features: FEATURES.iter().map(|f| (*f).to_owned()).collect(),
    };
    // Counted before the welcome, so a front that has its welcome is
    // counted.
    if let Some(conn) = shared.conns().get_mut(&id) {
        conn.instance = Some(hello.front.instance);
    }
    if write_frame(&mut stream, &welcome).is_err() {
        return;
    }
    // An idle connection waits for its next request up to IDLE_TIMEOUT;
    // a front stops using it well before.
    let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
    let front = hello.front;
    loop {
        let request = match read_frame::<Request>(&mut stream) {
            Ok(request) => request,
            Err(_) => return,
        };
        let reply = match request {
            Request::Tracked { paths } => Reply::Tracked {
                tracked: shared.recorder.tracked_query(&paths),
            },
            Request::Record { draft, mut stamps } => {
                let origin = record_origin(shared, &front, &draft);
                // One filesystem, numbered by each client (NFS): compared
                // as the tree's (design §14.1). An older front, without
                // `tree_dev`, is on this node and numbers it as we do.
                let tree_dev = front.tree_dev.or(shared.recorder.tree_dev());
                for s in &mut stamps {
                    s.stamp = s.stamp.normalised(tree_dev);
                }
                match shared
                    .recorder
                    .sequence(&origin, front.slot, *draft, &stamps)
                    .and_then(|recorded| {
                        crate::failpoints::hit("service.after_record")?;
                        Ok(recorded)
                    }) {
                    Ok(recorded) => Reply::Recorded {
                        seq: recorded.seq,
                        skipped: recorded.skipped.into_iter().map(wire_skip).collect(),
                    },
                    Err(e) => Reply::Error {
                        message: e.to_string(),
                    },
                }
            }
            Request::Ping => Reply::Pong,
            Request::StillLast { path, copy } => Reply::StillLast {
                last: shared.recorder.last_write_is(&path, copy),
            },
            Request::Listen => {
                // The connection is the front's link from now on: the
                // service calls, the front answers (design §15).
                if write_frame(&mut stream, &Reply::Listening).is_err() {
                    return;
                }
                shared.fronts.add(front, stream);
                return;
            }
        };
        if write_frame(&mut stream, &reply).is_err() {
            return;
        }
    }
}

/// The origin a front's record is journaled with: the front's own, or,
/// for a sweep record that names another (a repair the front inherited
/// from a dead predecessor in its slot, design §14.1), that one, if the
/// volume's identity templates render for it. A record of any other
/// trigger that names another origin is journaled as the front's own.
fn record_origin(shared: &Shared, front: &FrontInfo, draft: &Draft) -> Origin {
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
    match IdentityRenderer::new(shared.identity.clone(), &claimed.context(&shared.volume)) {
        Ok(_) => claimed.clone(),
        Err(e) => {
            eprintln!(
                "history service: a repair from {} names origin {}, whose committer does not \
                 render ({e}): recorded as {}",
                front.origin.replica, claimed.replica, front.origin.replica
            );
            front.origin.clone()
        }
    }
}

fn wire_skip(skipped: Skipped) -> WireSkip {
    // `Recorder::sequence` only skips for the rules and group refusals,
    // which all have a wire form; anything else still reaches the front
    // as a refusal with its message.
    let reason = WireSkipReason::from_reason(&skipped.reason).unwrap_or_else(|| {
        WireSkipReason::GroupMemberSkipped {
            reason: skipped.reason.to_string(),
        }
    });
    WireSkip {
        path: skipped.path,
        reason,
    }
}
