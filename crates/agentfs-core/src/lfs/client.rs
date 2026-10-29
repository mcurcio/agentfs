//! Moving LFS objects between the local cache and a remote's LFS endpoint
//! (design §7).
//!
//! - [`HttpTransfer`]: the git-lfs batch API (`POST <endpoint>/objects/batch`)
//!   with the `basic` transfer adapter only, over `ureq`: uploads (a `PUT`
//!   of the cached object, then `verify` when the server asks for it) and
//!   downloads (a `GET`, hashed as it is written into the cache). Batches
//!   of [`BATCH_SIZE`] run in sequence; within a batch, up to
//!   `lfs.concurrenttransfers` objects move at once ([`super::pool`]).
//!   An upload stops starting objects at its first failure and lets those
//!   in flight finish, so the next attempt sends only what is left (the
//!   batch API skips objects the server has). Downloads resume by byte
//!   from the cache's partial file ([`super::cache::Partial`]) with HTTP
//!   `Range`, and an object is tried [`DOWNLOAD_TRIES`] times when the
//!   server cannot be reached.
//! - [`FileTransfer`]: git-lfs's standalone-file layout for local and
//!   `file://` remotes, `<git dir>/lfs/objects/aa/bb/<oid>`.
//!
//! **Credentials** ([`super::endpoint`]) go only to the endpoint's origin:
//! agentfs's `Authorization` header is added to batch requests, and to an
//! action's `href` only when it has the endpoint's scheme, host and port
//! and the server gave the action no `Authorization` of its own. The
//! headers a server gives an action are sent as given. Redirects are never
//! followed; the remote's proxy (`remote.<name>.proxy`, else `http.proxy`,
//! [`crate::proxy`]) carries every request, with `CONNECT`, and proxy
//! environment variables are ignored.
//!
//! **Timeouts:** 30 s to connect; 5 minutes for a batch or verify request;
//! for an object, 5 minutes plus its size at 256 KiB/s.
//!
//! **Failures** are classified like push failures: connection problems,
//! timeouts, HTTP 429 and 5xx are `unreachable`; 401 and 403 `auth`; a TLS
//! certificate that fails verification `untrusted_host`; any other refusal
//! (another status, a per-object error, a redirect) `refused`; and the
//! cache or configuration `local`. Messages name the endpoint with any
//! userinfo redacted.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;

use super::cache::{ObjectCache, object_path, publish_into};
use super::endpoint::{Direction, EndpointAuth, EndpointKind, LfsEndpoint, resolve};
use super::pool;
#[cfg(feature = "ssh")]
use super::ssh_auth::authenticate;
use super::{LfsError, Pointer};
use crate::config::{RemoteSettings, Settings};
use crate::credentials::{Credential, CredentialFile};
use crate::events::PushFailureKind;

/// Without the `ssh` feature an SSH remote is misconfigured, so this is
/// never reached; it fails closed all the same.
#[cfg(not(feature = "ssh"))]
fn authenticate(
    remote: &RemoteSettings,
    _ssh_url: &str,
    _path: &str,
    _direction: Direction,
) -> Result<super::ssh_auth::Authentication, LfsError> {
    Err(LfsError::local(format!(
        "remote {} needs git-lfs-authenticate, and agentfs was built without the `ssh` cargo \
         feature",
        remote.name
    )))
}
use crate::remote_url::{self, Transport};

/// The git-lfs media type of batch requests and responses.
pub const MEDIA_TYPE: &str = "application/vnd.git-lfs+json";
/// Objects per batch request.
pub const BATCH_SIZE: usize = 100;
/// How long to wait for a connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a batch or verify request may take, and the base of an
/// object transfer's timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// The slowest transfer rate an object's timeout allows for.
pub const MIN_RATE: u64 = 256 << 10;
/// The largest batch response read.
const MAX_BATCH_RESPONSE: u64 = 16 << 20;
/// How many times a download of one object is tried when the server
/// cannot be reached, each resuming where the last stopped.
pub const DOWNLOAD_TRIES: usize = 3;
/// The pauses between those tries.
#[cfg(not(test))]
const RETRY_PAUSES: [Duration; DOWNLOAD_TRIES - 1] =
    [Duration::from_secs(1), Duration::from_secs(2)];
#[cfg(test)]
const RETRY_PAUSES: [Duration; DOWNLOAD_TRIES - 1] =
    [Duration::from_millis(10), Duration::from_millis(20)];

/// What an upload did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadReport {
    /// Objects the commits being pushed reference.
    pub objects: usize,
    /// Objects the server did not have, now uploaded.
    pub uploaded: usize,
    /// Bytes uploaded.
    pub bytes: u64,
}

/// How many objects of an upload missing from the cache a message names.
const NAMED_MISSING: usize = 5;

/// One remote's LFS storage.
pub trait Transfer: Send + Sync {
    /// Makes sure the server has every object in `objects`, uploading
    /// those it lacks from `cache`. Only the objects the server asks for
    /// need to be cached: one it lacks that is not in the cache fails the
    /// upload as `local` before anything is sent. `ref_name` is the ref
    /// being pushed, for servers that authorize by ref.
    fn upload(
        &self,
        objects: &[Pointer],
        cache: &ObjectCache,
        ref_name: &str,
    ) -> Result<UploadReport, LfsError>;

    /// Downloads each object of `pointers` that `cache` lacks into it,
    /// checking its hash, up to `lfs.concurrenttransfers` at once. One
    /// result per pointer, in order.
    fn download_many(
        &self,
        pointers: &[Pointer],
        cache: &ObjectCache,
        ref_name: &str,
    ) -> Vec<Result<(), LfsError>>;

    /// Downloads `pointer`'s object into `cache`, checking its hash.
    fn download(
        &self,
        pointer: &Pointer,
        cache: &ObjectCache,
        ref_name: &str,
    ) -> Result<(), LfsError> {
        self.download_many(std::slice::from_ref(pointer), cache, ref_name)
            .into_iter()
            .next()
            .unwrap_or_else(|| Err(LfsError::local("the download gave no result")))
    }

    /// Whether the endpoint holds each object of `pointers` for
    /// `direction`: for uploads, the server asks for no upload of it; for
    /// downloads, it offers one. Nothing is transferred. An object the
    /// server reports missing (404) is `false`; any other problem fails
    /// the whole call.
    fn present(
        &self,
        pointers: &[Pointer],
        direction: Direction,
        ref_name: &str,
    ) -> Result<Vec<bool>, LfsError>;

    /// Where the objects go.
    fn endpoint(&self) -> &LfsEndpoint;
}

/// Whether a download failure says the endpoint, not one object, is the
/// problem: then no further object is started from it.
fn stops_downloads(e: &LfsError) -> bool {
    e.kind != PushFailureKind::Refused
}

/// The error for objects an upload or download never started because an
/// earlier one failed.
fn not_started(first: Option<&LfsError>) -> LfsError {
    match first {
        Some(e) => LfsError::new(
            e.kind,
            format!("not attempted after an earlier failure: {e}"),
        ),
        None => LfsError::local("not attempted"),
    }
}

/// Fails an upload whose server asks for objects missing from the cache.
fn missing_from_cache(missing: &[&Pointer], endpoint: &LfsEndpoint) -> LfsError {
    let named: Vec<String> = missing
        .iter()
        .take(NAMED_MISSING)
        .map(|p| format!("{} ({} bytes)", p.oid, p.size))
        .collect();
    let more = missing.len().saturating_sub(NAMED_MISSING);
    let more = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    let mut error = LfsError::local(format!(
        "{} lacks {} LFS object(s) missing from the local cache, so they cannot be \
         uploaded: {}{more}; run `agentfs lfs fetch --all` to bring them back from another \
         remote",
        endpoint.display_url,
        missing.len(),
        named.join(", ")
    ));
    error.missing = missing.iter().map(|p| p.oid).collect();
    error
}

/// The transfer for `remote`'s endpoint in `direction`.
pub fn transfer_for(
    settings: &Settings,
    remote: &RemoteSettings,
    direction: Direction,
) -> Result<Box<dyn Transfer>, LfsError> {
    let endpoint = resolve(settings, remote, direction)?;
    let concurrency = settings.lfs.concurrent_transfers;
    match &endpoint.kind {
        EndpointKind::Http { .. } | EndpointKind::SshAuthenticate { .. } => Ok(Box::new(
            HttpTransfer::new(endpoint, remote, direction, concurrency)?,
        )),
        EndpointKind::File { git_dir } => Ok(Box::new(FileTransfer {
            objects: git_dir.join("lfs").join("objects"),
            tmp: git_dir.join("lfs").join("tmp"),
            endpoint,
        })),
    }
}

/// Where a request's credentials come from.
enum Credentials {
    None,
    File {
        key: &'static str,
        file: CredentialFile,
    },
    Fixed(Credential),
    /// The `Authorization` git-lfs-authenticate gave.
    Header(String),
}

/// Where the batch API is and how requests to it authenticate.
struct Base {
    /// The endpoint URL, without userinfo or a final `/`.
    url: String,
    origin: (String, String),
    credentials: Credentials,
    /// The other headers git-lfs-authenticate gave, for batch requests.
    headers: Vec<(String, String)>,
    /// When a git-lfs-authenticate answer stops being valid.
    expires_at: Option<std::time::SystemTime>,
}

/// How long before its expiry an authenticated base is renewed.
const RENEW_BEFORE: Duration = Duration::from_secs(5);

/// How the base is found.
enum Resolver {
    /// An explicit or derived `http(s)` endpoint.
    Fixed(Arc<Base>),
    /// git-lfs-authenticate over SSH, renewed when it expires, or at each
    /// upload, download or presence check when it gives no expiry.
    Ssh {
        remote: Box<RemoteSettings>,
        ssh_url: String,
        path: String,
        direction: Direction,
        lfs_credentials: Option<PathBuf>,
        current: Mutex<Option<Arc<Base>>>,
    },
}

/// The batch API over HTTP(S).
pub struct HttpTransfer {
    endpoint: LfsEndpoint,
    resolver: Resolver,
    agent: ureq::Agent,
    remote: String,
    /// Objects moved at once (`lfs.concurrenttransfers`).
    concurrency: usize,
}

/// A batch response (git-lfs `docs/api/batch.md`).
#[derive(Debug, Deserialize)]
struct BatchResponse {
    #[serde(default)]
    transfer: Option<String>,
    #[serde(default)]
    objects: Vec<BatchObject>,
}

#[derive(Debug, Deserialize)]
struct BatchObject {
    oid: String,
    #[serde(default)]
    actions: Option<Actions>,
    #[serde(default)]
    error: Option<ObjectError>,
}

#[derive(Debug, Default, Deserialize)]
struct Actions {
    #[serde(default)]
    upload: Option<Action>,
    #[serde(default)]
    verify: Option<Action>,
    #[serde(default)]
    download: Option<Action>,
}

#[derive(Debug, Clone, Deserialize)]
struct Action {
    href: String,
    #[serde(default)]
    header: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ObjectError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

impl HttpTransfer {
    /// A client for `endpoint`, an [`EndpointKind::Http`] or
    /// [`EndpointKind::SshAuthenticate`] of `remote`, moving up to
    /// `concurrency` objects at once, through the remote's proxy if it has
    /// one.
    pub fn new(
        endpoint: LfsEndpoint,
        remote: &RemoteSettings,
        direction: Direction,
        concurrency: usize,
    ) -> Result<Self, LfsError> {
        let resolver = match &endpoint.kind {
            EndpointKind::Http { url, auth } => {
                let url = url.trim_end_matches('/').to_owned();
                let origin = Self::origin_of(&url, &endpoint.display_url)?;
                let credentials = match auth {
                    EndpointAuth::None => Credentials::None,
                    EndpointAuth::File { key, path } => Credentials::File {
                        key,
                        file: CredentialFile::new(path, &url),
                    },
                    EndpointAuth::Url { username, password } => Credentials::Fixed(Credential {
                        username: username.clone(),
                        password: password.clone(),
                    }),
                };
                Resolver::Fixed(Arc::new(Base {
                    url,
                    origin,
                    credentials,
                    headers: Vec::new(),
                    expires_at: None,
                }))
            }
            EndpointKind::SshAuthenticate {
                ssh_url,
                path,
                lfs_credentials,
            } => Resolver::Ssh {
                remote: Box::new(remote.clone()),
                ssh_url: ssh_url.clone(),
                path: path.clone(),
                direction,
                lfs_credentials: lfs_credentials.clone(),
                current: Mutex::new(None),
            },
            EndpointKind::File { .. } => {
                return Err(LfsError::local("not an http(s) LFS endpoint"));
            }
        };
        let concurrency = concurrency.max(1);
        let proxy = remote
            .proxy
            .as_ref()
            .map(|p| {
                ureq::Proxy::new(&p.url)
                    .map_err(|e| LfsError::local(format!("the proxy {p} cannot be used: {e}")))
            })
            .transpose()?;
        Ok(Self {
            agent: agent(concurrency, proxy),
            resolver,
            remote: remote.name.clone(),
            endpoint,
            concurrency,
        })
    }

    /// The origin of an endpoint URL, which must be http, or https with
    /// the `https` feature.
    fn origin_of(url: &str, shown: &str) -> Result<(String, String), LfsError> {
        let Some(origin) = remote_url::origin(url) else {
            return Err(LfsError::local(format!("{shown} is not an http(s) URL")));
        };
        if origin.0 == "https" && !cfg!(feature = "https") {
            return Err(LfsError::local(format!(
                "the LFS endpoint {shown} is https, and agentfs was built without the `https` \
                 cargo feature"
            )));
        }
        Ok(origin)
    }

    /// Starts an upload, download or presence check: an authenticated
    /// base without an expiry is valid for one of them only.
    fn begin(&self) {
        if let Resolver::Ssh { current, .. } = &self.resolver {
            let mut current = current.lock().unwrap_or_else(|e| e.into_inner());
            if current.as_ref().is_some_and(|b| b.expires_at.is_none()) {
                *current = None;
            }
        }
    }

    /// The base for the next batch request: fixed, or from
    /// git-lfs-authenticate, run again once the last answer is within
    /// [`RENEW_BEFORE`] of its expiry.
    fn base(&self) -> Result<Arc<Base>, LfsError> {
        let Resolver::Ssh {
            remote,
            ssh_url,
            path,
            direction,
            lfs_credentials,
            current,
        } = &self.resolver
        else {
            let Resolver::Fixed(base) = &self.resolver else {
                unreachable!("the resolver is fixed or ssh");
            };
            return Ok(Arc::clone(base));
        };
        let mut current = current.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::SystemTime::now();
        if let Some(base) = current.as_ref()
            && base.expires_at.is_none_or(|at| now + RENEW_BEFORE < at)
        {
            return Ok(Arc::clone(base));
        }
        let auth = authenticate(remote, ssh_url, path, *direction)?;
        let origin = Self::origin_of(&auth.href, &crate::push::redact_url(&auth.href))?;
        let mut credentials = Credentials::None;
        let mut headers = Vec::new();
        for (name, value) in auth.headers {
            if name.eq_ignore_ascii_case("authorization") {
                credentials = Credentials::Header(value);
            } else {
                headers.push((name, value));
            }
        }
        if matches!(credentials, Credentials::None)
            && let Some(path) = lfs_credentials
        {
            credentials = Credentials::File {
                key: "agentfsLfsCredentials",
                file: CredentialFile::new(path, &auth.href),
            };
        }
        let base = Arc::new(Base {
            url: remote_url::without_userinfo(&auth.href),
            origin,
            credentials,
            headers,
            expires_at: auth.expires_at,
        });
        *current = Some(Arc::clone(&base));
        Ok(base)
    }

    fn display(&self) -> &str {
        &self.endpoint.display_url
    }

    /// The `Authorization` header for the endpoint's origin, if any.
    fn authorization(&self, base: &Base) -> Result<Option<String>, LfsError> {
        let credential = match &base.credentials {
            Credentials::None => return Ok(None),
            Credentials::Header(value) => return Ok(Some(value.clone())),
            Credentials::Fixed(c) => c.clone(),
            Credentials::File { key, file } => file
                .credential_for(&base.url, remote_url::url_username(&base.url))
                .map_err(|e| {
                    LfsError::new(
                        PushFailureKind::Auth,
                        format!(
                            "LFS credentials for {} (remote.{}.{key}): {e}",
                            self.display(),
                            self.remote
                        ),
                    )
                })?,
        };
        let token = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", credential.username, credential.password));
        Ok(Some(format!("Basic {token}")))
    }

    /// The headers for an action: the server's, plus agentfs's
    /// `Authorization` when the href has the endpoint's origin and the
    /// server gave none.
    fn action_headers(
        &self,
        base: &Base,
        action: &Action,
    ) -> Result<Vec<(String, String)>, LfsError> {
        let mut headers: Vec<(String, String)> = action
            .header
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let has_auth = headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
        let same_origin = remote_url::origin(&action.href).as_ref() == Some(&base.origin);
        if !has_auth
            && same_origin
            && let Some(auth) = self.authorization(base)?
        {
            headers.push(("Authorization".into(), auth));
        }
        Ok(headers)
    }

    /// Checks an action's href before anything is sent to it.
    fn check_href(&self, href: &str) -> Result<(), LfsError> {
        match remote_url::transport(href) {
            Transport::Http => Ok(()),
            Transport::Https if cfg!(feature = "https") => Ok(()),
            Transport::Https => Err(LfsError::local(format!(
                "the LFS server at {} sent an https action, and agentfs was built without the \
                 `https` cargo feature",
                self.display()
            ))),
            _ => Err(LfsError::new(
                PushFailureKind::Refused,
                format!(
                    "the LFS server at {} sent an action that is not an http(s) URL",
                    self.display()
                ),
            )),
        }
    }

    /// One batch request, with the base it went to.
    fn batch(
        &self,
        operation: &str,
        objects: &[Pointer],
        ref_name: &str,
    ) -> Result<(BatchResponse, Arc<Base>), LfsError> {
        let base = self.base()?;
        let body = serde_json::json!({
            "operation": operation,
            "transfers": ["basic"],
            "objects": objects
                .iter()
                .map(|p| serde_json::json!({"oid": p.oid.to_string(), "size": p.size}))
                .collect::<Vec<_>>(),
            "hash_algo": "sha256",
            "ref": {"name": ref_name},
        });
        let body = serde_json::to_vec(&body).map_err(|e| LfsError::local(e.to_string()))?;
        let mut request = self
            .agent
            .post(format!("{}/objects/batch", base.url))
            .header("Accept", MEDIA_TYPE)
            .header("Content-Type", MEDIA_TYPE);
        for (k, v) in &base.headers {
            request = request.header(k, v);
        }
        if let Some(auth) = self.authorization(&base)? {
            request = request.header("Authorization", auth);
        }
        let response = request
            .config()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .send(&body[..])
            .map_err(|e| self.transport_error("batch request", e))?;
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .with_config()
            .limit(MAX_BATCH_RESPONSE)
            .read_to_vec()
            .map_err(|e| self.transport_error("batch response", e))?;
        if status != 200 {
            return Err(self.status_error(&base, "batch request", status, &bytes));
        }
        let parsed: BatchResponse = serde_json::from_slice(&bytes).map_err(|e| {
            LfsError::new(
                PushFailureKind::Refused,
                format!(
                    "the LFS server at {} sent a batch response agentfs cannot read: {e}",
                    self.display()
                ),
            )
        })?;
        if let Some(transfer) = &parsed.transfer
            && transfer != "basic"
        {
            return Err(LfsError::new(
                PushFailureKind::Refused,
                format!(
                    "the LFS server at {} chose the {transfer:?} transfer; agentfs supports \
                     only basic",
                    self.display()
                ),
            ));
        }
        Ok((parsed, base))
    }

    /// The response's entry for `pointer`, or why there is none.
    fn object<'a>(
        &self,
        response: &'a BatchResponse,
        pointer: &Pointer,
    ) -> Result<&'a BatchObject, LfsError> {
        let oid = pointer.oid.to_string();
        let object = response
            .objects
            .iter()
            .find(|o| o.oid == oid)
            .ok_or_else(|| {
                LfsError::new(
                    PushFailureKind::Refused,
                    format!(
                        "the LFS server at {} did not answer for object {oid}",
                        self.display()
                    ),
                )
            })?;
        if let Some(error) = &object.error {
            let kind = match error.code {
                401 | 403 => PushFailureKind::Auth,
                429 | 500..=599 => PushFailureKind::Unreachable,
                _ => PushFailureKind::Refused,
            };
            return Err(LfsError::new(
                kind,
                format!(
                    "the LFS server at {} refused object {oid} ({} bytes): {} {}",
                    self.display(),
                    pointer.size,
                    error.code,
                    error.message
                ),
            ));
        }
        Ok(object)
    }

    fn put(
        &self,
        base: &Base,
        pointer: &Pointer,
        action: &Action,
        cache: &ObjectCache,
    ) -> Result<(), LfsError> {
        self.check_href(&action.href)?;
        let file = cache.open_object(pointer).map_err(|e| {
            LfsError::local(format!(
                "LFS object {} is not in the local cache ({e}); it cannot be uploaded",
                pointer.oid
            ))
        })?;
        let mut request = self
            .agent
            .put(&action.href)
            .header("Content-Type", "application/octet-stream");
        for (k, v) in self.action_headers(base, action)? {
            request = request.header(k, v);
        }
        let response = request
            .config()
            .timeout_global(Some(object_timeout(pointer.size)))
            .build()
            .send(&file)
            .map_err(|e| self.transport_error("upload", e))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = read_small(response);
            return Err(self.status_error(
                base,
                &format!("upload of {}", pointer.oid),
                status,
                &body,
            ));
        }
        Ok(())
    }

    fn verify(&self, base: &Base, pointer: &Pointer, action: &Action) -> Result<(), LfsError> {
        self.check_href(&action.href)?;
        let body = serde_json::to_vec(
            &serde_json::json!({"oid": pointer.oid.to_string(), "size": pointer.size}),
        )
        .map_err(|e| LfsError::local(e.to_string()))?;
        let mut request = self
            .agent
            .post(&action.href)
            .header("Accept", MEDIA_TYPE)
            .header("Content-Type", MEDIA_TYPE);
        for (k, v) in self.action_headers(base, action)? {
            request = request.header(k, v);
        }
        let response = request
            .config()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .send(&body[..])
            .map_err(|e| self.transport_error("verify", e))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = read_small(response);
            return Err(self.status_error(
                base,
                &format!("verify of {}", pointer.oid),
                status,
                &body,
            ));
        }
        Ok(())
    }

    /// Downloads one object, resuming its partial download if there is
    /// one, and retrying a failure to reach the server up to
    /// [`DOWNLOAD_TRIES`] times in all, each from where the last stopped.
    fn get(
        &self,
        base: &Base,
        pointer: &Pointer,
        action: &Action,
        cache: &ObjectCache,
    ) -> Result<(), LfsError> {
        let mut tries = 0;
        loop {
            tries += 1;
            match self.get_once(base, pointer, action, cache) {
                Err(e) if e.kind == PushFailureKind::Unreachable && tries < DOWNLOAD_TRIES => {
                    std::thread::sleep(RETRY_PAUSES[tries - 1]);
                }
                result => return result,
            }
        }
    }

    /// One download attempt (design §7): with `Range: bytes=<n>-` when
    /// `n` bytes are already in the partial. A `206` must start at `n`
    /// (only the start is checked: lfs-test-server reports the remaining
    /// length as the total); a `200` starts again from the first byte; a
    /// `416` or a `206` starting elsewhere drops the partial and asks once
    /// more for the whole object, without `Range`.
    fn get_once(
        &self,
        base: &Base,
        pointer: &Pointer,
        action: &Action,
        cache: &ObjectCache,
    ) -> Result<(), LfsError> {
        self.check_href(&action.href)?;
        let Some(mut partial) = cache.partial(pointer)? else {
            return Ok(()); // Another download finished it.
        };
        let local = |e: io::Error| LfsError::local(format!("writing to the LFS cache: {e}"));
        let mut whole = false;
        loop {
            let offset = if whole { 0 } else { partial.offset() };
            let mut request = self.agent.get(&action.href);
            for (k, v) in self.action_headers(base, action)? {
                request = request.header(k, v);
            }
            if offset > 0 {
                request = request.header("Range", format!("bytes={offset}-"));
            }
            let response = request
                .config()
                .timeout_global(Some(object_timeout(pointer.size - offset)))
                .build()
                .call()
                .map_err(|e| self.transport_error("download", e))?;
            let status = response.status().as_u16();
            let resumes = status == 206
                && offset > 0
                && content_range_start(response.headers()) == Some(offset);
            match status {
                200 => partial.restart().map_err(local)?,
                206 if resumes => {}
                206 | 416 if !whole && offset > 0 => {
                    partial.restart().map_err(local)?;
                    whole = true;
                    continue;
                }
                _ => {
                    let body = read_small(response);
                    return Err(self.status_error(
                        base,
                        &format!("download of {}", pointer.oid),
                        status,
                        &body,
                    ));
                }
            }
            let mut reader = response.into_body().into_reader();
            partial.append(&mut reader)?;
            return partial.finish(cache);
        }
    }

    fn transport_error(&self, what: &str, e: ureq::Error) -> LfsError {
        let text = e.to_string();
        let kind = match e {
            ureq::Error::BadUri(_) | ureq::Error::Http(_) => PushFailureKind::Local,
            _ if is_certificate_error(&text) => PushFailureKind::UntrustedHost,
            _ => PushFailureKind::Unreachable,
        };
        LfsError::new(
            kind,
            format!("LFS {what} to {} failed: {text}", self.display()),
        )
    }

    fn status_error(&self, base: &Base, what: &str, status: u16, body: &[u8]) -> LfsError {
        let kind = match status {
            401 | 403 => PushFailureKind::Auth,
            429 | 500..=599 => PushFailureKind::Unreachable,
            _ => PushFailureKind::Refused,
        };
        let detail = server_message(body);
        let hint = match (kind, &base.credentials) {
            (PushFailureKind::Auth, Credentials::None) => format!(
                "; no credentials are configured for it (set remote.{}.agentfsLfsCredentials)",
                self.remote
            ),
            (PushFailureKind::Auth, Credentials::File { key, .. }) => {
                format!("; check the file remote.{}.{key} names", self.remote)
            }
            (PushFailureKind::Refused, _) if (300..400).contains(&status) => {
                "; redirects are not followed".to_owned()
            }
            _ => String::new(),
        };
        LfsError::new(
            kind,
            format!(
                "LFS {what} to {} failed: HTTP {status}{detail}{hint}",
                self.display()
            ),
        )
    }
}

/// One object to upload: its upload action and verify action, if any.
type Upload<'a> = (&'a Pointer, Action, Option<Action>);

impl HttpTransfer {
    /// The objects of one batch response the server wants uploaded, with
    /// their upload and verify actions.
    fn uploads_of<'a>(
        &self,
        response: &BatchResponse,
        chunk: &'a [Pointer],
    ) -> Result<Vec<Upload<'a>>, LfsError> {
        let mut todo = Vec::new();
        for pointer in chunk {
            let object = self.object(response, pointer)?;
            let actions = object.actions.as_ref();
            // No upload action: the server has it already.
            let Some(upload) = actions.and_then(|a| a.upload.as_ref()) else {
                continue;
            };
            let verify = actions.and_then(|a| a.verify.clone());
            todo.push((pointer, upload.clone(), verify));
        }
        Ok(todo)
    }

    /// Downloads one batch's objects (all missing from `cache`).
    fn download_batch(
        &self,
        chunk: &[Pointer],
        cache: &ObjectCache,
        ref_name: &str,
    ) -> Vec<Result<(), LfsError>> {
        let (response, base) = match self.batch("download", chunk, ref_name) {
            Ok(answer) => answer,
            Err(e) => return chunk.iter().map(|_| Err(e.clone())).collect(),
        };
        let mut results: Vec<Option<Result<(), LfsError>>> = vec![None; chunk.len()];
        let mut todo = Vec::new();
        for (index, pointer) in chunk.iter().enumerate() {
            let action = self.object(&response, pointer).and_then(|object| {
                object
                    .actions
                    .as_ref()
                    .and_then(|a| a.download.clone())
                    .ok_or_else(|| {
                        LfsError::new(
                            PushFailureKind::Refused,
                            format!(
                                "the LFS server at {} offered no download of object {}",
                                self.display(),
                                pointer.oid
                            ),
                        )
                    })
            });
            match action {
                Ok(action) => todo.push((index, pointer, action)),
                Err(e) => results[index] = Some(Err(e)),
            }
        }
        let done = pool::run(
            &todo,
            self.concurrency,
            stops_downloads,
            |(_, pointer, action)| self.get(&base, pointer, action, cache),
        );
        let first = pool::first_error(&done).cloned();
        for ((index, _, _), result) in todo.iter().zip(done) {
            results[*index] = Some(result.unwrap_or_else(|| Err(not_started(first.as_ref()))));
        }
        results
            .into_iter()
            .map(|r| r.unwrap_or_else(|| Err(not_started(first.as_ref()))))
            .collect()
    }
}

impl Transfer for HttpTransfer {
    fn upload(
        &self,
        objects: &[Pointer],
        cache: &ObjectCache,
        ref_name: &str,
    ) -> Result<UploadReport, LfsError> {
        self.begin();
        let mut report = UploadReport {
            objects: objects.len(),
            ..UploadReport::default()
        };
        for chunk in objects.chunks(BATCH_SIZE) {
            let (response, base) = self.batch("upload", chunk, ref_name)?;
            let todo = self.uploads_of(&response, chunk)?;
            let missing: Vec<&Pointer> = todo
                .iter()
                .map(|(p, _, _)| *p)
                .filter(|p| !cache.contains(p))
                .collect();
            if !missing.is_empty() {
                return Err(missing_from_cache(&missing, &self.endpoint));
            }
            let done = pool::run(
                &todo,
                self.concurrency,
                |_| true,
                |(pointer, upload, verify)| {
                    self.put(&base, pointer, upload, cache)?;
                    if let Some(verify) = verify {
                        self.verify(&base, pointer, verify)?;
                    }
                    Ok(pointer.size)
                },
            );
            for size in done.iter().flatten().flatten() {
                report.uploaded += 1;
                report.bytes += size;
            }
            if let Some(e) = pool::first_error(&done) {
                return Err(e.clone());
            }
        }
        Ok(report)
    }

    fn download_many(
        &self,
        pointers: &[Pointer],
        cache: &ObjectCache,
        ref_name: &str,
    ) -> Vec<Result<(), LfsError>> {
        self.begin();
        let mut results: Vec<Option<Result<(), LfsError>>> = vec![None; pointers.len()];
        let wanted: Vec<(usize, Pointer)> = pointers
            .iter()
            .enumerate()
            .filter(|(index, p)| {
                if cache.contains(p) {
                    results[*index] = Some(Ok(()));
                    false
                } else {
                    true
                }
            })
            .map(|(index, p)| (index, *p))
            .collect();
        let mut stopped: Option<LfsError> = None;
        for chunk in wanted.chunks(BATCH_SIZE) {
            let pointers: Vec<Pointer> = chunk.iter().map(|(_, p)| *p).collect();
            let done = match &stopped {
                Some(e) => pointers.iter().map(|_| Err(not_started(Some(e)))).collect(),
                None => self.download_batch(&pointers, cache, ref_name),
            };
            for ((index, _), result) in chunk.iter().zip(done) {
                if let Err(e) = &result
                    && stopped.is_none()
                    && stops_downloads(e)
                {
                    stopped = Some(e.clone());
                }
                results[*index] = Some(result);
            }
        }
        results
            .into_iter()
            .map(|r| r.unwrap_or_else(|| Err(not_started(None))))
            .collect()
    }

    fn present(
        &self,
        pointers: &[Pointer],
        direction: Direction,
        ref_name: &str,
    ) -> Result<Vec<bool>, LfsError> {
        self.begin();
        let mut out = Vec::with_capacity(pointers.len());
        for chunk in pointers.chunks(BATCH_SIZE) {
            let (response, _) = self.batch(direction.operation(), chunk, ref_name)?;
            for pointer in chunk {
                let oid = pointer.oid.to_string();
                let object = response.objects.iter().find(|o| o.oid == oid);
                if let Some(BatchObject {
                    error: Some(ObjectError { code: 404, .. }),
                    ..
                }) = object
                {
                    out.push(false);
                    continue;
                }
                let object = self.object(&response, pointer)?;
                let actions = object.actions.as_ref();
                out.push(match direction {
                    Direction::Upload => actions.and_then(|a| a.upload.as_ref()).is_none(),
                    Direction::Download => actions.and_then(|a| a.download.as_ref()).is_some(),
                });
            }
        }
        Ok(out)
    }

    fn endpoint(&self) -> &LfsEndpoint {
        &self.endpoint
    }
}

/// The HTTP client: no redirects, the remote's proxy if it has one (never
/// one from the environment), the timeouts above, an idle connection kept
/// per concurrent transfer, and with the `https` feature native-tls over
/// the system trust store, as libgit2 uses. Also the fork delivery's
/// webhook client ([`crate::fork::deliver`]).
pub(crate) fn agent(concurrency: usize, proxy: Option<ureq::Proxy>) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .max_idle_connections_per_host(concurrency)
        .max_idle_connections(concurrency.max(3))
        .proxy(proxy)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .user_agent(concat!("agentfs/", env!("CARGO_PKG_VERSION")));
    #[cfg(feature = "https")]
    let config = config.tls_config(
        ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build(),
    );
    ureq::Agent::new_with_config(config.build())
}

/// What a [`post`] was answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    pub status: u16,
    /// The `Location` header, if any (a redirect is never followed).
    pub location: Option<String>,
    /// Up to 4 KiB of the body.
    pub body: Vec<u8>,
}

/// Why a [`post`] got no answer.
#[derive(Debug, thiserror::Error)]
pub enum PostError {
    /// The request could not be built (a bad URL or header).
    #[error("request to {url} is malformed: {detail}")]
    Malformed { url: String, detail: String },
    /// It could not reach the server, or timed out.
    #[error("POST {url}: {detail}")]
    Unreachable { url: String, detail: String },
}

/// One POST of `body` as `application/json` with `headers`, through
/// [`agent`] (no redirects, never a proxy from the environment, as the LFS
/// client, design §7), within `timeout`: the one HTTP POST of fork
/// delivery's webhook ([`crate::fork::deliver`]) and the patch queue's
/// notify webhook ([`crate::patch::notify`], bead agentfs-mia.5).
pub fn post(
    url: &str,
    body: &[u8],
    headers: &[(&str, String)],
    timeout: Duration,
) -> Result<Posted, PostError> {
    let mut request = agent(1, None)
        .post(url)
        .header("Content-Type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let response = request
        .config()
        .timeout_global(Some(timeout))
        .build()
        .send(body);
    let response = match response {
        Ok(r) => r,
        Err(e @ (ureq::Error::BadUri(_) | ureq::Error::Http(_))) => {
            return Err(PostError::Malformed {
                url: url.to_owned(),
                detail: e.to_string(),
            });
        }
        Err(e) => {
            return Err(PostError::Unreachable {
                url: url.to_owned(),
                detail: e.to_string(),
            });
        }
    };
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    Ok(Posted {
        status,
        location,
        body: read_small(response),
    })
}

/// [`post`] for a caller that needs only the status code; why it got none
/// as text.
pub fn post_json(
    url: &str,
    body: &[u8],
    headers: &[(&str, String)],
    timeout: Duration,
) -> Result<u16, String> {
    post(url, body, headers, timeout)
        .map(|posted| posted.status)
        .map_err(|e| e.to_string())
}

/// How long one object's transfer may take.
pub fn object_timeout(size: u64) -> Duration {
    REQUEST_TIMEOUT + Duration::from_secs(size / MIN_RATE)
}

/// Whether a transport error is a TLS certificate that failed
/// verification. Matched on OpenSSL's and native-tls's messages, which is
/// all ureq exposes.
fn is_certificate_error(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("certificate verify failed")
        || text.contains("self-signed certificate")
        || text.contains("self signed certificate")
        || text.contains("unable to get local issuer certificate")
        || text.contains("certificate has expired")
        || text.contains("hostname mismatch")
}

/// The first byte of a `206` response's `Content-Range: bytes <a>-<b>/<n>`.
fn content_range_start(headers: &ureq::http::HeaderMap) -> Option<u64> {
    let value = headers.get("content-range")?.to_str().ok()?.trim();
    let range = value.strip_prefix("bytes")?.trim_start();
    let (start, _) = range.split_once('-')?;
    start.trim().parse().ok()
}

/// Up to 4 KiB of an error response's body.
pub(crate) fn read_small(response: ureq::http::Response<ureq::Body>) -> Vec<u8> {
    response
        .into_body()
        .with_config()
        .limit(4096)
        .read_to_vec()
        .unwrap_or_default()
}

/// `: <message>` from a git-lfs error body (`{"message": ...}`), else from
/// the first line of the body, if any.
fn server_message(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Message {
        message: String,
    }
    if let Ok(m) = serde_json::from_slice::<Message>(body) {
        return format!(": {}", m.message.trim());
    }
    let text = String::from_utf8_lossy(body);
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line: String = line.trim().chars().take(200).collect();
    if line.is_empty() {
        String::new()
    } else {
        format!(": {line}")
    }
}

/// git-lfs's standalone-file layout for a local or `file://` remote.
pub struct FileTransfer {
    endpoint: LfsEndpoint,
    objects: PathBuf,
    tmp: PathBuf,
}

static FILE_TMP: AtomicU64 = AtomicU64::new(0);

impl FileTransfer {
    fn local(&self, what: &str, e: io::Error) -> LfsError {
        LfsError::new(
            PushFailureKind::Unreachable,
            format!("LFS {what} at {}: {e}", self.endpoint.display_url),
        )
    }

    /// Copies one cached object into the remote's objects directory,
    /// unless it is there already.
    fn put(&self, pointer: &Pointer, cache: &ObjectCache) -> Result<bool, LfsError> {
        if self.has(pointer) {
            return Ok(false);
        }
        let mut src = cache.open_object(pointer).map_err(|e| {
            LfsError::local(format!(
                "LFS object {} is not in the local cache ({e}); it cannot be uploaded",
                pointer.oid
            ))
        })?;
        fs::create_dir_all(&self.tmp).map_err(|e| self.local("upload", e))?;
        fs::create_dir_all(&self.objects).map_err(|e| self.local("upload", e))?;
        let tmp = self.tmp.join(format!(
            "{}-{}-{}",
            pointer.oid,
            std::process::id(),
            FILE_TMP.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(&tmp)?;
            let copied = io::copy(&mut src, &mut out)?;
            if copied != pointer.size {
                return Err(io::Error::other(format!(
                    "copied {copied} bytes of object {}, not {}",
                    pointer.oid, pointer.size
                )));
            }
            out.sync_all()?;
            publish_into(&self.objects, &tmp, &pointer.oid)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(self.local("upload", e));
        }
        Ok(true)
    }
}

impl FileTransfer {
    fn get(&self, pointer: &Pointer, cache: &ObjectCache) -> Result<(), LfsError> {
        let path = object_path(&self.objects, &pointer.oid);
        let mut file: Box<dyn Read> = Box::new(File::open(&path).map_err(|e| {
            LfsError::new(
                PushFailureKind::Refused,
                format!(
                    "LFS object {} is not at {}: {e}",
                    pointer.oid, self.endpoint.display_url
                ),
            )
        })?);
        cache.insert_verified(pointer, &mut file)
    }

    /// Whether the remote holds the object with the pointer's size.
    fn has(&self, pointer: &Pointer) -> bool {
        fs::metadata(object_path(&self.objects, &pointer.oid))
            .is_ok_and(|m| m.is_file() && m.len() == pointer.size)
    }
}

impl Transfer for FileTransfer {
    fn upload(
        &self,
        objects: &[Pointer],
        cache: &ObjectCache,
        _ref_name: &str,
    ) -> Result<UploadReport, LfsError> {
        let mut report = UploadReport {
            objects: objects.len(),
            ..UploadReport::default()
        };
        let missing: Vec<&Pointer> = objects
            .iter()
            .filter(|p| !self.has(p) && !cache.contains(p))
            .collect();
        if !missing.is_empty() {
            return Err(missing_from_cache(&missing, &self.endpoint));
        }
        for pointer in objects {
            if self.put(pointer, cache)? {
                report.uploaded += 1;
                report.bytes += pointer.size;
            }
        }
        Ok(report)
    }

    fn download_many(
        &self,
        pointers: &[Pointer],
        cache: &ObjectCache,
        _ref_name: &str,
    ) -> Vec<Result<(), LfsError>> {
        pointers
            .iter()
            .map(|p| {
                if cache.contains(p) {
                    Ok(())
                } else {
                    self.get(p, cache)
                }
            })
            .collect()
    }

    fn present(
        &self,
        pointers: &[Pointer],
        _direction: Direction,
        _ref_name: &str,
    ) -> Result<Vec<bool>, LfsError> {
        Ok(pointers.iter().map(|p| self.has(p)).collect())
    }

    fn endpoint(&self) -> &LfsEndpoint {
        &self.endpoint
    }
}

#[cfg(test)]
mod tests;
