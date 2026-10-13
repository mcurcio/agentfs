//! The SSH transport against an in-process russh server that runs the real
//! `git upload-pack` and `git receive-pack` (design §18): what CI's
//! e2e-ssh job checks against OpenSSH, here without sshd.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use git2::{PushOptions, Repository, Signature};
use russh::keys::PublicKey;
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::server::{self, Auth, Msg, Session};
use russh::{ChannelId, MethodKind, MethodSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::config::RemoteSettings;
use crate::events::PushFailureKind;
use crate::history::init_libgit2;
use crate::push::classify;
use crate::remote_auth::RemoteAuth;

/// A deterministic Ed25519 key.
fn ed25519(seed: u8) -> PrivateKey {
    PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]))
}

fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// How the test server behaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Runs git.
    Git,
    /// Accepts the command and never answers.
    Stall,
}

/// What the server saw.
#[derive(Default)]
struct Log {
    /// Fingerprints of the keys offered, in order.
    offered: Vec<String>,
    accepted: Vec<String>,
    commands: Vec<String>,
}

struct TestServer {
    port: u16,
    host_key: PublicKey,
    log: Arc<Mutex<Log>>,
    _runtime: tokio::runtime::Runtime,
}

impl TestServer {
    /// A server on 127.0.0.1 with host key `host`, accepting `authorized`
    /// for user `git`.
    fn start(host: PrivateKey, authorized: Vec<PublicKey>, mode: Mode) -> Self {
        Self::start_with_certificates(host, Vec::new(), authorized, mode)
    }

    /// [`TestServer::start`], also presenting host `certificates` to
    /// clients that ask for them.
    fn start_with_certificates(
        host: PrivateKey,
        certificates: Vec<Certificate>,
        authorized: Vec<PublicKey>,
        mode: Mode,
    ) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let host_key = host.public_key().clone();
        let config = Arc::new(server::Config {
            keys: vec![host],
            certificates,
            methods: MethodSet::from(&[MethodKind::PublicKey][..]),
            auth_rejection_time: Duration::from_millis(1),
            auth_rejection_time_initial: Some(Duration::from_millis(1)),
            ..server::Config::default()
        });
        let log = Arc::new(Mutex::new(Log::default()));
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let authorized = Arc::new(authorized);
        let server_log = Arc::clone(&log);
        runtime.spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let handler = Conn {
                    authorized: Arc::clone(&authorized),
                    log: Arc::clone(&server_log),
                    mode,
                    channels: HashMap::new(),
                };
                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    if let Ok(session) = server::run_stream(config, socket, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        Self {
            port,
            host_key,
            log,
            _runtime: runtime,
        }
    }

    fn url(&self, path: &Path) -> String {
        format!("ssh://git@127.0.0.1:{}{}", self.port, path.display())
    }

    fn known_hosts_line(&self) -> String {
        format!(
            "[127.0.0.1]:{} {}",
            self.port,
            self.host_key.to_openssh().unwrap()
        )
    }

    fn offered(&self) -> Vec<String> {
        self.log.lock().unwrap().offered.clone()
    }
}

struct Conn {
    authorized: Arc<Vec<PublicKey>>,
    log: Arc<Mutex<Log>>,
    mode: Mode,
    channels: HashMap<ChannelId, russh::Channel<Msg>>,
}

impl server::Handler for Conn {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.log.lock().unwrap().offered.push(fingerprint(key));
        Ok(Auth::Accept)
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        let known = self
            .authorized
            .iter()
            .any(|k| k.key_data() == key.key_data());
        if user == "git" && known {
            self.log.lock().unwrap().accepted.push(fingerprint(key));
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        self.log.lock().unwrap().commands.push(command.clone());
        let Some(channel) = self.channels.remove(&id) else {
            return Ok(());
        };
        session.channel_success(id)?;
        if self.mode == Mode::Stall {
            tokio::spawn(async move {
                let _keep = channel;
                tokio::time::sleep(Duration::from_secs(3600)).await;
            });
            return Ok(());
        }
        tokio::spawn(run_git(channel, command));
        Ok(())
    }
}

/// `git-upload-pack '<path>'` as `git upload-pack <path>`, piped through
/// the channel, as sshd and git-shell run it.
async fn run_git(channel: russh::Channel<Msg>, command: String) {
    let (verb, quoted) = command.split_once(' ').unwrap();
    let path = quoted
        .strip_prefix('\'')
        .and_then(|p| p.strip_suffix('\''))
        .unwrap()
        .replace("'\\''", "'");
    let mut child = tokio::process::Command::new("git")
        .arg(verb.strip_prefix("git-").unwrap())
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (mut read, write) = channel.split();
    let input = tokio::spawn(async move {
        while let Some(msg) = read.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    if stdin.write_all(&data).await.is_err() {
                        break;
                    }
                }
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
    });
    let mut out = vec![0u8; 32 << 10];
    loop {
        let n = stdout.read(&mut out).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        if write.data(&out[..n]).await.is_err() {
            break;
        }
    }
    let mut err = Vec::new();
    let _ = stderr.read_to_end(&mut err).await;
    if !err.is_empty() {
        let _ = write.extended_data(1, &err[..]).await;
    }
    let status = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(1);
    input.abort();
    let _ = write.exit_status(status as u32).await;
    let _ = write.eof().await;
    let _ = write.close().await;
}

/// A client side: a repository with one commit, the remote's settings and
/// a bare repository the server serves.
struct Fixture {
    dir: tempfile::TempDir,
    repo: Repository,
    head: git2::Oid,
    served: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        init_libgit2();
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path().join("local.git")).unwrap();
        let blob = repo.blob(b"hello\n").unwrap();
        let head = {
            let mut builder = repo.treebuilder(None).unwrap();
            builder.insert("a.txt", blob, 0o100644).unwrap();
            let tree = repo.find_tree(builder.write().unwrap()).unwrap();
            let sig = Signature::now("t", "t@example.com").unwrap();
            repo.commit(Some("refs/heads/main"), &sig, &sig, "one", &tree, &[])
                .unwrap()
        };
        let served = dir.path().join("served.git");
        Repository::init_bare(&served).unwrap();
        Self {
            dir,
            repo,
            head,
            served,
        }
    }

    /// Settings for a remote at `url` with `key` and `known_hosts` lines.
    fn remote(&self, url: &str, key: &PrivateKey, known_hosts: &str) -> RemoteSettings {
        let key_path = self.dir.path().join("id");
        fs::write(
            &key_path,
            key.to_openssh(LineEnding::LF).unwrap().as_bytes(),
        )
        .unwrap();
        let kh = self.dir.path().join("known_hosts");
        fs::write(&kh, known_hosts).unwrap();
        RemoteSettings {
            name: "origin".into(),
            url: url.into(),
            push_url: None,
            max_blob_size: None,
            max_push_size: None,
            credentials: None,
            ssh_key: Some(key_path),
            known_hosts: Some(kh),
            lfs_url: None,
            lfs_push_url: None,
            lfs_credentials: None,
            ssh_agent: None,
            ssh_key_passphrase: None,
        }
    }

    /// Pushes `main` to `url` in `settings`' scope.
    fn push(&self, settings: &RemoteSettings) -> (Result<(), GitError>, Arc<AuthState>) {
        let auth = RemoteAuth::new(settings);
        let state = AuthState::new();
        let _scope = auth.ssh_scope(&state);
        let result = (|| {
            let mut remote = self.repo.remote_anonymous(settings.push_url())?;
            let mut options = PushOptions::new();
            options.remote_callbacks(auth.callbacks(&state));
            remote.push(&["refs/heads/main:refs/heads/main"], Some(&mut options))
        })();
        (result, state)
    }

    fn served_head(&self) -> Option<git2::Oid> {
        let served = Repository::open_bare(&self.served).unwrap();
        served.refname_to_id("refs/heads/main").ok()
    }
}

#[test]
fn quoting_and_preference() {
    assert_eq!(shell_quote("/srv/r.git"), "'/srv/r.git'");
    assert_eq!(shell_quote("it's!"), "'it'\\''s'\\!''");
    let listed = ["ssh-rsa".to_owned(), "ecdsa-sha2-nistp256".to_owned()];
    let order = host_key_preference(&listed);
    assert_eq!(
        order[..3],
        [
            Algorithm::Rsa {
                hash: Some(HashAlg::Sha512)
            },
            Algorithm::Rsa {
                hash: Some(HashAlg::Sha256)
            },
            Algorithm::Ecdsa {
                curve: russh::keys::EcdsaCurve::NistP256
            },
        ]
    );
    // Everything russh supports follows, once.
    assert_eq!(order.len(), Preferred::DEFAULT.key.len());
    assert_eq!(host_key_preference(&[]), Preferred::DEFAULT.key.to_vec());
    assert_eq!(
        host_key_preference(&["ssh-dss".to_owned(), "junk".to_owned()]),
        Preferred::DEFAULT.key.to_vec()
    );
}

#[test]
fn push_and_fetch_over_ssh() {
    let f = Fixture::new();
    let client = ed25519(1);
    let server = TestServer::start(ed25519(9), vec![client.public_key().clone()], Mode::Git);
    let url = server.url(&f.served);
    let settings = f.remote(&url, &client, &server.known_hosts_line());
    let (result, state) = f.push(&settings);
    result.unwrap();
    assert!(!state.auth_failed());
    assert_eq!(f.served_head(), Some(f.head));
    assert_eq!(server.offered(), [fingerprint(client.public_key())]);
    let commands = server.log.lock().unwrap().commands.clone();
    assert_eq!(
        commands,
        [format!("git-receive-pack '{}'", f.served.display())]
    );

    // Up to date: a second push connects again and sends nothing.
    f.push(&settings).0.unwrap();

    // Fetch it back into another repository.
    let other = Repository::init_bare(f.dir.path().join("other.git")).unwrap();
    let auth = RemoteAuth::new(&settings);
    let state = AuthState::new();
    {
        let _scope = auth.ssh_scope(&state);
        let mut remote = other.remote_anonymous(&url).unwrap();
        let mut options = git2::FetchOptions::new();
        options.remote_callbacks(auth.callbacks(&state));
        remote
            .fetch(&["+refs/heads/*:refs/heads/*"], Some(&mut options), None)
            .unwrap();
    }
    assert_eq!(other.refname_to_id("refs/heads/main").unwrap(), f.head);
}

#[test]
fn scp_like_urls_work() {
    let f = Fixture::new();
    let client = ed25519(1);
    let server = TestServer::start(ed25519(9), vec![client.public_key().clone()], Mode::Git);
    let url = format!("[git@127.0.0.1:{}]:{}", server.port, f.served.display());
    let settings = f.remote(&url, &client, &server.known_hosts_line());
    f.push(&settings).0.unwrap();
    assert_eq!(f.served_head(), Some(f.head));
}

#[test]
fn untrusted_hosts_never_see_the_key() {
    let f = Fixture::new();
    let client = ed25519(1);
    let server = TestServer::start(ed25519(9), vec![client.public_key().clone()], Mode::Git);
    let url = server.url(&f.served);
    let changed = format!(
        "[127.0.0.1]:{} {}",
        server.port,
        ed25519(8).public_key().to_openssh().unwrap()
    );
    let unknown = server.known_hosts_line().replace("127.0.0.1", "otherhost");
    for (known_hosts, says) in [
        (changed, "does not match"),
        (unknown, "is unknown"),
        (String::new(), "is unknown"),
    ] {
        let settings = f.remote(&url, &client, &known_hosts);
        let (result, state) = f.push(&settings);
        let err = result.unwrap_err();
        let why = state.host_key_failure().expect("host key refused");
        assert!(why.contains(says), "{why}");
        assert!(why.contains(&fingerprint(&server.host_key)), "{why}");
        assert!(
            why.contains(&format!("[127.0.0.1]:{}", server.port)),
            "{why}"
        );
        assert_eq!(err.code(), ErrorCode::Certificate, "{err}");
        assert_eq!(classify(&err, false), PushFailureKind::UntrustedHost);
    }
    assert!(server.offered().is_empty(), "{:?}", server.offered());
    assert_eq!(f.served_head(), None);
}

#[test]
fn a_refused_key_is_an_auth_failure_offered_once() {
    let f = Fixture::new();
    let wrong = ed25519(2);
    let server = TestServer::start(ed25519(9), vec![ed25519(1).public_key().clone()], Mode::Git);
    let settings = f.remote(&server.url(&f.served), &wrong, &server.known_hosts_line());
    let (result, state) = f.push(&settings);
    let err = result.unwrap_err();
    assert!(state.auth_failed());
    assert_eq!(classify(&err, false), PushFailureKind::Auth);
    assert!(err.message().contains("rejected the SSH key"), "{err}");
    assert!(
        err.message().contains(&fingerprint(wrong.public_key())),
        "{err}"
    );
    assert_eq!(server.offered(), [fingerprint(wrong.public_key())]);
    assert_eq!(f.served_head(), None);
}

#[test]
fn an_unreadable_key_is_an_auth_failure_without_connecting() {
    let f = Fixture::new();
    let server = TestServer::start(ed25519(9), vec![], Mode::Git);
    let settings = f.remote(
        &server.url(&f.served),
        &ed25519(1),
        &server.known_hosts_line(),
    );
    fs::write(settings.ssh_key.as_ref().unwrap(), "ssh-ed25519 AAAA x\n").unwrap();
    let (result, state) = f.push(&settings);
    let err = result.unwrap_err();
    assert!(state.auth_failed());
    assert!(err.message().contains("no private key"), "{err}");
    assert!(server.offered().is_empty());
    assert!(server.log.lock().unwrap().commands.is_empty());
}

#[test]
fn a_missing_repository_reports_the_server_error() {
    let f = Fixture::new();
    let client = ed25519(1);
    let server = TestServer::start(ed25519(9), vec![client.public_key().clone()], Mode::Git);
    let missing = f.dir.path().join("missing.git");
    let settings = f.remote(&server.url(&missing), &client, &server.known_hosts_line());
    let err = f.push(&settings).0.unwrap_err();
    assert!(err.message().contains("git-receive-pack"), "{err}");
    assert_eq!(classify(&err, false), PushFailureKind::Unreachable);
}

#[test]
fn a_stalled_server_times_out_as_unreachable() {
    let f = Fixture::new();
    let client = ed25519(1);
    let server = TestServer::start(ed25519(9), vec![client.public_key().clone()], Mode::Stall);
    let settings = f.remote(&server.url(&f.served), &client, &server.known_hosts_line());
    set_io_timeout_for_this_thread(Some(Duration::from_millis(500)));
    let started = std::time::Instant::now();
    let err = f.push(&settings).0.unwrap_err();
    set_io_timeout_for_this_thread(None);
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(err.message().contains("no data from"), "{err}");
    assert_eq!(classify(&err, false), PushFailureKind::Unreachable);
}

#[test]
fn connections_outside_a_remote_scope_fail_closed() {
    let f = Fixture::new();
    let client = ed25519(1);
    let server = TestServer::start(ed25519(9), vec![client.public_key().clone()], Mode::Git);
    let url = server.url(&f.served);
    // No scope at all.
    let mut remote = f.repo.remote_anonymous(&url).unwrap();
    let err = remote
        .push(&["refs/heads/main:refs/heads/main"], None)
        .unwrap_err();
    assert!(err.message().contains("no remote's SSH settings"), "{err}");
    // A scope for another host or port.
    let settings = f.remote(&url, &client, &server.known_hosts_line());
    let other = server.url(&f.served).replace("127.0.0.1", "localhost");
    let auth = RemoteAuth::new(&settings);
    let state = AuthState::new();
    {
        let _scope = auth.ssh_scope(&state);
        let mut remote = f.repo.remote_anonymous(&other).unwrap();
        let err = remote
            .push(&["refs/heads/main:refs/heads/main"], None)
            .unwrap_err();
        assert!(err.message().contains("nothing is offered"), "{err}");
    }
    // A local path with ':' reaches the SSH transport in libgit2; it is
    // refused there.
    let colon = f.dir.path().join("a:b.git");
    Repository::init_bare(&colon).unwrap();
    {
        let _scope = auth.ssh_scope(&state);
        let mut remote = f
            .repo
            .remote_anonymous(&colon.display().to_string())
            .unwrap();
        let err = remote
            .push(&["refs/heads/main:refs/heads/main"], None)
            .unwrap_err();
        assert!(err.message().contains("not an SSH URL"), "{err}");
    }
    assert!(server.offered().is_empty());
    assert_eq!(f.served_head(), None);
}

#[test]
fn scopes_nest_and_restore() {
    let f = Fixture::new();
    let settings = f.remote("ssh://git@h/r.git", &ed25519(1), "");
    let auth = RemoteAuth::new(&settings);
    let (a, b) = (AuthState::new(), AuthState::new());
    let current = || CURRENT.with(|c| c.borrow().as_ref().map(|c| Arc::as_ptr(&c.state)));
    assert_eq!(current(), None);
    let outer = auth.ssh_scope(&a);
    assert_eq!(current(), Some(Arc::as_ptr(&a)));
    {
        let _inner = auth.ssh_scope(&b);
        assert_eq!(current(), Some(Arc::as_ptr(&b)));
    }
    assert_eq!(current(), Some(Arc::as_ptr(&a)));
    drop(outer);
    assert_eq!(current(), None);
}

#[test]
fn the_patch_queue_fetches_its_inbox_over_ssh() {
    let f = Fixture::new();
    let client = ed25519(1);
    let server = TestServer::start(ed25519(9), vec![client.public_key().clone()], Mode::Git);
    let settings = f.remote(&server.url(&f.served), &client, &server.known_hosts_line());
    f.push(&settings).0.unwrap();
    let served = Repository::open_bare(&f.served).unwrap();
    served
        .reference(&crate::patch::inbox_ref("vol", 1), f.head, false, "a patch")
        .unwrap();
    let outcome =
        crate::patch::fetch::fetch_inbox(f.repo.path(), &settings, "vol", Duration::from_secs(60));
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.remote_inbox, 1);
    assert_eq!(
        f.repo
            .refname_to_id(&crate::patch::inbox_ref("vol", 1))
            .unwrap(),
        f.head
    );
    // An untrusted host fails it the same way as a push.
    let settings = f.remote(&server.url(&f.served), &client, "");
    let outcome =
        crate::patch::fetch::fetch_inbox(f.repo.path(), &settings, "vol", Duration::from_secs(60));
    assert_eq!(
        outcome.kind,
        Some(PushFailureKind::UntrustedHost),
        "{outcome:?}"
    );
}

/// Test-only randomness for key encryption: a counter, not random.
struct CounterRng(u64);

impl russh::keys::ssh_key::rand_core::TryRng for CounterRng {
    type Error = std::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        self.try_next_u64().map(|v| v as u32)
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        Ok(self.0)
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        for b in dst {
            *b = self.try_next_u64()? as u8;
        }
        Ok(())
    }
}

impl russh::keys::ssh_key::rand_core::TryCryptoRng for CounterRng {}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A host certificate of `host`'s key for `principal`, signed by `ca`.
fn host_cert(
    host: &PrivateKey,
    ca: &PrivateKey,
    principal: &str,
    after: u64,
    before: u64,
) -> Certificate {
    use russh::keys::ssh_key::certificate::{Builder, CertType};
    let mut builder = Builder::new(
        [7u8; 16].to_vec(),
        host.public_key().key_data().clone(),
        after,
        before,
    )
    .unwrap();
    builder.cert_type(CertType::Host).unwrap();
    builder.key_id("test host").unwrap();
    builder.valid_principal(principal).unwrap();
    builder.sign(ca).unwrap()
}

#[test]
fn host_certificates_are_checked_against_cert_authorities() {
    let f = Fixture::new();
    let client = ed25519(1);
    let (host, ca, other_ca) = (ed25519(9), ed25519(20), ed25519(21));
    let ca_line = |ca: &PrivateKey, port: u16| {
        format!(
            "@cert-authority [127.0.0.1]:{port} {}",
            ca.public_key().to_openssh().unwrap()
        )
    };

    // A valid certificate, with only the CA in known_hosts.
    let server = TestServer::start_with_certificates(
        host.clone(),
        vec![host_cert(
            &host,
            &ca,
            "127.0.0.1",
            now() - 300,
            now() + 7200,
        )],
        vec![client.public_key().clone()],
        Mode::Git,
    );
    let settings = f.remote(&server.url(&f.served), &client, &ca_line(&ca, server.port));
    let (result, state) = f.push(&settings);
    result.unwrap();
    assert!(state.host_key_failure().is_none());
    assert_eq!(f.served_head(), Some(f.head));
    // A CA from another line is unknown.
    let settings = f.remote(
        &server.url(&f.served),
        &client,
        &ca_line(&other_ca, server.port),
    );
    let (result, state) = f.push(&settings);
    let err = result.unwrap_err();
    assert_eq!(classify(&err, false), PushFailureKind::UntrustedHost);
    let why = state.host_key_failure().unwrap();
    assert!(why.contains("no @cert-authority line"), "{why}");
    assert!(why.contains(&fingerprint(ca.public_key())), "{why}");
    // A @revoked CA.
    let revoked = format!(
        "{}\n@revoked * {}\n",
        ca_line(&ca, server.port),
        ca.public_key().to_openssh().unwrap()
    );
    let settings = f.remote(&server.url(&f.served), &client, &revoked);
    let (_, state) = f.push(&settings);
    assert!(
        state.host_key_failure().unwrap().contains("@revoked"),
        "{:?}",
        state.host_key_failure()
    );
    // A plain key line still works when the server has a certificate too.
    let settings = f.remote(&server.url(&f.served), &client, &server.known_hosts_line());
    f.push(&settings).0.unwrap();
    assert_eq!(server.offered().len(), 2, "{:?}", server.offered());

    // Expired, and for another host.
    for (cert, says) in [
        (
            host_cert(&host, &ca, "127.0.0.1", now() - 7200, now() - 3600),
            "expired",
        ),
        (
            host_cert(&host, &ca, "otherhost", now() - 300, now() + 7200),
            "is for otherhost, not 127.0.0.1",
        ),
    ] {
        let server = TestServer::start_with_certificates(
            host.clone(),
            vec![cert],
            vec![client.public_key().clone()],
            Mode::Git,
        );
        let settings = f.remote(&server.url(&f.served), &client, &ca_line(&ca, server.port));
        let (result, state) = f.push(&settings);
        let err = result.unwrap_err();
        assert_eq!(err.code(), ErrorCode::Certificate, "{err}");
        assert_eq!(classify(&err, false), PushFailureKind::UntrustedHost);
        let why = state.host_key_failure().unwrap();
        assert!(why.contains(says), "{why}");
        assert!(why.contains("ssh-keygen -L -f"), "{why}");
        assert!(server.offered().is_empty(), "{:?}", server.offered());
    }
}

#[test]
fn encrypted_keys_are_decrypted_with_their_passphrase() {
    let f = Fixture::new();
    let client = ed25519(1);
    let server = TestServer::start(ed25519(9), vec![client.public_key().clone()], Mode::Git);
    let mut settings = f.remote(&server.url(&f.served), &client, &server.known_hosts_line());
    let key_path = settings.ssh_key.clone().unwrap();
    let openssh = client
        .encrypt(&mut CounterRng(1), "two words")
        .unwrap()
        .to_openssh(LineEnding::LF)
        .unwrap()
        .to_string();
    let mut pkcs8 = Vec::new();
    russh::keys::encode_pkcs8_pem_encrypted(&client, b"two words", 16, &mut pkcs8).unwrap();
    let pass = f.dir.path().join("passphrase");
    for text in [openssh.into_bytes(), pkcs8] {
        fs::write(&key_path, &text).unwrap();
        let offered = server.offered().len();
        // Without a passphrase: refused before connecting.
        settings.ssh_key_passphrase = None;
        let (result, state) = f.push(&settings);
        let err = result.unwrap_err();
        assert!(state.auth_failed());
        assert!(err.message().contains("agentfsSshKeyPassphrase"), "{err}");
        // A wrong passphrase: refused before connecting.
        fs::write(&pass, "wrong\n").unwrap();
        settings.ssh_key_passphrase = Some(pass.clone());
        let (result, state) = f.push(&settings);
        let err = result.unwrap_err();
        assert!(state.auth_failed());
        assert_eq!(classify(&err, false), PushFailureKind::Auth);
        assert!(err.message().contains("cannot decrypt"), "{err}");
        assert_eq!(server.offered().len(), offered, "{:?}", server.offered());
        // The right one.
        fs::write(&pass, "two words\n").unwrap();
        f.push(&settings).0.unwrap();
    }
    assert_eq!(f.served_head(), Some(f.head));
    assert_eq!(
        server.offered(),
        [
            fingerprint(client.public_key()),
            fingerprint(client.public_key())
        ]
    );
}

/// An in-process ssh-agent on a Unix socket in `dir`, holding `keys`.
struct TestAgent {
    socket: PathBuf,
    runtime: Option<tokio::runtime::Runtime>,
}

#[derive(Clone)]
struct AllowAll;

impl russh::keys::agent::server::Agent for AllowAll {}

impl TestAgent {
    fn start(dir: &Path, keys: &[PrivateKey]) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let socket = dir.join(format!("agent-{}.sock", keys.len()));
        let _ = fs::remove_file(&socket);
        let listener = {
            let _guard = runtime.enter();
            tokio::net::UnixListener::bind(&socket).unwrap()
        };
        let connections = futures::stream::unfold(listener, |listener| async move {
            let accepted = listener.accept().await.map(|(stream, _)| stream);
            Some((accepted, listener))
        });
        runtime.spawn(russh::keys::agent::server::serve(
            Box::pin(connections),
            AllowAll,
        ));
        let socket_path = socket.clone();
        let keys = keys.to_vec();
        runtime.block_on(async move {
            let mut client = AgentClient::connect_uds(&socket_path).await.unwrap();
            for key in &keys {
                client.add_identity(key, &[]).await.unwrap();
            }
        });
        Self {
            socket,
            runtime: Some(runtime),
        }
    }

    fn stop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
        let _ = fs::remove_file(&self.socket);
    }
}

impl Drop for TestAgent {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn an_agent_offers_each_identity_once() {
    let f = Fixture::new();
    let (wrong, right) = (ed25519(2), ed25519(1));
    let server = TestServer::start(ed25519(9), vec![right.public_key().clone()], Mode::Git);
    let mut settings = f.remote(&server.url(&f.served), &right, &server.known_hosts_line());
    settings.ssh_key = None;

    // The identity the server authorizes is accepted.
    let mut agent = TestAgent::start(f.dir.path(), &[wrong.clone(), right.clone()]);
    settings.ssh_agent = Some(agent.socket.clone());
    let (result, state) = f.push(&settings);
    result.unwrap();
    assert!(!state.auth_failed());
    assert_eq!(f.served_head(), Some(f.head));
    // The test agent keeps its keys in a hash map, so their order varies:
    // offered once each, up to the accepted one.
    let offered = server.offered();
    assert_eq!(
        offered.last(),
        Some(&fingerprint(right.public_key())),
        "{offered:?}"
    );
    let wrong_first = [
        fingerprint(wrong.public_key()),
        fingerprint(right.public_key()),
    ];
    assert!(
        offered == wrong_first[1..] || offered == wrong_first,
        "{offered:?}"
    );
    agent.stop();

    // Only a refused identity: auth, naming what was offered.
    let only_wrong = TestAgent::start(f.dir.path(), std::slice::from_ref(&wrong));
    settings.ssh_agent = Some(only_wrong.socket.clone());
    let (result, state) = f.push(&settings);
    let err = result.unwrap_err();
    assert!(state.auth_failed());
    assert_eq!(classify(&err, false), PushFailureKind::Auth);
    assert!(err.message().contains("rejected every identity"), "{err}");
    assert!(
        err.message().contains(&fingerprint(wrong.public_key())),
        "{err}"
    );

    // An empty agent, and a dead socket: auth, naming the socket.
    let empty = TestAgent::start(f.dir.path(), &[]);
    let dead = f.dir.path().join("dead.sock");
    for (socket, says) in [
        (empty.socket.clone(), "holds no identity"),
        (dead.clone(), "ssh-agent at"),
    ] {
        settings.ssh_agent = Some(socket.clone());
        let before = server.offered().len();
        let (result, state) = f.push(&settings);
        let err = result.unwrap_err();
        assert!(state.auth_failed());
        assert_eq!(classify(&err, false), PushFailureKind::Auth);
        assert!(err.message().contains(says), "{err}");
        assert!(
            err.message().contains(&socket.display().to_string()),
            "{err}"
        );
        assert_eq!(server.offered().len(), before);
    }
}
