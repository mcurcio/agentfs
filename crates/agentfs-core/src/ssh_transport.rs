//! agentfs's SSH transport for libgit2 (design §11.1, `ssh` feature).
//!
//! libgit2 is built without libssh2. Instead, [`register`] installs this
//! module as libgit2's smart subtransport for `ssh://`, `ssh+git://`,
//! `git+ssh://` and scp-like `[user@]host:path` URLs (libgit2 sends any URL
//! with a `:` that no other transport claims here). Each connection runs
//! `git-upload-pack '<path>'` or `git-receive-pack '<path>'` over one
//! [russh](russh) session, with ring for its cryptography.
//!
//! The remote's settings reach the transport through a thread-local
//! [`Scope`], entered by [`crate::remote_auth::RemoteAuth::ssh_scope`]
//! around each push or fetch: libgit2 creates the transport and does all
//! its reads and writes on the calling thread. A connection made with no
//! scope, to a URL that is not the scope's push URL host and port, or to a
//! local path containing `:` fails closed: nothing is offered and no host
//! key is trusted.
//!
//! In order, a connection:
//!
//! 1. reads the key from `agentfsSshKey` (a key that cannot be read or
//!    decoded is an `auth` failure, and nothing is sent);
//! 2. connects within [`SERVER_CONNECT_TIMEOUT_MS`], asking for the host key
//!    types `agentfsKnownHosts` lists for the host first, as OpenSSH does;
//! 3. checks the host key with
//!    [`SshRemote::check_host_key`](crate::remote_auth::SshRemote) before
//!    any authentication, recording a refusal in the connection's
//!    [`AuthState`];
//! 4. offers that one key, once, as the URL's user (else `git`);
//! 5. runs the git command, with every read and write bounded by
//!    [`SERVER_IO_TIMEOUT_MS`] (libgit2's own timeouts do not apply to
//!    custom transports).
//!
//! The SSH session runs on a current-thread tokio runtime owned by the
//! connection's stream and driven only while libgit2 reads or writes.

use std::borrow::Cow;
use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use git2::transport::{Service, SmartSubtransport, SmartSubtransportStream, Transport};
use git2::{Error as GitError, ErrorClass, ErrorCode};
use russh::client::{self, Handle};
use russh::keys::{Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{Channel, ChannelMsg, Disconnect, Preferred};
use tokio::runtime::Runtime;

use crate::credentials::DEFAULT_SSH_USERNAME;
use crate::history::{SERVER_CONNECT_TIMEOUT_MS, SERVER_IO_TIMEOUT_MS};
use crate::known_hosts::lookup_name;
use crate::remote_auth::{AuthState, SshRemote};
use crate::remote_url::{self, Endpoint};

/// The URL schemes the transport is registered for (scp-like URLs reach
/// it through `ssh`, as libgit2 looks them up).
const SCHEMES: [&str; 3] = ["ssh", "ssh+git", "git+ssh"];

/// The most of a command's stderr kept for an error message.
const MAX_STDERR: usize = 4096;

/// How long a stream waits for the exit status once the server has sent
/// its end of file.
const EXIT_WAIT: Duration = Duration::from_secs(5);

/// The remote and the state of the push or fetch running on this thread.
#[derive(Clone)]
struct Context {
    remote: Arc<SshRemote>,
    state: Arc<AuthState>,
}

thread_local! {
    static CURRENT: RefCell<Option<Context>> = const { RefCell::new(None) };
    /// Tests only: a shorter I/O timeout for connections made on this
    /// thread.
    #[cfg(test)]
    static IO_TIMEOUT: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// The SSH settings of one push or fetch, for libgit2 calls on this
/// thread until dropped. Scopes nest; the previous one comes back.
pub(crate) struct Scope {
    previous: Option<Context>,
    /// Thread-bound: dropping it elsewhere would restore the wrong thread.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Scope {
    pub(crate) fn enter(remote: Arc<SshRemote>, state: Arc<AuthState>) -> Self {
        let previous = CURRENT.with(|c| c.replace(Some(Context { remote, state })));
        Self {
            previous,
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|c| *c.borrow_mut() = previous);
    }
}

/// Registers the transport with libgit2. Called once, from
/// [`crate::history::init_libgit2`], before any remote is used.
pub(crate) fn register() {
    for scheme in SCHEMES {
        // SAFETY: called once, inside init_libgit2's `Once`, before this
        // process creates any transport.
        let result = unsafe { git2::transport::register(scheme, factory) };
        assert!(
            result.is_ok(),
            "libgit2 refused the SSH transport for {scheme}://: {result:?}"
        );
    }
}

fn factory(remote: &git2::Remote<'_>) -> Result<Transport, GitError> {
    let context = CURRENT.with(|c| c.borrow().clone());
    Transport::smart(remote, false, Subtransport { context })
}

struct Subtransport {
    context: Option<Context>,
}

impl SmartSubtransport for Subtransport {
    fn action(
        &self,
        url: &str,
        action: Service,
    ) -> Result<Box<dyn SmartSubtransportStream>, GitError> {
        let Some(context) = &self.context else {
            return Err(ssh_error(
                ErrorCode::GenericError,
                format!(
                    "{url}: no remote's SSH settings apply to this connection; agentfs \
                     connects over SSH only for a remote's own push or fetch"
                ),
            ));
        };
        let command = match action {
            Service::UploadPackLs | Service::UploadPack => "git-upload-pack",
            Service::ReceivePackLs | Service::ReceivePack => "git-receive-pack",
        };
        let stream = Stream::open(context, url, command)?;
        Ok(Box::new(stream))
    }

    fn close(&self) -> Result<(), GitError> {
        Ok(())
    }
}

fn ssh_error(code: ErrorCode, message: impl AsRef<str>) -> GitError {
    GitError::new(code, ErrorClass::Ssh, message.as_ref())
}

fn connect_timeout() -> Duration {
    Duration::from_millis(SERVER_CONNECT_TIMEOUT_MS.unsigned_abs().into())
}

fn io_timeout() -> Duration {
    #[cfg(test)]
    if let Some(timeout) = IO_TIMEOUT.with(std::cell::Cell::get) {
        return timeout;
    }
    Duration::from_millis(SERVER_IO_TIMEOUT_MS.unsigned_abs().into())
}

/// Tests only: makes connections opened on this thread time out on reads
/// and writes after `timeout` instead.
#[cfg(test)]
pub(crate) fn set_io_timeout_for_this_thread(timeout: Option<Duration>) {
    IO_TIMEOUT.with(|t| t.set(timeout));
}

/// `s` quoted for a POSIX shell, as git quotes the repository path of an
/// SSH command (`'` and `!` outside the quotes).
pub(crate) fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' | '!' => {
                out.push('\'');
                out.push('\\');
                out.push(c);
                out.push('\'');
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// The host key algorithms to ask for: those of the key types listed in
/// known_hosts for the host, in file order, then every other one russh
/// supports, so that an unlisted type still reaches the host key check and
/// its "lists only" message.
pub(crate) fn host_key_preference(listed: &[String]) -> Vec<Algorithm> {
    let mut out: Vec<Algorithm> = Vec::new();
    let mut add = |a: Algorithm| {
        if !out.contains(&a) {
            out.push(a);
        }
    };
    for key_type in listed {
        if key_type == "ssh-rsa" {
            add(Algorithm::Rsa {
                hash: Some(HashAlg::Sha512),
            });
            add(Algorithm::Rsa {
                hash: Some(HashAlg::Sha256),
            });
        } else if let Ok(a) = Algorithm::from_str(key_type)
            && Preferred::DEFAULT.key.contains(&a)
        {
            add(a);
        }
    }
    for a in Preferred::DEFAULT.key.iter() {
        add(a.clone());
    }
    out
}

/// The russh side of one connection: checks the host key.
struct Client {
    remote: Arc<SshRemote>,
    state: Arc<AuthState>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        presented: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let host = self.remote.endpoint().host.clone();
        let verdict = match presented {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                let raw = key.to_bytes().ok();
                let sha256 = key.fingerprint(HashAlg::Sha256).sha256();
                self.remote
                    .check_host_key(&host, raw.as_deref(), sha256.as_ref())
            }
            PublicKeyOrCertificate::Certificate(cert) => Err(format!(
                "the SSH server at {} presented a {} host certificate, which agentfs did not \
                 ask for",
                lookup_name(&host, self.remote.endpoint().port),
                cert.algorithm()
            )),
        };
        match verdict {
            Ok(()) => Ok(true),
            Err(why) => {
                self.state.refuse_host(why);
                Ok(false)
            }
        }
    }
}

/// One running git command over SSH, as libgit2 reads and writes it.
struct Stream {
    runtime: Runtime,
    session: Handle<Client>,
    channel: Channel<client::Msg>,
    /// `[user@]host:port` and the command, for messages.
    peer: String,
    command: String,
    io_timeout: Duration,
    buffer: Vec<u8>,
    position: usize,
    /// The server sent its end of file.
    eof: bool,
    /// The channel is closed; reads return 0.
    done: bool,
    exit_status: Option<u32>,
    stderr: Vec<u8>,
}

impl Stream {
    fn open(context: &Context, url: &str, command: &str) -> Result<Self, GitError> {
        let Context { remote, state } = context;
        let (Some(endpoint), Some(path)) =
            (remote_url::ssh_endpoint(url), remote_url::ssh_path(url))
        else {
            return Err(ssh_error(
                ErrorCode::GenericError,
                format!(
                    "{url} is not an SSH URL with a host and a path (libgit2 sends a local \
                     path containing ':' to the SSH transport; use a file:// URL for it)"
                ),
            ));
        };
        let bound = remote.endpoint();
        let lookup = lookup_name(&bound.host, bound.port);
        if endpoint != *bound {
            return Err(ssh_error(
                ErrorCode::GenericError,
                format!(
                    "{url} is not on {lookup}, the host of remote {}'s push URL; nothing is \
                     offered to it",
                    remote.name()
                ),
            ));
        }
        let auth_error = |message: String| {
            state.fail_auth();
            ssh_error(ErrorCode::Auth, message)
        };
        let key = remote.key(url).map_err(auth_error)?;
        let key_path = remote
            .key_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let key = russh::keys::decode_secret_key(&key.text, None)
            .map_err(|e| auth_error(format!("{key_path}: the SSH key cannot be decoded: {e}")))?;
        let user = remote_url::url_username(url)
            .unwrap_or(DEFAULT_SSH_USERNAME)
            .to_owned();
        let command = format!("{command} {}", shell_quote(&path));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(|e| {
                GitError::new(
                    ErrorCode::GenericError,
                    ErrorClass::Os,
                    format!("starting the SSH runtime: {e}"),
                )
            })?;
        let io_timeout = io_timeout();
        let connected = runtime.block_on(connect(
            remote, state, &endpoint, &user, key, &command, io_timeout,
        ));
        let (session, channel) = connected?;
        Ok(Self {
            runtime,
            session,
            channel,
            peer: format!("{user}@{lookup}"),
            command,
            io_timeout,
            buffer: Vec::new(),
            position: 0,
            eof: false,
            done: false,
            exit_status: None,
            stderr: Vec::new(),
        })
    }

    /// The command's failure, once the channel has ended: a non-zero exit
    /// status, with what it printed on stderr.
    fn failure(&self) -> Option<io::Error> {
        let stderr = String::from_utf8_lossy(&self.stderr);
        let stderr = stderr.trim();
        let status = match self.exit_status {
            Some(0) => return None,
            Some(status) => format!("exited with status {status}"),
            None if stderr.is_empty() => return None,
            None => "ended".to_owned(),
        };
        let said = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        };
        Some(io::Error::other(format!(
            "{} on {} {status}{said}",
            self.command, self.peer
        )))
    }

    /// The next message on the channel, waiting at most `wait`.
    fn next(&mut self, wait: Duration) -> io::Result<Option<ChannelMsg>> {
        let channel = &mut self.channel;
        match self
            .runtime
            .block_on(async { tokio::time::timeout(wait, channel.wait()).await })
        {
            Ok(msg) => Ok(msg),
            Err(_) if self.eof => {
                // The server sent everything and never closed: done.
                self.done = true;
                Ok(None)
            }
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "no data from {} for {} s ({})",
                    self.peer,
                    wait.as_secs(),
                    self.command
                ),
            )),
        }
    }
}

/// Connects, checks the host key, authenticates and starts `command`.
async fn connect(
    remote: &Arc<SshRemote>,
    state: &Arc<AuthState>,
    endpoint: &Endpoint,
    user: &str,
    key: PrivateKey,
    command: &str,
    io_timeout: Duration,
) -> Result<(Handle<Client>, Channel<client::Msg>), GitError> {
    let lookup = lookup_name(&endpoint.host, endpoint.port);
    let unreachable = |what: String| ssh_error(ErrorCode::GenericError, what);
    let timed_out = |what: &str, after: Duration| {
        ssh_error(
            ErrorCode::Timeout,
            format!("{what} {lookup}: no answer within {} s", after.as_secs()),
        )
    };
    let port = endpoint.port.unwrap_or(22);
    let tcp = tokio::time::timeout(
        connect_timeout(),
        tokio::net::TcpStream::connect((endpoint.host.as_str(), port)),
    )
    .await
    .map_err(|_| timed_out("connecting to", connect_timeout()))?
    .map_err(|e| unreachable(format!("connecting to {lookup}: {e}")))?;
    let _ = tcp.set_nodelay(true);
    let config = client::Config {
        preferred: Preferred {
            key: Cow::Owned(host_key_preference(&remote.known_key_types())),
            ..Preferred::default()
        },
        inactivity_timeout: None,
        nodelay: true,
        ..client::Config::default()
    };
    let handler = Client {
        remote: Arc::clone(remote),
        state: Arc::clone(state),
    };
    let handshake = tokio::time::timeout(
        io_timeout,
        client::connect_stream(Arc::new(config), tcp, handler),
    )
    .await
    .map_err(|_| timed_out("handshake with", io_timeout))?;
    let mut session = match handshake {
        Ok(session) => session,
        Err(e) => {
            if let Some(why) = state.host_key_failure() {
                return Err(ssh_error(ErrorCode::Certificate, why));
            }
            return Err(unreachable(format!("SSH handshake with {lookup}: {e}")));
        }
    };
    let fingerprint = format!(
        "SHA256:{}",
        STANDARD_NO_PAD.encode(key.public_key().fingerprint(HashAlg::Sha256).as_bytes())
    );
    let hash = if key.algorithm().is_rsa() {
        match session.best_supported_rsa_hash().await {
            Ok(Some(hash)) => hash,
            _ => Some(HashAlg::Sha256),
        }
    } else {
        None
    };
    let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
    let auth = tokio::time::timeout(io_timeout, session.authenticate_publickey(user, key))
        .await
        .map_err(|_| timed_out("authenticating to", io_timeout))?
        .map_err(|e| unreachable(format!("authenticating to {lookup}: {e}")))?;
    if !auth.success() {
        state.fail_auth();
        return Err(ssh_error(
            ErrorCode::Auth,
            format!(
                "{lookup} rejected the SSH key {fingerprint} of remote {} for user {user}",
                remote.name()
            ),
        ));
    }
    let start = async {
        let channel = session.channel_open_session().await?;
        channel.exec(true, command.as_bytes()).await?;
        Ok::<_, russh::Error>(channel)
    };
    let channel = tokio::time::timeout(io_timeout, start)
        .await
        .map_err(|_| timed_out("starting the git command on", io_timeout))?
        .map_err(|e| unreachable(format!("starting `{command}` on {lookup}: {e}")))?;
    Ok((session, channel))
}

impl Read for Stream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.position < self.buffer.len() {
                let n = out.len().min(self.buffer.len() - self.position);
                out[..n].copy_from_slice(&self.buffer[self.position..self.position + n]);
                self.position += n;
                return Ok(n);
            }
            if self.done {
                return match self.failure() {
                    Some(e) => Err(e),
                    None => Ok(0),
                };
            }
            let wait = if self.eof { EXIT_WAIT } else { self.io_timeout };
            match self.next(wait)? {
                Some(ChannelMsg::Data { data }) => {
                    self.buffer.clear();
                    self.buffer.extend_from_slice(&data);
                    self.position = 0;
                }
                Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                    let room = MAX_STDERR.saturating_sub(self.stderr.len());
                    self.stderr.extend_from_slice(&data[..data.len().min(room)]);
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    self.exit_status = Some(exit_status);
                    if self.eof {
                        self.done = true;
                    }
                }
                Some(ChannelMsg::Failure) => {
                    return Err(io::Error::other(format!(
                        "{} refused to run {}",
                        self.peer, self.command
                    )));
                }
                Some(ChannelMsg::Eof) => {
                    self.eof = true;
                    if self.exit_status.is_some() {
                        self.done = true;
                    }
                }
                Some(ChannelMsg::Close) | None => self.done = true,
                Some(_) => {}
            }
        }
    }
}

impl Write for Stream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let channel = &self.channel;
        let io_timeout = self.io_timeout;
        match self
            .runtime
            .block_on(async { tokio::time::timeout(io_timeout, channel.data(data)).await })
        {
            Ok(Ok(())) => Ok(data.len()),
            Ok(Err(e)) => Err(io::Error::other(format!(
                "sending to {} ({}): {e}",
                self.peer, self.command
            ))),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{} accepted no data for {} s ({})",
                    self.peer,
                    self.io_timeout.as_secs(),
                    self.command
                ),
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let channel = &self.channel;
        let session = &self.session;
        let _ = self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), async {
                let _ = channel.eof().await;
                let _ = session
                    .disconnect(Disconnect::ByApplication, "", "en")
                    .await;
            })
            .await
        });
    }
}

#[cfg(test)]
mod tests;
