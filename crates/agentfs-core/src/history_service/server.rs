//! The history service's socket (design §14).
//!
//! One thread accepts, and one thread serves each connection, one request
//! at a time ([`MAX_CONNECTIONS`] at most). Concurrent records from many
//! connections meet in the journal, whose group commit makes them durable
//! together. A connection is served only after:
//!
//! - its peer's `SO_PEERCRED` uid is root or the service's own
//!   ([`local_socket::allowed`]);
//! - its [`Hello`] arrives within [`HELLO_TIMEOUT`], speaks
//!   [`PROTOCOL_VERSION`] and names this volume;
//! - the front holds the lease on the slot it names ([`FrontLease`]), so
//!   its frozen copies are its own;
//! - the volume's identity templates render for its pod (design §9), so
//!   its commits can carry its committer.
//!
//! [`Hello`]: super::protocol::Hello
//! [`PROTOCOL_VERSION`]: super::protocol::PROTOCOL_VERSION

use std::collections::HashMap;
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::Address;
use super::lease::{FrontLease, MAX_SLOT};
use super::protocol::{
    FrontInfo, Reply, Request, WireSkip, WireSkipReason, parse_hello, read_frame, read_frame_bytes,
    write_frame,
};
use crate::config::Settings;
use crate::identity::{IdentityRenderer, Templates};
use crate::local_socket;
use crate::telemetry::Telemetry;
use crate::volume::{Recorder, Skipped, StateDir};

/// The most connections served at once.
pub const MAX_CONNECTIONS: usize = 256;

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
    gone: Condvar,
}

struct Conn {
    /// A handle to shut the connection's read side at shutdown.
    stream: UnixStream,
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

/// A running history service socket.
pub struct HistoryServer {
    address: Address,
    dir: OwnedFd,
    name: String,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
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
            gone: Condvar::new(),
        });
        let gauge: Arc<dyn Fn() -> usize + Send + Sync> = {
            let shared = Arc::downgrade(&shared);
            Arc::new(move || shared.upgrade().map_or(0, |s| s.fronts()))
        };
        telemetry.observe_fronts(Arc::downgrade(&gauge));
        let accept = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("agentfs-history".into())
                .spawn(move || accept_loop(&listener, &shared))?
        };
        Ok(Self {
            address: address.clone(),
            dir: dir_fd,
            name: name.to_owned(),
            shared,
            accept: Some(accept),
            _gauge: gauge,
        })
    }

    pub fn address(&self) -> &Address {
        &self.address
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
        let _ = local_socket::unlink(&self.dir, &self.name);
        idle
    }

    fn stop_accepting(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            // Wakes the accept loop, which then sees `stop`.
            let _ = UnixStream::connect(local_socket::socket_address(&self.dir, &self.name));
            let _ = accept.join();
        }
    }
}

impl Drop for HistoryServer {
    /// A server dropped without [`HistoryServer::shutdown`] (a test, a
    /// panic) stops at once: like a crash, except the socket is removed.
    fn drop(&mut self) {
        if self.accept.is_some() {
            self.stop_accepting();
            for conn in self.shared.conns().values() {
                let _ = conn.stream.shutdown(Shutdown::Both);
            }
            let _ = local_socket::unlink(&self.dir, &self.name);
        }
    }
}

fn accept_loop(listener: &UnixListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(mut stream) = stream else { continue };
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut conns = shared.conns();
            if conns.len() >= MAX_CONNECTIONS {
                drop(conns);
                refuse(
                    &mut stream,
                    format!("the history service already serves {MAX_CONNECTIONS} connections"),
                );
                continue;
            }
            let Ok(handle) = stream.try_clone() else {
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
                serve(&worker, stream, id);
                worker.conns().remove(&id);
                worker.gone.notify_all();
            });
        if spawned.is_err() {
            shared.conns().remove(&id);
            shared.gone.notify_all();
        }
    }
}

fn refuse(stream: &mut UnixStream, reason: String) {
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

fn serve(shared: &Shared, mut stream: UnixStream, id: u64) {
    match local_socket::peer(&stream) {
        Ok(peer) if local_socket::allowed(peer.uid, shared.euid) => {}
        Ok(peer) => {
            return refuse(
                &mut stream,
                format!(
                    "uid {} may not use this history service (root or uid {} only)",
                    peer.uid, shared.euid
                ),
            );
        }
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
    let welcome = Reply::Welcome {
        volume: shared.volume.clone(),
        branch: shared.branch.clone(),
        service_pid: std::process::id(),
    };
    // Counted before the welcome, so a front that has its welcome is
    // counted.
    if let Some(conn) = shared.conns().get_mut(&id) {
        conn.instance = Some(hello.front.instance);
    }
    if write_frame(&mut stream, &welcome).is_err() {
        return;
    }
    // Idle connections wait for their next request as long as the front
    // keeps them.
    let _ = stream.set_read_timeout(None);
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
            Request::Record { draft } => {
                match shared.recorder.sequence(&front.origin, front.slot, *draft) {
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
        };
        if write_frame(&mut stream, &reply).is_err() {
            return;
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
