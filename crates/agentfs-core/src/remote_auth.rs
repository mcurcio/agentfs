//! Authenticating to one remote (design §11.1), shared by the push
//! ([`crate::push`]) and the patch-queue fetch and push
//! ([`crate::patch`]).
//!
//! The credentials callback gives one credential per connection, for the
//! remote URL's host only. HTTP(S) remotes get a username and password
//! (token) from `agentfsCredentials`; SSH remotes get only their private
//! key from `agentfsSshKey`, and only after their host key has been checked
//! against `agentfsKnownHosts` in libgit2's certificate callback, which
//! runs before authentication.

use std::cell::{Cell, RefCell};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use git2::{
    CertificateCheckStatus, Cred, CredentialType, Error as GitError, ErrorClass, ErrorCode,
    RemoteCallbacks,
};

use crate::config::RemoteSettings;
use crate::credentials::{Credential, CredentialFile, DEFAULT_SSH_USERNAME, SshKey, SshKeyFile};
use crate::known_hosts::{self, KnownHostsFile};
use crate::remote_url::{Endpoint, ssh_endpoint};

/// What the credentials callback hands libgit2 (see
/// [`RemoteAuth::answer`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialAnswer {
    /// The user name alone, asked before authenticating over SSH.
    Username(String),
    UserPass(Credential),
    SshKey {
        username: String,
        key: SshKey,
    },
}

impl CredentialAnswer {
    fn into_cred(self) -> Result<Cred, GitError> {
        match self {
            Self::Username(username) => Cred::username(&username),
            Self::UserPass(c) => Cred::userpass_plaintext(&c.username, &c.password),
            // Without the `ssh` feature libgit2 refuses this, but config
            // never lets an ssh remote through then (design §11.1).
            Self::SshKey { username, key } => {
                Cred::ssh_key_from_memory(&username, None, &key.text, None)
            }
        }
    }
}

/// What the callbacks of one connection saw, read once libgit2 returns.
#[derive(Debug, Default)]
pub(crate) struct AuthState {
    /// Credentials handed out on this connection.
    calls: Cell<u32>,
    /// The credentials callback refused or ran out of attempts.
    pub auth_failed: Cell<bool>,
    /// Why the SSH host key was refused, if it was: libgit2 replaces the
    /// callback's message with its own.
    pub host_key_failure: RefCell<Option<String>>,
}

/// One remote's credentials, SSH key and known hosts, bound to its push
/// URL's host.
pub struct RemoteAuth {
    name: String,
    credentials: Option<CredentialFile>,
    /// The push URL's host and port, for ssh remotes only.
    ssh: Option<Endpoint>,
    ssh_key: Option<SshKeyFile>,
    known_hosts: Option<KnownHostsFile>,
}

impl RemoteAuth {
    pub fn new(remote: &RemoteSettings) -> Self {
        Self {
            name: remote.name.clone(),
            credentials: remote
                .credentials
                .as_ref()
                .map(|path| CredentialFile::new(path, remote.push_url())),
            ssh: ssh_endpoint(remote.push_url()),
            ssh_key: remote
                .ssh_key
                .as_ref()
                .map(|path| SshKeyFile::new(path, remote.push_url())),
            known_hosts: remote.known_hosts.as_ref().map(KnownHostsFile::new),
        }
    }

    /// The push URL's host and port, if it is an ssh remote.
    pub fn ssh(&self) -> Option<&Endpoint> {
        self.ssh.as_ref()
    }

    /// The credentials and, for ssh remotes, certificate callbacks of one
    /// connection, reporting into `state`.
    pub(crate) fn callbacks<'a>(&'a self, state: &'a AuthState) -> RemoteCallbacks<'a> {
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(move |url, username, allowed| {
            let result = self
                .answer(url, username, allowed, &state.calls)
                .and_then(|answer| answer.into_cred().map_err(|e| e.message().to_owned()))
                .map_err(|message| GitError::new(ErrorCode::Auth, ErrorClass::Callback, message));
            if result.is_err() {
                state.auth_failed.set(true);
            }
            result
        });
        if self.ssh.is_some() {
            // Runs before any authentication, so the key is never offered
            // to a host that is not trusted.
            callbacks.certificate_check(move |cert, hostname| {
                let Some(hostkey) = cert.as_hostkey() else {
                    return Ok(CertificateCheckStatus::CertificatePassthrough);
                };
                // Never `hostkey_type()`: it panics on types git2 does not
                // know.
                match self.check_host_key(hostname, hostkey.hostkey(), hostkey.hash_sha256()) {
                    Ok(()) => Ok(CertificateCheckStatus::CertificateOk),
                    Err(message) => {
                        // libgit2 replaces this error's message with its
                        // own, so keep the verdict on the side.
                        *state.host_key_failure.borrow_mut() = Some(message.clone());
                        Err(GitError::new(
                            ErrorCode::Certificate,
                            ErrorClass::Ssh,
                            message,
                        ))
                    }
                }
            });
        }
        callbacks
    }

    /// The credentials callback's decision (design §11.1): one credential
    /// or key per connection, from the remote's own file, for its own host
    /// only. SSH remotes get only their key, never a password; other
    /// remotes never get a key. libgit2's request for a user name alone,
    /// made before authenticating to an SSH URL that names none, is
    /// answered with [`DEFAULT_SSH_USERNAME`] and is not an attempt.
    pub(crate) fn answer(
        &self,
        url: &str,
        username: Option<&str>,
        allowed: CredentialType,
        calls: &Cell<u32>,
    ) -> Result<CredentialAnswer, String> {
        let name = &self.name;
        if self.ssh.is_some() && allowed == CredentialType::USERNAME {
            return Ok(CredentialAnswer::Username(DEFAULT_SSH_USERNAME.to_owned()));
        }
        calls.set(calls.get() + 1);
        if calls.get() > 1 {
            return Err(match self.ssh {
                Some(_) => format!("{url} rejected the SSH key for remote {name}"),
                None => format!("{url} rejected the credentials for remote {name}"),
            });
        }
        if self.ssh.is_some() {
            let Some(file) = &self.ssh_key else {
                return Err(format!(
                    "{url} asks for a key and remote.{name}.agentfsSshKey is not set"
                ));
            };
            if !allowed.contains(CredentialType::SSH_MEMORY) {
                return Err(format!(
                    "{url} does not accept public keys (it offers {allowed:?}); ssh remotes \
                     authenticate only with agentfsSshKey"
                ));
            }
            let key = file
                .key_for(url)
                .map_err(|e| format!("{}: {e}", file.path().display()))?;
            let username = username
                .filter(|u| !u.is_empty())
                .unwrap_or(DEFAULT_SSH_USERNAME)
                .to_owned();
            return Ok(CredentialAnswer::SshKey { username, key });
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
            .map(CredentialAnswer::UserPass)
            .map_err(|e| e.to_string())
    }

    /// Whether the SSH server's host key is trusted (design §11.1): the
    /// host libgit2 connected to must be the push URL's, and its raw key
    /// must be listed for that host and port in `agentfsKnownHosts`. The
    /// error says which file, name, key type and SHA256 fingerprint, and
    /// how to fix it.
    pub(crate) fn check_host_key(
        &self,
        hostname: &str,
        raw: Option<&[u8]>,
        sha256: Option<&[u8; 32]>,
    ) -> Result<(), String> {
        let name = &self.name;
        let Some(endpoint) = &self.ssh else {
            return Err(format!("remote {name} is not an ssh remote"));
        };
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
                "libgit2 connected to host {hostname:?}, not {:?} from the push URL; its host \
                 key is not trusted",
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
