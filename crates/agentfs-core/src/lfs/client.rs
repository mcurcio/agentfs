//! Moving LFS objects between the local cache and a remote's LFS endpoint
//! (design §7).
//!
//! - [`HttpTransfer`]: the git-lfs batch API (`POST <endpoint>/objects/batch`)
//!   with the `basic` transfer adapter only, over `ureq`: uploads (a `PUT`
//!   of the cached object, then `verify` when the server asks for it) and
//!   downloads (a `GET`, hashed as it is written into the cache).
//!   Sequential, in batches of [`BATCH_SIZE`].
//! - [`FileTransfer`]: git-lfs's standalone-file layout for local and
//!   `file://` remotes, `<git dir>/lfs/objects/aa/bb/<oid>`.
//!
//! **Credentials** ([`super::endpoint`]) go only to the endpoint's origin:
//! agentfs's `Authorization` header is added to batch requests, and to an
//! action's `href` only when it has the endpoint's scheme, host and port
//! and the server gave the action no `Authorization` of its own. The
//! headers a server gives an action are sent as given. Redirects are never
//! followed and proxy environment variables are ignored.
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
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;

use super::cache::{ObjectCache, object_path, publish_into};
use super::endpoint::{Direction, EndpointAuth, EndpointKind, LfsEndpoint, resolve};
use super::{LfsError, Pointer};
use crate::config::{RemoteSettings, Settings};
use crate::credentials::{Credential, CredentialFile};
use crate::events::PushFailureKind;
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

/// One remote's LFS storage.
pub trait Transfer: Send {
    /// Makes sure the server has every object in `objects` (all in
    /// `cache`), uploading those it lacks. `ref_name` is the ref being
    /// pushed, for servers that authorize by ref.
    fn upload(
        &self,
        objects: &[Pointer],
        cache: &ObjectCache,
        ref_name: &str,
    ) -> Result<UploadReport, LfsError>;

    /// Downloads `pointer`'s object into `cache`, checking its hash.
    fn download(
        &self,
        pointer: &Pointer,
        cache: &ObjectCache,
        ref_name: &str,
    ) -> Result<(), LfsError>;

    /// Where the objects go.
    fn endpoint(&self) -> &LfsEndpoint;
}

/// The transfer for `remote`'s endpoint in `direction`.
pub fn transfer_for(
    settings: &Settings,
    remote: &RemoteSettings,
    direction: Direction,
) -> Result<Box<dyn Transfer>, LfsError> {
    let endpoint = resolve(settings, remote, direction)?;
    match &endpoint.kind {
        EndpointKind::Http { .. } => Ok(Box::new(HttpTransfer::new(endpoint, &remote.name)?)),
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
}

/// The batch API over HTTP(S).
pub struct HttpTransfer {
    endpoint: LfsEndpoint,
    /// The endpoint URL, without userinfo or a final `/`.
    url: String,
    origin: (String, String),
    credentials: Credentials,
    agent: ureq::Agent,
    remote: String,
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

#[derive(Debug, Deserialize)]
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
    /// A client for `endpoint`, an [`EndpointKind::Http`].
    pub fn new(endpoint: LfsEndpoint, remote: &str) -> Result<Self, LfsError> {
        let EndpointKind::Http { url, auth } = &endpoint.kind else {
            return Err(LfsError::local("not an http(s) LFS endpoint"));
        };
        let url = url.trim_end_matches('/').to_owned();
        let Some(origin) = remote_url::origin(&url) else {
            return Err(LfsError::local(format!(
                "{} is not an http(s) URL",
                endpoint.display_url
            )));
        };
        if origin.0 == "https" && !cfg!(feature = "https") {
            return Err(LfsError::local(format!(
                "the LFS endpoint {} is https, and agentfs was built without the `https` \
                 cargo feature",
                endpoint.display_url
            )));
        }
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
        Ok(Self {
            agent: agent(),
            url,
            origin,
            credentials,
            remote: remote.to_owned(),
            endpoint,
        })
    }

    fn display(&self) -> &str {
        &self.endpoint.display_url
    }

    /// The `Authorization` header for the endpoint's origin, if any.
    fn authorization(&self) -> Result<Option<String>, LfsError> {
        let credential = match &self.credentials {
            Credentials::None => return Ok(None),
            Credentials::Fixed(c) => c.clone(),
            Credentials::File { key, file } => file
                .credential_for(&self.url, remote_url::url_username(&self.url))
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
    fn action_headers(&self, action: &Action) -> Result<Vec<(String, String)>, LfsError> {
        let mut headers: Vec<(String, String)> = action
            .header
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let has_auth = headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
        let same_origin = remote_url::origin(&action.href).as_ref() == Some(&self.origin);
        if !has_auth
            && same_origin
            && let Some(auth) = self.authorization()?
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

    fn batch(
        &self,
        operation: &str,
        objects: &[Pointer],
        ref_name: &str,
    ) -> Result<BatchResponse, LfsError> {
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
            .post(format!("{}/objects/batch", self.url))
            .header("Accept", MEDIA_TYPE)
            .header("Content-Type", MEDIA_TYPE);
        if let Some(auth) = self.authorization()? {
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
            return Err(self.status_error("batch request", status, &bytes));
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
        Ok(parsed)
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

    fn put(&self, pointer: &Pointer, action: &Action, cache: &ObjectCache) -> Result<(), LfsError> {
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
        for (k, v) in self.action_headers(action)? {
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
            return Err(self.status_error(&format!("upload of {}", pointer.oid), status, &body));
        }
        Ok(())
    }

    fn verify(&self, pointer: &Pointer, action: &Action) -> Result<(), LfsError> {
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
        for (k, v) in self.action_headers(action)? {
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
            return Err(self.status_error(&format!("verify of {}", pointer.oid), status, &body));
        }
        Ok(())
    }

    fn get(&self, pointer: &Pointer, action: &Action, cache: &ObjectCache) -> Result<(), LfsError> {
        self.check_href(&action.href)?;
        let mut request = self.agent.get(&action.href);
        for (k, v) in self.action_headers(action)? {
            request = request.header(k, v);
        }
        let response = request
            .config()
            .timeout_global(Some(object_timeout(pointer.size)))
            .build()
            .call()
            .map_err(|e| self.transport_error("download", e))?;
        let status = response.status().as_u16();
        if status != 200 {
            let body = read_small(response);
            return Err(self.status_error(&format!("download of {}", pointer.oid), status, &body));
        }
        let mut reader = response.into_body().into_reader();
        cache.insert_verified(pointer, &mut reader)
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

    fn status_error(&self, what: &str, status: u16, body: &[u8]) -> LfsError {
        let kind = match status {
            401 | 403 => PushFailureKind::Auth,
            429 | 500..=599 => PushFailureKind::Unreachable,
            _ => PushFailureKind::Refused,
        };
        let detail = server_message(body);
        let hint = match (kind, &self.credentials) {
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

impl Transfer for HttpTransfer {
    fn upload(
        &self,
        objects: &[Pointer],
        cache: &ObjectCache,
        ref_name: &str,
    ) -> Result<UploadReport, LfsError> {
        let mut report = UploadReport {
            objects: objects.len(),
            ..UploadReport::default()
        };
        for chunk in objects.chunks(BATCH_SIZE) {
            let response = self.batch("upload", chunk, ref_name)?;
            for pointer in chunk {
                let object = self.object(&response, pointer)?;
                let actions = object.actions.as_ref();
                // No upload action: the server has it already.
                let Some(upload) = actions.and_then(|a| a.upload.as_ref()) else {
                    continue;
                };
                self.put(pointer, upload, cache)?;
                if let Some(verify) = actions.and_then(|a| a.verify.as_ref()) {
                    self.verify(pointer, verify)?;
                }
                report.uploaded += 1;
                report.bytes += pointer.size;
            }
        }
        Ok(report)
    }

    fn download(
        &self,
        pointer: &Pointer,
        cache: &ObjectCache,
        ref_name: &str,
    ) -> Result<(), LfsError> {
        let response = self.batch("download", std::slice::from_ref(pointer), ref_name)?;
        let object = self.object(&response, pointer)?;
        let Some(download) = object.actions.as_ref().and_then(|a| a.download.as_ref()) else {
            return Err(LfsError::new(
                PushFailureKind::Refused,
                format!(
                    "the LFS server at {} offered no download of object {}",
                    self.display(),
                    pointer.oid
                ),
            ));
        };
        self.get(pointer, download, cache)
    }

    fn endpoint(&self) -> &LfsEndpoint {
        &self.endpoint
    }
}

/// The HTTP client: no redirects, no proxy, the timeouts above, and with
/// the `https` feature native-tls over the system trust store, as libgit2
/// uses.
fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None)
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

/// Up to 4 KiB of an error response's body.
fn read_small(response: ureq::http::Response<ureq::Body>) -> Vec<u8> {
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

    /// Copies one cached object into the remote's objects directory.
    fn put(&self, pointer: &Pointer, cache: &ObjectCache) -> Result<bool, LfsError> {
        let dest = object_path(&self.objects, &pointer.oid);
        if fs::metadata(&dest).is_ok_and(|m| m.is_file() && m.len() == pointer.size) {
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
        for pointer in objects {
            if self.put(pointer, cache)? {
                report.uploaded += 1;
                report.bytes += pointer.size;
            }
        }
        Ok(report)
    }

    fn download(
        &self,
        pointer: &Pointer,
        cache: &ObjectCache,
        _ref_name: &str,
    ) -> Result<(), LfsError> {
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

    fn endpoint(&self) -> &LfsEndpoint {
        &self.endpoint
    }
}

#[cfg(test)]
mod tests;
