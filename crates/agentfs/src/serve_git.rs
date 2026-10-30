//! `agentfs serve-git`: the volume's history repository as a git server for
//! patch pushes (design §12, §17.1, §19.2 item 11; bead agentfs-mia.9).
//!
//! The owner, 2026-10-28: "agentfs should be a legitimate git server, and
//! its up to the specific use case to wire up the relationship of the
//! various distributed remotes"; "git protocol is ideal".
//!
//! - **Transport.** git's smart HTTP, served by git's own CGI program, `git
//!   http-backend`, behind a small HTTP/1.1 server in the binary
//!   (`tiny_http`, design §17.3). Only the smart endpoints are served
//!   (`GET /info/refs?service=...`, `POST /git-upload-pack`, `POST
//!   /git-receive-pack`), never dumb HTTP, which would hand out every
//!   object. Every pusher speaks plain git: `agentfs patch submit` and
//!   `patch approve` work against its URL unchanged.
//! - **The patch source.** It refuses to start while
//!   `agentfs.patchRemote` names a remote: the queue's fetch would prune
//!   what pushers wrote. It never takes the state-area lock; git writes
//!   the refs (with its lock files), as a push to a `file://` remote does.
//! - **Refs.** Fetches and pushes are shown only the branch and
//!   `refs/agentfs/<volume>/{inbox,applied,approved}/*` and its `status`
//!   (`transfer.hideRefs`); `held/*`, other volumes' refs, `HEAD` and the
//!   rest are not listed. That hides names, not objects: under any
//!   protocol a pusher can read a hidden object whose id it knows, by
//!   fetching it by id (protocol v2's upload-pack serves it) or by pushing
//!   a patch whose tree names it and fetching that (receive-pack's
//!   connectivity check finds the object already there; v0 too)
//!   (git-config(1) `transfer.hideRefs`, gitnamespaces(7) "SECURITY"). Every client is a
//!   pusher the operator configured, and the threat is mistakes, not
//!   malice (design §1.1), so this is kept; data no pusher may read
//!   belongs in another repository.
//! - **Pushers.** Every request authenticates as a pusher of the volume's
//!   config ([`agentfs_core::patch::admit`]): HTTP Basic with the pusher's
//!   name and token, or a bearer token; otherwise 401. git runs with the
//!   pusher's name as `REMOTE_USER`.
//! - **Admit before keeping.** `git receive-pack` runs with
//!   `core.hooksPath` set to agentfs's own hooks under the state area
//!   (`meta/serve-git/hooks`, `0700`, written at each start): `pre-receive`
//!   is [`admit_hook`], which refuses the whole push unless the pusher's
//!   rights allow every update and every offered tip is a patch the queue
//!   could land; git then discards the quarantined objects.
//! - **Arrival.** `post-receive` is [`arrived_hook`]: the queue is judged
//!   and the integrator notified, never landed (the owner, 2026-10-29: "No
//!   live landing yet"). Through a running daemon's control socket
//!   (`judge`), else under the state-area lock taken without waiting; when
//!   another command holds the lock, that command's own run judges.
//! - **Integrator.** `POST /agentfs/apply`, for a pusher with the
//!   `integrator` right, is `agentfs patch apply`: a running daemon judges
//!   and answers 409 with its reply; otherwise `agentfs patch apply` runs.
//!
//! Costs: plain HTTP only (TLS belongs to a fronting proxy), one volume per
//! process, no SSH transport. Request bodies are spooled to a file under
//! the state area before git reads them. Under the sidecar the workload
//! can reach the port, but the tokens live outside the mount.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use git2::Repository;
use serde::Serialize;
use tiny_http::{Header, Method, Response, Server, StatusCode};

use agentfs_core::config::Settings;
use agentfs_core::git_cli;
use agentfs_core::identity::IdentityRenderer;
use agentfs_core::lock::{LockError, StateLock};
use agentfs_core::patch::admit::{self, Pusher, RefUpdate, Right};
use agentfs_core::patch::{Verdict, applied_prefix, approved_prefix, inbox_prefix, status_ref};
use agentfs_core::volume::StateDir;

use crate::control::{self, ClientError, Reply, Request};
use crate::daemon::Daemon;
use crate::patch_cmd::{self, Applier};
use crate::{EXIT_LOCKED, Pod, Session};

/// How long a stopping `serve-git` waits for requests in progress.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// agentfs's own git hooks, under the state area's `meta/`.
const HOOKS_DIR: &str = "serve-git/hooks";

/// Where request bodies are spooled, under `meta/`.
const SPOOL_DIR: &str = "serve-git";

/// Who `serve-git`'s requests are answered with.
struct Backend {
    git: PathBuf,
    git_dir: PathBuf,
    /// The state area, absolute.
    state: StateDir,
    root: Option<PathBuf>,
    exe: PathBuf,
    /// `-c` settings for `git http-backend`.
    config: Vec<String>,
    /// Passed to git, and so to the hooks and `patch apply`.
    env: Vec<(String, String)>,
    spooled: AtomicU64,
}

/// `agentfs serve-git --listen ADDR` (see the module docs): serves until
/// SIGINT or SIGTERM, then waits up to `shutdown_grace` for requests in
/// progress.
pub(crate) fn serve(
    state: &StateDir,
    root: Option<&Path>,
    listen: &str,
    pod: &Pod,
    shutdown_grace: Duration,
) -> Result<ExitCode> {
    crate::refuse_finished(state)?;
    let daemon = Daemon::start()?;
    let state = StateDir::new(
        state
            .root()
            .canonicalize()
            .with_context(|| format!("the state area {}", state.root().display()))?,
    );
    let settings = Settings::load(&state.git())
        .with_context(|| format!("reading {}", state.git().display()))?;
    if let Some(remote) = settings.patch_remote() {
        bail!(
            "serve-git is the volume's patch source, but agentfs.patchRemote is the remote {:?} \
             (its fetch would prune what pushers write): set agentfs.patchRemote = none",
            remote.name
        );
    }
    pushers(&state)?;
    let git = git_cli::find_git().ok_or_else(|| {
        anyhow!(
            "serve-git runs `git http-backend`: no git in {}",
            git_cli::GIT_PATH
        )
    })?;
    let hooks = install_hooks(&state)?;
    let server = Server::http(listen).map_err(|e| anyhow!("listening on {listen}: {e}"))?;
    let address = server
        .server_addr()
        .to_ip()
        .map_or_else(|| listen.to_owned(), |a| a.to_string());
    let backend = Arc::new(Backend {
        git,
        git_dir: state.git(),
        config: git_config(&settings.volume, &settings.branch_ref, &hooks),
        env: child_env(pod),
        root: root.map(Path::to_path_buf),
        exe: std::env::current_exe().context("finding the agentfs binary")?,
        state,
        spooled: AtomicU64::new(0),
    });
    println!("serve-git: listening on http://{address}/");
    io::stdout().flush()?;

    let server = Arc::new(server);
    let in_flight = Arc::new((Mutex::new(0usize), Condvar::new()));
    let accept = {
        let (server, in_flight, backend) = (server.clone(), in_flight.clone(), backend.clone());
        std::thread::Builder::new()
            .name("agentfs-serve-git".into())
            .spawn(move || {
                for request in server.incoming_requests() {
                    *lock(&in_flight.0) += 1;
                    let (flight, backend) = (in_flight.clone(), backend.clone());
                    let spawned = std::thread::Builder::new()
                        .name("agentfs-git-request".into())
                        .spawn(move || {
                            handle(&backend, request);
                            *lock(&flight.0) -= 1;
                            flight.1.notify_all();
                        });
                    if spawned.is_err() {
                        *lock(&in_flight.0) -= 1;
                    }
                }
            })?
    };
    let wake = daemon.wait();
    eprintln!("serve-git: {}; stopping", crate::describe_wake(wake));
    server.unblock();
    let _ = accept.join();
    let deadline = Instant::now() + shutdown_grace;
    let mut count = lock(&in_flight.0);
    while *count > 0 && Instant::now() < deadline {
        count = in_flight
            .1
            .wait_timeout(count, deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
    if *count > 0 {
        eprintln!(
            "serve-git: {} request(s) still running at the grace's end",
            *count
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The volume's pushers, read now.
fn pushers(state: &StateDir) -> Result<Vec<Pusher>> {
    let path = state.git().join("config");
    let config =
        git2::Config::open(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(admit::pushers(&config)?)
}

/// The `-c` settings `git http-backend` runs with: agentfs's hooks, the
/// visible refs, smart HTTP only, durable objects and refs before a push
/// is acknowledged, and no gc (agentfs repacks, design §8).
fn git_config(volume: &str, branch_ref: &str, hooks: &Path) -> Vec<String> {
    let mut config = vec![
        format!("core.hooksPath={}", hooks.display()),
        "core.fsync=committed".to_owned(),
        "receive.autogc=false".to_owned(),
        "http.receivepack=true".to_owned(),
        "http.uploadpack=true".to_owned(),
        "http.getanyfile=false".to_owned(),
        // Hide everything, then show the volume's own (the last match
        // wins; a pattern matches a ref or a prefix ending at `/`).
        "transfer.hideRefs=refs".to_owned(),
        "transfer.hideRefs=HEAD".to_owned(),
    ];
    for visible in [
        branch_ref.to_owned(),
        inbox_prefix(volume),
        applied_prefix(volume),
        approved_prefix(volume),
        status_ref(volume),
    ] {
        config.push(format!(
            "transfer.hideRefs=!{}",
            visible.trim_end_matches('/')
        ));
    }
    config
}

/// The pod's flags as environment variables, so the hooks and `patch
/// apply` see the same pod; and the telemetry variables, so they export
/// where this process does.
fn child_env(pod: &Pod) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = [
        ("AGENTFS_NAMESPACE", &pod.namespace),
        ("AGENTFS_POD", &pod.pod),
        ("AGENTFS_POD_UID", &pod.pod_uid),
        ("AGENTFS_NODE", &pod.node),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.clone()))
    .collect();
    if let Some(replica) = &pod.replica {
        env.push(("AGENTFS_REPLICA".into(), replica.clone()));
    }
    for (name, value) in std::env::vars() {
        if name.starts_with("OTEL_") {
            env.push((name, value));
        }
    }
    env
}

/// `'...'` for sh.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Writes `pre-receive` and `post-receive` into `meta/serve-git/hooks`
/// (`0700`), each replaced whole; returns the directory.
fn install_hooks(state: &StateDir) -> Result<PathBuf> {
    let dir = state.meta().join(HOOKS_DIR);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let exe = std::env::current_exe().context("finding the agentfs binary")?;
    for (hook, command) in [("pre-receive", "admit"), ("post-receive", "arrived")] {
        let script = format!(
            "#!/bin/sh\n# Written by agentfs serve-git at each start (bead agentfs-mia.9).\nexec \
             {} --state {} serve-git {command}\n",
            sh_quote(&exe.to_string_lossy()),
            sh_quote(&state.root().to_string_lossy()),
        );
        let path = dir.join(hook);
        let tmp = dir.join(format!(".{hook}.tmp"));
        let _ = fs::remove_file(&tmp);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(script.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(dir)
}

/// A request's header, by name (any case).
fn header<'r>(request: &'r tiny_http::Request, name: &str) -> Option<&'r str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn plain(status: u16, body: impl Into<Vec<u8>>) -> Response<io::Cursor<Vec<u8>>> {
    let mut body = body.into();
    body.push(b'\n');
    Response::from_data(body)
        .with_status_code(status)
        .with_header(content_type("text/plain; charset=utf-8"))
}

fn content_type(value: &str) -> Header {
    // A fixed, valid header: cannot fail.
    Header::from_bytes(&b"Content-Type"[..], value.as_bytes())
        .unwrap_or_else(|()| unreachable!("a valid header"))
}

fn handle(backend: &Backend, request: tiny_http::Request) {
    let url = request.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let pushers = match pushers(&backend.state) {
        Ok(pushers) => pushers,
        Err(e) => {
            eprintln!("serve-git: {e:#}");
            let _ = request.respond(plain(500, "the volume's pushers cannot be read"));
            return;
        }
    };
    let Some(pusher) = admit::authenticate(&pushers, header(&request, "Authorization")) else {
        let challenge =
            Header::from_bytes(&b"WWW-Authenticate"[..], &b"Basic realm=\"agentfs\""[..])
                .unwrap_or_else(|()| unreachable!("a valid header"));
        let _ = request.respond(plain(401, "authentication required").with_header(challenge));
        return;
    };
    let service = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("service="));
    let method = request.method().clone();
    let result = match (&method, path) {
        (Method::Get, "/info/refs")
            if matches!(service, Some("git-upload-pack" | "git-receive-pack")) =>
        {
            cgi(backend, request, pusher, path, query)
        }
        (Method::Post, "/git-upload-pack" | "/git-receive-pack") => {
            cgi(backend, request, pusher, path, query)
        }
        (Method::Post, "/agentfs/apply") if pusher.rights.contains(&Right::Integrator) => {
            let (status, body) = apply(backend);
            request.respond(
                Response::from_data(body)
                    .with_status_code(status)
                    .with_header(content_type("application/json")),
            )
        }
        (Method::Post, "/agentfs/apply") => request.respond(plain(
            403,
            format!(
                "{} may not apply the queue (Integrator right needed)",
                pusher.name
            ),
        )),
        _ => request.respond(plain(404, "not found")),
    };
    if let Err(e) = result {
        eprintln!("serve-git: {method} {path}: {e}");
    }
}

/// Copies the request body to an unlinked file under the state area, read
/// back from its start: git's stdin.
fn spool(backend: &Backend, request: &mut tiny_http::Request) -> io::Result<(File, u64)> {
    let dir = backend.state.meta().join(SPOOL_DIR);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let n = backend.spooled.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("body-{}-{n}", std::process::id()));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let _ = fs::remove_file(&path);
    let len = io::copy(request.as_reader(), &mut file)?;
    file.rewind()?;
    Ok((file, len))
}

/// Runs `git http-backend` for one request and streams its answer.
fn cgi(
    backend: &Backend,
    mut request: tiny_http::Request,
    pusher: &Pusher,
    path: &str,
    query: &str,
) -> io::Result<()> {
    let (body, len) = match spool(backend, &mut request) {
        Ok(spooled) => spooled,
        Err(e) => {
            eprintln!("serve-git: spooling a request body: {e}");
            return request.respond(plain(500, "the request could not be read"));
        }
    };
    let mut cmd = git_cli::command(&backend.git, &backend.git_dir);
    cmd.env_remove("GIT_DIR");
    for setting in &backend.config {
        cmd.arg("-c").arg(setting);
    }
    cmd.arg("http-backend")
        .envs(backend.env.iter().map(|(k, v)| (k, v)))
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .env("REQUEST_METHOD", request.method().as_str())
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("GIT_PROJECT_ROOT", &backend.git_dir)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REMOTE_USER", &pusher.name)
        .env("CONTENT_LENGTH", len.to_string())
        .stdin(Stdio::from(body))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(addr) = request.remote_addr() {
        cmd.env("REMOTE_ADDR", addr.ip().to_string());
    }
    for h in request.headers() {
        let name = h.field.as_str().as_str();
        if name.eq_ignore_ascii_case("content-type") {
            cmd.env("CONTENT_TYPE", h.value.as_str());
        } else if !["authorization", "proxy-authorization", "content-length"]
            .iter()
            .any(|n| name.eq_ignore_ascii_case(n))
        {
            cmd.env(
                format!("HTTP_{}", name.to_ascii_uppercase().replace('-', "_")),
                h.value.as_str(),
            );
        }
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("serve-git: running git http-backend: {e}");
            return request.respond(plain(500, "git http-backend could not run"));
        }
    };
    let result = answer_cgi(request, &mut child);
    let _ = child.wait();
    result
}

fn answer_cgi(request: tiny_http::Request, child: &mut Child) -> io::Result<()> {
    let Some(stdout) = child.stdout.take() else {
        return request.respond(plain(500, "git http-backend gave no output"));
    };
    let mut stdout = BufReader::new(stdout);
    let head = match read_cgi_head(&mut stdout) {
        Ok(head) => head,
        Err(e) => {
            eprintln!("serve-git: git http-backend: {e}");
            return request.respond(plain(500, "git http-backend failed"));
        }
    };
    let headers = head
        .headers
        .iter()
        .filter_map(|(k, v)| Header::from_bytes(k.as_bytes(), v.as_bytes()).ok())
        .collect();
    request.respond(Response::new(
        StatusCode(head.status),
        headers,
        stdout,
        head.length,
        None,
    ))
}

/// A CGI response's header block.
#[derive(Debug, PartialEq, Eq)]
struct CgiHead {
    status: u16,
    /// Every header but `Status`, `Content-Length` and
    /// `Transfer-Encoding`, which the server writes itself.
    headers: Vec<(String, String)>,
    length: Option<usize>,
}

/// Reads a CGI header block (RFC 3875 §6): lines up to an empty one,
/// `Status: <code> <reason>` setting the status (default 200).
fn read_cgi_head(reader: &mut impl BufRead) -> io::Result<CgiHead> {
    let mut head = CgiHead {
        status: 200,
        headers: Vec::new(),
        length: None,
    };
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "no header block before the end of its output",
            ));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Ok(head);
        }
        let (name, value) = line.split_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad header line {line:?}"),
            )
        })?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            head.status = value
                .split_whitespace()
                .next()
                .and_then(|code| code.parse().ok())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("bad status {value:?}"))
                })?;
        } else if name.eq_ignore_ascii_case("content-length") {
            head.length = value.parse().ok();
        } else if !name.eq_ignore_ascii_case("transfer-encoding") {
            head.headers.push((name.to_owned(), value.to_owned()));
        }
    }
}

/// What `POST /agentfs/apply` answers when `patch apply` ran.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ApplyRun {
    /// `agentfs patch apply`'s exit status and what it printed.
    Ran { exit: i32, output: String },
}

/// `POST /agentfs/apply`: `agentfs patch apply` (a running daemon first).
/// Its status and JSON body: a daemon's reply (409 when attached), or the
/// command's run (200 exit 0, 422 exit 1, 409 exit 75, else 500).
fn apply(backend: &Backend) -> (u16, Vec<u8>) {
    let json = |reply: &Reply| serde_json::to_vec(reply).unwrap_or_default();
    match control::call(&backend.state.control_dir(), &Request::apply(true)) {
        Ok(reply @ (Reply::Attached { .. } | Reply::Busy(_))) => return (409, json(&reply)),
        // A daemon from before `apply`: the command's lock path refuses.
        Ok(Reply::Error(m)) if m.contains("unknown variant") => {}
        Ok(reply) => return (500, json(&reply)),
        Err(ClientError::NotRunning | ClientError::Protocol(_)) => {}
        Err(ClientError::Io(e)) => {
            return (500, json(&Reply::Error(format!("the control socket: {e}"))));
        }
    }
    let mut cmd = Command::new(&backend.exe);
    cmd.arg("--state").arg(backend.state.root());
    if let Some(root) = &backend.root {
        cmd.arg("--root").arg(root);
    }
    cmd.args(["patch", "apply"])
        .envs(backend.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null());
    let (status, run) = match cmd.output() {
        Ok(out) => {
            let exit = out.status.code().unwrap_or(-1);
            let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
            output.push_str(&String::from_utf8_lossy(&out.stderr));
            let status = match exit {
                0 => 200,
                1 => 422,
                e if e == i32::from(EXIT_LOCKED) => 409,
                _ => 500,
            };
            (status, ApplyRun::Ran { exit, output })
        }
        Err(e) => (
            500,
            ApplyRun::Ran {
                exit: -1,
                output: format!("running agentfs patch apply: {e}"),
            },
        ),
    };
    (status, serde_json::to_vec(&run).unwrap_or_default())
}

/// The `pre-receive` hook (`serve-git admit`): exit 0 admits the push;
/// 1 refuses it whole, with the reason on stderr, which git relays to the
/// pusher.
pub(crate) fn admit_hook(state: &StateDir) -> Result<ExitCode> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    Ok(match admit_push(state, &input) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("agentfs: push refused: {why}");
            ExitCode::FAILURE
        }
    })
}

fn admit_push(state: &StateDir, input: &str) -> Result<(), String> {
    let user = std::env::var("REMOTE_USER")
        .map_err(|_| "no authenticated pusher (REMOTE_USER is not set)".to_owned())?;
    let pushers = pushers(state).map_err(|e| format!("{e:#}"))?;
    let pusher = pushers
        .iter()
        .find(|p| p.name == user)
        .ok_or_else(|| format!("{user} is not a pusher of this volume"))?;
    let settings = Settings::load(&state.git()).map_err(|e| e.to_string())?;
    let updates = input
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| RefUpdate::parse(l).ok_or_else(|| format!("bad hook input {l:?}")))
        .collect::<Result<Vec<_>, _>>()?;
    let repo = Repository::open_bare(state.git()).map_err(|e| e.to_string())?;
    // The pushed objects, still quarantined.
    if let Some(quarantine) =
        std::env::var_os("GIT_QUARANTINE_PATH").or_else(|| std::env::var_os("GIT_OBJECT_DIRECTORY"))
    {
        repo.odb()
            .and_then(|odb| odb.add_disk_alternate(&quarantine.to_string_lossy()))
            .map_err(|e| format!("reading the pushed objects: {e}"))?;
    }
    admit::admit(
        &repo,
        &settings.volume,
        &settings.branch_ref,
        pusher,
        &updates,
    )
    .map_err(|e| e.to_string())
}

/// The `post-receive` hook (`serve-git arrived`): judges and notifies,
/// never lands. Always exit 0: a push git already kept is never failed
/// afterwards. A judging failure is printed (the pusher sees it); a
/// notify failure raises `NotifyFailed` ([`patch_cmd::judge`]).
pub(crate) fn arrived_hook(state: &StateDir, pod: &Pod) -> Result<ExitCode> {
    // git writes the updates to stdin; they are not needed.
    let _ = io::copy(&mut io::stdin(), &mut io::sink());
    match control::call(&state.control_dir(), &Request::judge()) {
        Ok(Reply::Judged { verdicts }) => print_verdicts(&verdicts),
        Ok(Reply::Error(m)) if m.contains("unknown variant") => {
            eprintln!("agentfs: the running daemon cannot judge yet; its next run judges the push")
        }
        Ok(other) => eprintln!("agentfs: judging through the daemon: {other:?}"),
        Err(ClientError::NotRunning | ClientError::Protocol(_)) => judge_locked(state, pod),
        Err(ClientError::Io(e)) => eprintln!("agentfs: the daemon's control socket: {e}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn print_verdicts(verdicts: &[(u64, Verdict)]) {
    for (n, verdict) in verdicts {
        let (label, detail) = patch_cmd::verdict_parts(verdict);
        eprintln!("agentfs: patch {n} {label}{detail}");
    }
}

/// No daemon: judges under the state-area lock, taken without waiting;
/// another holder's own run judges.
fn judge_locked(state: &StateDir, pod: &Pod) {
    let _lock = match StateLock::acquire(state, "serve-git arrived", Duration::ZERO) {
        Ok(lock) => lock,
        Err(LockError::Busy { holder, .. }) => {
            eprintln!("agentfs: the volume is in use ({holder}); its run judges the push");
            return;
        }
        Err(e) => {
            eprintln!("agentfs: judging the push: {e}");
            return;
        }
    };
    if let Err(e) = (|| -> Result<()> {
        let session = Session::start(state, pod)?;
        let settings = Settings::load(&state.git())?;
        let committer =
            IdentityRenderer::new(settings.identity.clone(), &pod.context(&settings.volume))?
                .committer()
                .clone();
        let applier =
            Applier::with_identity(committer, session.events.clone(), session.telemetry());
        let judged = patch_cmd::judge(state, &applier);
        session.finish();
        print_verdicts(&judged?);
        Ok(())
    })() {
        eprintln!("agentfs: judging the push: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgi_heads_parse() {
        let mut out =
            &b"Status: 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: 3\r\n\
                         Expires: never\r\n\r\nno\n"[..];
        assert_eq!(
            read_cgi_head(&mut out).unwrap(),
            CgiHead {
                status: 403,
                headers: vec![
                    ("Content-Type".into(), "text/plain".into()),
                    ("Expires".into(), "never".into()),
                ],
                length: Some(3),
            }
        );
        assert_eq!(out, b"no\n");
        let mut out = &b"Content-Type: x\n\nbody"[..];
        assert_eq!(read_cgi_head(&mut out).unwrap().status, 200);
        assert!(read_cgi_head(&mut &b"Content-Type: x\r\n"[..]).is_err());
        assert!(read_cgi_head(&mut &b"Status: soon\r\n\r\n"[..]).is_err());
    }

    #[test]
    fn only_the_volumes_refs_are_shown() {
        let config = git_config("v", "refs/heads/agentfs/v", Path::new("/h"));
        let hide: Vec<&str> = config
            .iter()
            .filter_map(|c| c.strip_prefix("transfer.hideRefs="))
            .collect();
        assert_eq!(
            hide,
            [
                "refs",
                "HEAD",
                "!refs/heads/agentfs/v",
                "!refs/agentfs/v/inbox",
                "!refs/agentfs/v/applied",
                "!refs/agentfs/v/approved",
                "!refs/agentfs/v/status",
            ]
        );
        assert!(config.contains(&"core.hooksPath=/h".to_owned()));
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(sh_quote("/a b"), "'/a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }
}
