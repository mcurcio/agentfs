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
//! 1. reads the key from `agentfsSshKey`, decrypted with the passphrase in
//!    `agentfsSshKeyPassphrase` if it is encrypted (a key that cannot be
//!    read, decoded or decrypted is an `auth` failure, and nothing is sent);
//! 2. connects within [`SERVER_CONNECT_TIMEOUT_MS`], asking for host
//!    certificates first if a `@cert-authority` line of `agentfsKnownHosts`
//!    matches the host, then the host key types it lists for the host, as
//!    OpenSSH does;
//! 3. checks the host key or certificate with
//!    [`SshRemote`](crate::remote_auth::SshRemote) before any
//!    authentication, recording a refusal in the connection's
//!    [`AuthState`];
//! 4. authenticates as the URL's user (else `git`): offers the key once,
//!    or, with `agentfsSshAgent`, asks that agent for its identities and
//!    offers each once, in the agent's order, until one is accepted (the
//!    environment's `SSH_AUTH_SOCK` is never read);
//! 5. runs the git command, with every read and write bounded by
//!    [`SERVER_IO_TIMEOUT_MS`] (libgit2's own timeouts do not apply to
//!    custom transports).
//!
//! The SSH session runs on a current-thread tokio runtime owned by the
//! connection's stream and driven only while libgit2 reads or writes.
//!
//! [`run_command`] runs one other command the same way, bound to the same
//! remote's host and port with the same checks, and returns its output:
//! `git-lfs-authenticate` for the LFS client (design §7).

use std::borrow::Cow;
use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use git2::transport::{Service, SmartSubtransport, SmartSubtransportStream, Transport};
use git2::{Error as GitError, ErrorClass, ErrorCode};
use russh::client::{self, Handle};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{
    Algorithm, Certificate, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey,
    PublicKeyOrCertificate,
};
use russh::{Channel, ChannelMsg, Disconnect, Preferred};
use tokio::runtime::Runtime;

use crate::credentials::DEFAULT_SSH_USERNAME;
use crate::history::{SERVER_CONNECT_TIMEOUT_MS, SERVER_IO_TIMEOUT_MS};
use crate::known_hosts::{HostCertificate, fingerprint, lookup_name};
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
            PublicKeyOrCertificate::Certificate(cert) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                self.remote
                    .check_host_certificate(&host, &host_certificate(cert), now)
            }
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

/// The SSH wire-format blob of a public key.
fn key_blob(key: &russh::keys::ssh_key::public::KeyData) -> Vec<u8> {
    PublicKey::new(key.clone(), "")
        .to_bytes()
        .unwrap_or_default()
}

/// What [`crate::known_hosts`] checks of a host certificate.
fn host_certificate(cert: &Certificate) -> HostCertificate {
    HostCertificate {
        key_type: cert.algorithm().to_certificate_type(),
        is_host: cert.cert_type().is_host(),
        key: key_blob(cert.public_key()),
        ca_key: key_blob(cert.signature_key()),
        signature_valid: cert.verify_signature().is_ok(),
        principals: cert.valid_principals().to_vec(),
        valid_after: cert.valid_after(),
        valid_before: cert.valid_before(),
        critical_options: cert.critical_options().keys().cloned().collect(),
    }
}

/// How a connection authenticates.
enum Credentials {
    /// The remote's key, decoded.
    Key(Arc<PrivateKey>),
    /// The identities of the ssh-agent at this socket.
    Agent(std::path::PathBuf),
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

/// A connected session with one command started on it.
struct Opened {
    runtime: Runtime,
    session: Handle<Client>,
    channel: Channel<client::Msg>,
    /// `user@host:port`, for messages.
    peer: String,
    io_timeout: Duration,
}

/// Connects to `url` (which must be on `remote`'s push URL host and port),
/// checks the host, authenticates and starts the command `command` makes
/// of the URL's path.
fn open_command(
    remote: &Arc<SshRemote>,
    state: &Arc<AuthState>,
    url: &str,
    command: impl FnOnce(&str) -> String,
) -> Result<(Opened, String), GitError> {
    let (Some(endpoint), Some(path)) = (remote_url::ssh_endpoint(url), remote_url::ssh_path(url))
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
    let credentials = match remote.agent() {
        Some(socket) => Credentials::Agent(socket.to_path_buf()),
        None => Credentials::Key(remote.private_key(url).map_err(auth_error)?),
    };
    let user = remote_url::url_username(url)
        .unwrap_or(DEFAULT_SSH_USERNAME)
        .to_owned();
    let command = command(&path);
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
        remote,
        state,
        &endpoint,
        &user,
        credentials,
        &command,
        io_timeout,
    ));
    let (session, channel) = connected?;
    Ok((
        Opened {
            runtime,
            session,
            channel,
            peer: format!("{user}@{lookup}"),
            io_timeout,
        },
        command,
    ))
}

impl Stream {
    fn open(context: &Context, url: &str, command: &str) -> Result<Self, GitError> {
        let Context { remote, state } = context;
        let (opened, command) = open_command(remote, state, url, |path| {
            format!("{command} {}", shell_quote(path))
        })?;
        let Opened {
            runtime,
            session,
            channel,
            peer,
            io_timeout,
        } = opened;
        Ok(Self {
            runtime,
            session,
            channel,
            peer,
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

/// What a command run with [`run_command`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandOutput {
    /// The exit status, if the server sent one.
    pub status: Option<u32>,
    pub stdout: Vec<u8>,
    /// Up to [`MAX_STDERR`] bytes of stderr.
    pub stderr: String,
}

/// Runs `command` (sent as given: the caller quotes it) on `remote`'s
/// server over `url`, which must be on its push URL's host and port, with
/// the transport's checks: the host key or certificate before anything is
/// offered, then the remote's key or agent, as the URL's user, within the
/// transport's timeouts. More than `max_stdout` bytes of output is an
/// error. Refusals are recorded in `state`, as for git commands.
pub(crate) fn run_command(
    remote: &Arc<SshRemote>,
    state: &Arc<AuthState>,
    url: &str,
    command: &str,
    max_stdout: usize,
) -> Result<CommandOutput, GitError> {
    let (opened, command) = open_command(remote, state, url, |_| command.to_owned())?;
    let Opened {
        runtime,
        session,
        mut channel,
        peer,
        io_timeout,
    } = opened;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    let mut eof = false;
    let result = loop {
        let wait = if eof { EXIT_WAIT } else { io_timeout };
        let channel = &mut channel;
        let msg = runtime.block_on(async { tokio::time::timeout(wait, channel.wait()).await });
        match msg {
            Err(_) if eof => break Ok(()),
            Err(_) => {
                break Err(ssh_error(
                    ErrorCode::Timeout,
                    format!("no answer from {peer} for {} s ({command})", wait.as_secs()),
                ));
            }
            Ok(None) | Ok(Some(ChannelMsg::Close)) => break Ok(()),
            Ok(Some(ChannelMsg::Data { data })) => {
                if stdout.len() + data.len() > max_stdout {
                    break Err(ssh_error(
                        ErrorCode::GenericError,
                        format!("{command} on {peer} printed more than {max_stdout} bytes"),
                    ));
                }
                stdout.extend_from_slice(&data);
            }
            Ok(Some(ChannelMsg::ExtendedData { data, ext: 1 })) => {
                let room = MAX_STDERR.saturating_sub(stderr.len());
                stderr.extend_from_slice(&data[..data.len().min(room)]);
            }
            Ok(Some(ChannelMsg::ExitStatus { exit_status })) => {
                status = Some(exit_status);
                if eof {
                    break Ok(());
                }
            }
            Ok(Some(ChannelMsg::Eof)) => {
                eof = true;
                if status.is_some() {
                    break Ok(());
                }
            }
            Ok(Some(ChannelMsg::Failure)) => {
                break Err(ssh_error(
                    ErrorCode::GenericError,
                    format!("{peer} refused to run {command}"),
                ));
            }
            Ok(Some(_)) => {}
        }
    };
    disconnect(&runtime, &session, &channel);
    result.map(|()| CommandOutput {
        status,
        stdout,
        stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
    })
}

/// Ends a session politely, waiting at most a second.
fn disconnect(runtime: &Runtime, session: &Handle<Client>, channel: &Channel<client::Msg>) {
    let _ = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), async {
            let _ = channel.eof().await;
            let _ = session
                .disconnect(Disconnect::ByApplication, "", "en")
                .await;
        })
        .await
    });
}

/// Connects, checks the host key, authenticates and starts `command`.
async fn connect(
    remote: &Arc<SshRemote>,
    state: &Arc<AuthState>,
    endpoint: &Endpoint,
    user: &str,
    credentials: Credentials,
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
            host_key_certificates: if remote.has_cert_authority() {
                Preferred::DEFAULT.key
            } else {
                Cow::Borrowed(&[])
            },
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
    let accepted = match credentials {
        Credentials::Key(key) => {
            authenticate_key(&mut session, remote, &lookup, user, key, io_timeout).await?
        }
        Credentials::Agent(socket) => {
            authenticate_agent(&mut session, remote, &lookup, user, &socket, io_timeout).await?
        }
    };
    if let Err(why) = accepted {
        state.fail_auth();
        return Err(ssh_error(ErrorCode::Auth, why));
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

/// `SHA256:<base64>` of a public key, as `ssh-keygen -l` prints it.
fn public_fingerprint(key: &PublicKey) -> String {
    fingerprint(&key.to_bytes().unwrap_or_default())
}

/// The RSA signature hash to use with `algorithm`, if it is RSA: the best
/// the server supports, else SHA-256.
async fn rsa_hash(session: &mut Handle<Client>, algorithm: &Algorithm) -> Option<HashAlg> {
    if !algorithm.clone().is_rsa() {
        return None;
    }
    match session.best_supported_rsa_hash().await {
        Ok(Some(hash)) => hash,
        _ => Some(HashAlg::Sha256),
    }
}

fn auth_timeout(lookup: &str, after: Duration) -> GitError {
    ssh_error(
        ErrorCode::Timeout,
        format!(
            "authenticating to {lookup}: no answer within {} s",
            after.as_secs()
        ),
    )
}

/// Offers the remote's key once. The inner error is the server's refusal.
async fn authenticate_key(
    session: &mut Handle<Client>,
    remote: &SshRemote,
    lookup: &str,
    user: &str,
    key: Arc<PrivateKey>,
    io_timeout: Duration,
) -> Result<Result<(), String>, GitError> {
    let fingerprint = public_fingerprint(key.public_key());
    let hash = rsa_hash(session, &key.algorithm()).await;
    let key = PrivateKeyWithHashAlg::new(key, hash);
    let auth = tokio::time::timeout(io_timeout, session.authenticate_publickey(user, key))
        .await
        .map_err(|_| auth_timeout(lookup, io_timeout))?
        .map_err(|e| {
            ssh_error(
                ErrorCode::GenericError,
                format!("authenticating to {lookup}: {e}"),
            )
        })?;
    if auth.success() {
        return Ok(Ok(()));
    }
    Ok(Err(format!(
        "{lookup} rejected the SSH key {fingerprint} of remote {} for user {user}",
        remote.name()
    )))
}

/// Offers each identity of the ssh-agent at `socket` once, in the agent's
/// order, until the server accepts one. The inner error says why none was
/// accepted: the agent could not be asked, held no identity, or every
/// identity was refused (listed by fingerprint).
async fn authenticate_agent(
    session: &mut Handle<Client>,
    remote: &SshRemote,
    lookup: &str,
    user: &str,
    socket: &std::path::Path,
    io_timeout: Duration,
) -> Result<Result<(), String>, GitError> {
    let name = remote.name();
    let agent_error = |e: &dyn std::fmt::Display| {
        format!(
            "ssh-agent at {} (remote.{name}.agentfsSshAgent): {e}",
            socket.display()
        )
    };
    let asked = tokio::time::timeout(io_timeout, async {
        let mut agent = AgentClient::connect_uds(socket).await?;
        let identities = agent.request_identities().await?;
        Ok::<_, russh::keys::Error>((agent, identities))
    })
    .await;
    let (mut agent, identities) = match asked {
        Ok(Ok(asked)) => asked,
        Ok(Err(e)) => return Ok(Err(agent_error(&e))),
        Err(_) => return Ok(Err(agent_error(&"no answer within the I/O timeout"))),
    };
    if identities.is_empty() {
        return Ok(Err(agent_error(&"the agent holds no identity")));
    }
    let mut offered = Vec::new();
    for identity in identities {
        let (public, comment) = match &identity {
            AgentIdentity::PublicKey { key, comment } => (key.clone(), comment.clone()),
            AgentIdentity::Certificate {
                certificate,
                comment,
            } => (
                PublicKey::new(certificate.public_key().clone(), ""),
                comment.clone(),
            ),
        };
        let hash = rsa_hash(session, &public.algorithm()).await;
        let attempt = async {
            match identity {
                AgentIdentity::PublicKey { key, .. } => {
                    session
                        .authenticate_publickey_with(user, key, hash, &mut agent)
                        .await
                }
                AgentIdentity::Certificate { certificate, .. } => {
                    session
                        .authenticate_certificate_with(user, certificate, hash, &mut agent)
                        .await
                }
            }
        };
        let auth = match tokio::time::timeout(io_timeout, attempt).await {
            Err(_) => return Err(auth_timeout(lookup, io_timeout)),
            Ok(Err(russh::AgentAuthError::Send(e))) => {
                return Err(ssh_error(
                    ErrorCode::GenericError,
                    format!("authenticating to {lookup}: {e}"),
                ));
            }
            Ok(Err(russh::AgentAuthError::Key(e))) => return Ok(Err(agent_error(&e))),
            Ok(Ok(auth)) => auth,
        };
        if auth.success() {
            return Ok(Ok(()));
        }
        let comment = if comment.is_empty() {
            String::new()
        } else {
            format!(" ({comment})")
        };
        offered.push(format!("{}{comment}", public_fingerprint(&public)));
    }
    Ok(Err(format!(
        "{lookup} rejected every identity of the ssh-agent at {} for remote {name}, user \
         {user}: {}",
        socket.display(),
        offered.join(", ")
    )))
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
        disconnect(&self.runtime, &self.session, &self.channel);
    }
}

#[cfg(test)]
mod tests;
