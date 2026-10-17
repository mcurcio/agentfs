//! The control socket of a running daemon (design §2.2, §2.3, §15).
//!
//! `agentfs mount`, `agentfs run` and `agentfs serve-history` listen on
//! `<state>/meta/control/agentfs.sock` while they hold the state-area
//! lock, so that `agentfs push` can ask them to commit and push now, and
//! `agentfs restore` and `agentfs revert` can ask a mount to write through
//! its own mount (`run` and `serve-history` answer those `busy`). The
//! directory is `0700` and the socket `0600`, and each connection's peer
//! credentials (`SO_PEERCRED`) must be root or the daemon's own user.
//!
//! Both ends address the socket as `/proc/self/fd/<dir fd>/agentfs.sock`,
//! so a long state-area path never hits the 108-byte `sun_path` limit.
//!
//! **Protocol v1**, JSON lines, one request per connection:
//!
//! - request: `{"v":1,"op":"restore"|"revert","path":...,"commit":...,
//!   "force":false,"dry_run":false}` (`path` for restore only), or
//!   `{"v":1,"op":"push","remotes":[...],"timeout_ms":N}` (both
//!   optional), or `{"v":1,"op":"receive_fork","path":...}` (a finished
//!   fork's state area, F8 S3), at most [`MAX_REQUEST`] bytes, read
//!   within [`READ_TIMEOUT`];
//! - reply: `{"ok":{...}}` (a restore or revert), `{"pushed":{...}}` (a
//!   push), `{"received":{"change":...}}` (a fork received into this
//!   local-only volume's history),
//!   `{"refused":{"kind":...,"message":...,"hint":...}}`,
//!   `{"busy":"..."}` (another restore is running, or this daemon serves
//!   no restores) or `{"error":"..."}`.
//!
//! One restore or revert runs at a time; a second gets `busy`. Pushes run
//! beside them and each other. `push` was added to v1 with optional
//! fields only: a daemon from before it answers `{"error":"malformed
//! request: unknown variant ..."}`, and `agentfs push` then falls back to
//! pushing directly (which the lock refuses with exit 75).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use agentfs_core::local_socket;
use agentfs_core::volume::CONTROL_SOCKET;

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;
/// The longest request line accepted.
pub const MAX_REQUEST: usize = 64 << 10;
/// How long the daemon waits for a request line.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Restore,
    Revert,
    /// Commit and push now (design §2.3): every daemon serves it.
    Push,
    /// Receive a finished fork into this local-only volume's history (F8
    /// S3): every daemon serves it, beside the others.
    ReceiveFork,
}

impl Op {
    /// Restores and reverts write through the mount: one at a time.
    fn exclusive(self) -> bool {
        matches!(self, Self::Restore | Self::Revert)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub op: Op,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Restore and revert: the commit. Empty for a push.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub commit: String,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub dry_run: bool,
    /// Push: the remotes to push (every remote when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remotes: Vec<String>,
    /// Push: how long the daemon waits for the attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl Request {
    /// A push request.
    pub fn push(remotes: Vec<String>, timeout: Duration) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            op: Op::Push,
            path: None,
            commit: String::new(),
            force: false,
            dry_run: false,
            remotes,
            timeout_ms: Some(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
        }
    }

    /// A request to receive the finished fork whose state area is `from`
    /// (an absolute path).
    pub fn receive_fork(from: String) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            op: Op::ReceiveFork,
            path: Some(from),
            commit: String::new(),
            force: false,
            dry_run: false,
            remotes: Vec::new(),
            timeout_ms: None,
        }
    }
}

/// One remote's result in a [`PushReply`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteReply {
    pub name: String,
    pub ok: bool,
    /// What `agentfs push` prints for it, as the daemon rendered them.
    pub lines: Vec<String>,
    /// Commits it still lacks after the attempt, if known.
    #[serde(default)]
    pub unpushed: Option<u64>,
}

/// A finished push request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushReply {
    /// The daemon's process id.
    pub pid: u32,
    /// What the commit before the push did, or why it failed.
    pub records: u64,
    pub commits: u64,
    #[serde(default)]
    pub commit_error: Option<String>,
    pub remotes: Vec<RemoteReply>,
    /// Chosen remotes that are misconfigured, and why: never pushed.
    #[serde(default)]
    pub misconfigured: Vec<String>,
    /// Remotes still pushing at the timeout.
    #[serde(default)]
    pub still_running: Vec<String>,
}

/// A finished restore or revert.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// `<action> <path>` per step.
    pub plan: Vec<String>,
    pub written: usize,
    pub removed: usize,
    /// The operation's commit, if one was made.
    pub commit: Option<String>,
    pub dry_run: bool,
    /// A dry run: LFS objects the plan writes that are not in the local
    /// cache (a restore downloads them), and their total size.
    #[serde(default)]
    pub lfs_uncached: usize,
    #[serde(default)]
    pub lfs_uncached_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Ok(Outcome),
    Pushed(PushReply),
    /// The fork's change (or its finish, when it changed nothing), now in
    /// this volume's history.
    Received {
        change: String,
    },
    Refused {
        kind: String,
        message: String,
        #[serde(default)]
        hint: Option<String>,
    },
    Busy(String),
    Error(String),
}

pub use agentfs_core::local_socket::{Peer, allowed};

/// Serves one request.
pub type Handler = Arc<dyn Fn(Request, Peer) -> Reply + Send + Sync>;

/// `/proc/self/fd/<dir>/agentfs.sock`.
fn socket_address(dir: &OwnedFd) -> String {
    local_socket::socket_address(dir, CONTROL_SOCKET)
}

/// Requests in progress, so shutdown can wait for them.
#[derive(Default)]
struct InFlight {
    count: Mutex<usize>,
    done: Condvar,
}

/// The listening control socket of a running mount.
pub struct Server {
    dir: OwnedFd,
    stop: Arc<AtomicBool>,
    in_flight: Arc<InFlight>,
    accept: Option<JoinHandle<()>>,
}

impl Server {
    /// Listens in `dir` (created `0700`), replacing a stale socket (the
    /// caller holds the state-area lock, so no other daemon owns it).
    pub fn start(dir: &Path, handler: Handler) -> io::Result<Self> {
        let (dir_fd, listener) = local_socket::bind_private(dir, CONTROL_SOCKET)?;
        let stop = Arc::new(AtomicBool::new(false));
        let in_flight = Arc::new(InFlight::default());
        let busy = Arc::new(Mutex::new(()));
        let euid = rustix::process::geteuid().as_raw();
        let accept = {
            let stop = stop.clone();
            let in_flight = in_flight.clone();
            std::thread::Builder::new()
                .name("agentfs-control".into())
                .spawn(move || {
                    for stream in listener.incoming() {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        let Ok(stream) = stream else { continue };
                        *in_flight
                            .count
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
                        let (handler, busy, flight) =
                            (handler.clone(), busy.clone(), in_flight.clone());
                        let spawned = std::thread::Builder::new()
                            .name("agentfs-restore".into())
                            .spawn(move || {
                                serve_one(stream, &handler, &busy, euid);
                                let mut count = flight
                                    .count
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                *count -= 1;
                                flight.done.notify_all();
                            });
                        if spawned.is_err() {
                            *in_flight
                                .count
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) -= 1;
                        }
                    }
                })?
        };
        Ok(Self {
            dir: dir_fd,
            stop,
            in_flight,
            accept: Some(accept),
        })
    }

    /// Stops accepting, waits up to `grace` for a request in progress,
    /// and removes the socket. Returns whether nothing was left running.
    pub fn shutdown(mut self, grace: Duration) -> bool {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept loop, which then sees `stop`.
        let _ = UnixStream::connect(socket_address(&self.dir));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let deadline = Instant::now() + grace;
        let mut count = self
            .in_flight
            .count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *count > 0 {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            count = self
                .in_flight
                .done
                .wait_timeout(count, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        let idle = *count == 0;
        drop(count);
        let _ = local_socket::unlink(&self.dir, CONTROL_SOCKET);
        idle
    }
}

fn write_reply(stream: &mut UnixStream, reply: &Reply) {
    if let Ok(mut line) = serde_json::to_vec(reply) {
        line.push(b'\n');
        let _ = stream.write_all(&line);
    }
}

/// Reads one request line of at most [`MAX_REQUEST`] bytes.
pub fn read_request(reader: impl Read) -> Result<Request, String> {
    let mut line = Vec::new();
    BufReader::new(reader.take(MAX_REQUEST as u64 + 1))
        .read_until(b'\n', &mut line)
        .map_err(|e| format!("reading the request: {e}"))?;
    if line.len() > MAX_REQUEST {
        return Err(format!("request over {MAX_REQUEST} bytes"));
    }
    #[derive(Deserialize)]
    struct Version {
        v: u32,
    }
    let version: Version =
        serde_json::from_slice(&line).map_err(|e| format!("malformed request: {e}"))?;
    if version.v != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version {} is not supported (this daemon speaks {PROTOCOL_VERSION}); \
             use the agentfs binary the daemon runs",
            version.v
        ));
    }
    serde_json::from_slice(&line).map_err(|e| format!("malformed request: {e}"))
}

fn serve_one(mut stream: UnixStream, handler: &Handler, busy: &Mutex<()>, euid: u32) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let peer = match local_socket::peer(&stream) {
        Ok(peer) => peer,
        Err(e) => {
            write_reply(&mut stream, &Reply::Error(format!("peer credentials: {e}")));
            return;
        }
    };
    if !allowed(peer.uid, euid) {
        write_reply(
            &mut stream,
            &Reply::Refused {
                kind: "permission".into(),
                message: format!(
                    "uid {} may not use this mount's control socket (root or uid {euid} only)",
                    peer.uid
                ),
                hint: None,
            },
        );
        return;
    }
    let request = match read_request(&stream) {
        Ok(request) => request,
        Err(e) => {
            write_reply(&mut stream, &Reply::Error(e));
            return;
        }
    };
    // Pushes run beside a restore and each other: the service's push
    // loops serialize them.
    let _running = if request.op.exclusive() {
        match busy.try_lock() {
            Ok(guard) => Some(guard),
            Err(_) => {
                write_reply(
                    &mut stream,
                    &Reply::Busy("another restore or revert is running on this mount".into()),
                );
                return;
            }
        }
    } else {
        None
    };
    let reply = handler(request, peer);
    write_reply(&mut stream, &reply);
}

/// Why the client could not get a reply.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No mount is listening: no socket, or a stale one.
    #[error("no mount is running")]
    NotRunning,
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Protocol(String),
}

/// Sends `request` to the mount listening in `dir` and waits for its
/// reply, however long the restore takes.
pub fn call(dir: &Path, request: &Request) -> Result<Reply, ClientError> {
    let dir_fd = match local_socket::open_dir(dir) {
        Ok(fd) => fd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(ClientError::NotRunning),
        Err(e) => return Err(e.into()),
    };
    let mut stream = match UnixStream::connect(socket_address(&dir_fd)) {
        Ok(stream) => stream,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(ClientError::NotRunning);
        }
        Err(e) => return Err(e.into()),
    };
    let mut line = serde_json::to_vec(request).map_err(|e| ClientError::Protocol(e.to_string()))?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reply = Vec::new();
    BufReader::new(&stream).read_until(b'\n', &mut reply)?;
    if reply.is_empty() {
        return Err(ClientError::Protocol(
            "the mount closed the connection without a reply".into(),
        ));
    }
    serde_json::from_slice(&reply).map_err(|e| ClientError::Protocol(format!("bad reply: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn request() -> Request {
        Request {
            v: PROTOCOL_VERSION,
            op: Op::Restore,
            path: Some("a/b".into()),
            commit: "HEAD~1".into(),
            force: false,
            dry_run: true,
            remotes: Vec::new(),
            timeout_ms: None,
        }
    }

    #[test]
    fn protocol_round_trips() {
        let r = request();
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(
            json,
            r#"{"v":1,"op":"restore","path":"a/b","commit":"HEAD~1","force":false,"dry_run":true}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), r);
        let revert: Request =
            serde_json::from_str(r#"{"v":1,"op":"revert","commit":"abc"}"#).unwrap();
        assert_eq!(
            (revert.op, revert.path, revert.force),
            (Op::Revert, None, false)
        );

        for reply in [
            Reply::Ok(Outcome {
                plan: vec!["write a".into()],
                written: 1,
                removed: 0,
                commit: Some("c".into()),
                dry_run: false,
                lfs_uncached: 1,
                lfs_uncached_bytes: 2,
            }),
            Reply::Refused {
                kind: "unrecorded".into(),
                message: "m".into(),
                hint: Some("h".into()),
            },
            Reply::Pushed(PushReply {
                pid: 7,
                records: 2,
                commits: 1,
                commit_error: None,
                remotes: vec![RemoteReply {
                    name: "origin".into(),
                    ok: true,
                    lines: vec!["origin: pushed none..abc".into()],
                    unpushed: Some(0),
                }],
                misconfigured: vec!["bad: no url".into()],
                still_running: vec!["slow".into()],
            }),
            Reply::Busy("b".into()),
            Reply::Error("e".into()),
        ] {
            let json = serde_json::to_string(&reply).unwrap();
            assert_eq!(
                serde_json::from_str::<Reply>(&json).unwrap(),
                reply,
                "{json}"
            );
        }
        assert!(
            serde_json::to_string(&Reply::Busy("b".into()))
                .unwrap()
                .starts_with(r#"{"busy":"#)
        );
    }

    #[test]
    fn push_requests_round_trip() {
        let all = Request::push(Vec::new(), Duration::from_secs(120));
        let json = serde_json::to_string(&all).unwrap();
        assert_eq!(
            json,
            r#"{"v":1,"op":"push","force":false,"dry_run":false,"timeout_ms":120000}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), all);
        let some = Request::push(vec!["a".into(), "b".into()], Duration::from_millis(5));
        let json = serde_json::to_string(&some).unwrap();
        assert!(json.contains(r#""remotes":["a","b"]"#), "{json}");
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), some);
        // The minimal form a hand-written client sends.
        let bare: Request = serde_json::from_str(r#"{"v":1,"op":"push"}"#).unwrap();
        assert_eq!(
            (
                bare.op,
                bare.commit.as_str(),
                bare.remotes.len(),
                bare.timeout_ms
            ),
            (Op::Push, "", 0, None)
        );
        assert!(
            serde_json::to_string(&Reply::Pushed(PushReply::default()))
                .unwrap()
                .starts_with(r#"{"pushed":{"#)
        );
    }

    /// A push is served while a restore holds the daemon.
    #[test]
    fn pushes_run_beside_a_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("control");
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let wait = Mutex::new(wait);
        let handler: Handler = Arc::new(move |request: Request, _| match request.op {
            Op::Push => Reply::Pushed(PushReply {
                pid: 1,
                ..PushReply::default()
            }),
            _ => {
                if request.commit == "slow" {
                    let _ = wait.lock().unwrap().recv();
                }
                Reply::Ok(Outcome::default())
            }
        });
        let server = Server::start(&dir, handler).unwrap();
        let slow = {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let slow = Request {
                    commit: "slow".into(),
                    ..request()
                };
                // A probe below may hold the daemon when it arrives.
                loop {
                    match call(&dir, &slow) {
                        Ok(Reply::Busy(_)) => std::thread::sleep(Duration::from_millis(1)),
                        other => return other,
                    }
                }
            })
        };
        // Wait until the slow restore holds the daemon.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(call(&dir, &request()).unwrap(), Reply::Busy(_)) {
            assert!(Instant::now() < deadline, "never busy");
            std::thread::sleep(Duration::from_millis(10));
        }
        let push = Request::push(Vec::new(), Duration::from_secs(1));
        for _ in 0..2 {
            assert_eq!(
                call(&dir, &push).unwrap(),
                Reply::Pushed(PushReply {
                    pid: 1,
                    ..PushReply::default()
                })
            );
        }
        release.send(()).unwrap();
        assert!(matches!(slow.join().unwrap(), Ok(Reply::Ok(_))));
        assert!(server.shutdown(Duration::from_secs(5)));
    }

    #[test]
    fn requests_are_checked() {
        let line = format!("{}\n", serde_json::to_string(&request()).unwrap());
        assert_eq!(read_request(line.as_bytes()).unwrap(), request());
        let v2 = line.replace(r#""v":1"#, r#""v":2"#);
        assert!(
            read_request(v2.as_bytes())
                .unwrap_err()
                .contains("protocol version 2 is not supported")
        );
        assert!(
            read_request(&b"not json\n"[..])
                .unwrap_err()
                .contains("malformed")
        );
        let huge = format!(
            r#"{{"v":1,"op":"restore","commit":"{}"}}"#,
            "x".repeat(MAX_REQUEST)
        );
        assert!(read_request(huge.as_bytes()).unwrap_err().contains("over"));
    }

    #[test]
    fn serves_one_request_at_a_time() {
        // A deep directory: the socket path itself would be over 108 bytes.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join("x".repeat(60))
            .join("y".repeat(60))
            .join("control");
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let wait = Mutex::new(wait);
        let handler: Handler = Arc::new(move |request: Request, peer: Peer| {
            if request.commit == "slow" {
                let _ = wait.lock().unwrap().recv();
            }
            Reply::Ok(Outcome {
                plan: vec![format!("{} by {}", request.commit, peer.uid)],
                ..Outcome::default()
            })
        });
        assert!(matches!(
            call(&dir, &request()),
            Err(ClientError::NotRunning)
        ));
        let server = Server::start(&dir, handler).unwrap();
        let mode = std::fs::metadata(dir.join(CONTROL_SOCKET))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let me = rustix::process::geteuid().as_raw();
        match call(&dir, &request()).unwrap() {
            Reply::Ok(o) => assert_eq!(o.plan, [format!("HEAD~1 by {me}")]),
            other => panic!("{other:?}"),
        }

        let slow = {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let slow = Request {
                    commit: "slow".into(),
                    ..request()
                };
                // It can arrive while one of the probes below is served,
                // and be the one told busy: then it asks again.
                loop {
                    match call(&dir, &slow) {
                        Ok(Reply::Busy(_)) => std::thread::sleep(Duration::from_millis(1)),
                        other => return other,
                    }
                }
            })
        };
        // Wait until the slow request holds the daemon.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match call(&dir, &request()).unwrap() {
                Reply::Busy(_) => break,
                Reply::Ok(_) => {}
                other => panic!("{other:?}"),
            }
            assert!(Instant::now() < deadline, "never busy");
            std::thread::sleep(Duration::from_millis(10));
        }
        release.send(()).unwrap();
        assert!(matches!(slow.join().unwrap(), Ok(Reply::Ok(_))));

        // A bad request gets an error reply, not a hang.
        let fd = local_socket::open_dir(&dir).unwrap();
        let mut raw = UnixStream::connect(socket_address(&fd)).unwrap();
        raw.write_all(b"{\"v\":9,\"op\":\"revert\",\"commit\":\"x\"}\n")
            .unwrap();
        let mut reply = String::new();
        BufReader::new(&raw).read_line(&mut reply).unwrap();
        assert!(reply.contains("not supported"), "{reply}");

        assert!(server.shutdown(Duration::from_secs(5)));
        assert!(!dir.join(CONTROL_SOCKET).exists());
        assert!(matches!(
            call(&dir, &request()),
            Err(ClientError::NotRunning)
        ));
        // A stale socket file (a crashed daemon) is not running either,
        // and the next server replaces it.
        let stale =
            UnixListener::bind(socket_address(&local_socket::open_dir(&dir).unwrap())).unwrap();
        drop(stale);
        assert!(matches!(
            call(&dir, &request()),
            Err(ClientError::NotRunning)
        ));
        let server = Server::start(&dir, Arc::new(|_, _| Reply::Error("second".into()))).unwrap();
        assert_eq!(
            call(&dir, &request()).unwrap(),
            Reply::Error("second".into())
        );
        server.shutdown(Duration::from_secs(1));
    }
}
