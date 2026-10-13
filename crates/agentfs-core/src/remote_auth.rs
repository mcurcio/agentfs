//! Authenticating to one remote (design §11.1), shared by the push
//! ([`crate::push`]) and the patch-queue fetch and push
//! ([`crate::patch`]).
//!
//! HTTP(S) remotes authenticate through libgit2's credentials callback,
//! which gives one username and password (token) per connection from
//! `agentfsCredentials`, for the remote URL's host only.
//!
//! SSH remotes never go through libgit2's callbacks: agentfs's own SSH
//! transport (`ssh_transport`, `ssh` feature) checks the host key against
//! `agentfsKnownHosts` before any authentication and then offers only the
//! key from `agentfsSshKey`. It finds the remote's [`SshRemote`] and the
//! connection's [`AuthState`] through [`RemoteAuth::ssh_scope`], held
//! around each libgit2 call that may connect.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use git2::{Cred, CredentialType, Error as GitError, ErrorClass, ErrorCode, RemoteCallbacks};

use crate::config::RemoteSettings;
use crate::credentials::{Credential, CredentialFile, SshKey, SshKeyFile};
use crate::known_hosts::{self, KnownHostsFile};
use crate::remote_url::{Endpoint, ssh_endpoint};

/// What one connection's authentication saw, read once libgit2 returns.
/// Shared with the SSH transport, which may run on another thread.
#[derive(Debug, Default)]
pub(crate) struct AuthState {
    /// The credential or key was refused, or none could be read.
    auth_failed: AtomicBool,
    /// Why the SSH host key was refused, if it was: libgit2 replaces the
    /// transport's message with its own.
    host_key_failure: Mutex<Option<String>>,
}

impl AuthState {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn auth_failed(&self) -> bool {
        self.auth_failed.load(Ordering::SeqCst)
    }

    pub(crate) fn fail_auth(&self) {
        self.auth_failed.store(true, Ordering::SeqCst);
    }

    pub(crate) fn host_key_failure(&self) -> Option<String> {
        self.host_key_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    // Only the SSH transport refuses host keys.
    #[cfg_attr(not(feature = "ssh"), allow(dead_code))]
    pub(crate) fn refuse_host(&self, why: String) {
        *self
            .host_key_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(why);
    }
}

/// One SSH remote's key and known hosts, bound to its push URL's host and
/// port. Read by the SSH transport (`ssh` feature).
#[cfg_attr(not(feature = "ssh"), allow(dead_code))]
pub struct SshRemote {
    name: String,
    endpoint: Endpoint,
    key: Option<SshKeyFile>,
    known_hosts: Option<KnownHostsFile>,
}

#[cfg_attr(not(feature = "ssh"), allow(dead_code))]
impl SshRemote {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The push URL's host and port.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The `agentfsSshKey` file, if set.
    pub fn key_path(&self) -> Option<&std::path::Path> {
        self.key.as_ref().map(SshKeyFile::path)
    }

    /// The private key for a connection to `url`, from the remote's own
    /// file, for its own host and port only. The error names the file.
    pub(crate) fn key(&self, url: &str) -> Result<SshKey, String> {
        let name = &self.name;
        let Some(file) = &self.key else {
            return Err(format!(
                "{url} needs a key and remote.{name}.agentfsSshKey is not set"
            ));
        };
        file.key_for(url)
            .map_err(|e| format!("{}: {e}", file.path().display()))
    }

    /// The key types `agentfsKnownHosts` lists for the remote's host and
    /// port, in file order, so that the transport asks for those first, as
    /// OpenSSH does. Empty if the file cannot be read; the host key check
    /// then says why.
    pub(crate) fn known_key_types(&self) -> Vec<String> {
        let Some(file) = &self.known_hosts else {
            return Vec::new();
        };
        file.load()
            .map(|hosts| hosts.key_types(&self.endpoint.host, self.endpoint.port))
            .unwrap_or_default()
    }

    /// Whether the SSH server's host key is trusted (design §11.1): the
    /// host connected to must be the push URL's, and its raw key must be
    /// listed for that host and port in `agentfsKnownHosts`. The error says
    /// which file, name, key type and SHA256 fingerprint, and how to fix
    /// it.
    pub(crate) fn check_host_key(
        &self,
        hostname: &str,
        raw: Option<&[u8]>,
        sha256: Option<&[u8; 32]>,
    ) -> Result<(), String> {
        let name = &self.name;
        let endpoint = &self.endpoint;
        let lookup = known_hosts::lookup_name(&endpoint.host, endpoint.port);
        let Some(file) = &self.known_hosts else {
            return Err(format!(
                "host key of {lookup} not checked: remote.{name}.agentfsKnownHosts is not set"
            ));
        };
        let connected = hostname
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(hostname)
            .to_ascii_lowercase();
        if connected != endpoint.host {
            return Err(format!(
                "connected to host {hostname:?}, not {:?} from the push URL; its host key is \
                 not trusted",
                endpoint.host
            ));
        }
        let Some(raw) = raw else {
            return Err(format!(
                "the SSH server at {lookup} presented no host key that could be checked"
            ));
        };
        let key_type = known_hosts::key_type(raw).unwrap_or("unknown-type");
        let fingerprint = sha256.map_or_else(
            || "(no SHA256 fingerprint)".to_owned(),
            |h| format!("SHA256:{}", STANDARD_NO_PAD.encode(h)),
        );
        file.check(&endpoint.host, endpoint.port, raw).map_err(|e| {
            let port = endpoint
                .port
                .map(|p| format!(" -p {p}"))
                .unwrap_or_default();
            format!(
                "{key_type} host key {fingerprint} of {lookup} refused by {}: {e}; once the \
                 fingerprint is confirmed out of band, list the server's keys with \
                 `ssh-keyscan{port} {}`",
                file.path().display(),
                endpoint.host
            )
        })
    }
}

/// While alive, libgit2 connections made on this thread to an SSH URL use
/// this remote's key and known hosts and report into its [`AuthState`]
/// (see [`RemoteAuth::ssh_scope`]).
#[must_use = "the scope ends when dropped"]
pub(crate) struct SshScope {
    #[cfg(feature = "ssh")]
    _scope: Option<crate::ssh_transport::Scope>,
}

/// One remote's credentials, SSH key and known hosts, bound to its push
/// URL's host.
pub struct RemoteAuth {
    name: String,
    credentials: Option<CredentialFile>,
    /// For ssh remotes only.
    ssh: Option<Arc<SshRemote>>,
}

impl RemoteAuth {
    pub fn new(remote: &RemoteSettings) -> Self {
        let url = remote.push_url();
        Self {
            name: remote.name.clone(),
            credentials: remote
                .credentials
                .as_ref()
                .map(|path| CredentialFile::new(path, url)),
            ssh: ssh_endpoint(url).map(|endpoint| {
                Arc::new(SshRemote {
                    name: remote.name.clone(),
                    endpoint,
                    key: remote
                        .ssh_key
                        .as_ref()
                        .map(|path| SshKeyFile::new(path, url)),
                    known_hosts: remote.known_hosts.as_ref().map(KnownHostsFile::new),
                })
            }),
        }
    }

    /// The push URL's host and port, if it is an ssh remote.
    pub fn ssh(&self) -> Option<&Endpoint> {
        self.ssh.as_deref().map(SshRemote::endpoint)
    }

    /// The remote's SSH side, if it is an ssh remote.
    pub fn ssh_remote(&self) -> Option<&Arc<SshRemote>> {
        self.ssh.as_ref()
    }

    /// Hands this remote's SSH side and `state` to the SSH transport for
    /// libgit2 calls made on this thread until the scope is dropped.
    /// Without it, or for a remote that is not an ssh remote, the transport
    /// refuses every connection (design §11.1).
    pub(crate) fn ssh_scope(&self, state: &Arc<AuthState>) -> SshScope {
        #[cfg(feature = "ssh")]
        {
            SshScope {
                _scope: self.ssh.as_ref().map(|remote| {
                    crate::ssh_transport::Scope::enter(Arc::clone(remote), Arc::clone(state))
                }),
            }
        }
        #[cfg(not(feature = "ssh"))]
        {
            let _ = state;
            SshScope {}
        }
    }

    /// The credentials callback of one connection, reporting into `state`.
    pub(crate) fn callbacks<'a>(&'a self, state: &'a AuthState) -> RemoteCallbacks<'a> {
        let mut callbacks = RemoteCallbacks::new();
        let calls = Cell::new(0);
        callbacks.credentials(move |url, username, allowed| {
            let result = self
                .answer(url, username, allowed, &calls)
                .and_then(|c| {
                    Cred::userpass_plaintext(&c.username, &c.password)
                        .map_err(|e| e.message().to_owned())
                })
                .map_err(|message| GitError::new(ErrorCode::Auth, ErrorClass::Callback, message));
            if result.is_err() {
                state.fail_auth();
            }
            result
        });
        callbacks
    }

    /// The credentials callback's decision (design §11.1): one username and
    /// password (token) per connection, from the remote's own file, for its
    /// own host only. SSH remotes never get one: their transport is
    /// agentfs's own.
    pub(crate) fn answer(
        &self,
        url: &str,
        username: Option<&str>,
        allowed: CredentialType,
        calls: &Cell<u32>,
    ) -> Result<Credential, String> {
        let name = &self.name;
        if self.ssh.is_some() {
            return Err(format!(
                "{url} asked libgit2 for credentials; ssh remotes authenticate only in \
                 agentfs's SSH transport, with remote.{name}.agentfsSshKey"
            ));
        }
        calls.set(calls.get() + 1);
        if calls.get() > 1 {
            return Err(format!("{url} rejected the credentials for remote {name}"));
        }
        let Some(file) = &self.credentials else {
            return Err(format!(
                "{url} asks for credentials and remote.{name}.agentfsCredentials is not set"
            ));
        };
        if !allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
            return Err(format!(
                "{url} asks for {allowed:?}; only username and password (token) \
                 credentials are supported"
            ));
        }
        file.credential_for(url, username)
            .map_err(|e| e.to_string())
    }

    /// [`SshRemote::check_host_key`] for an ssh remote.
    #[cfg(test)]
    pub(crate) fn check_host_key(
        &self,
        hostname: &str,
        raw: Option<&[u8]>,
        sha256: Option<&[u8; 32]>,
    ) -> Result<(), String> {
        match &self.ssh {
            Some(remote) => remote.check_host_key(hostname, raw, sha256),
            None => Err(format!("remote {} is not an ssh remote", self.name)),
        }
    }
}
