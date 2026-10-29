//! Where a remote's LFS objects go, and with which credentials (design
//! §7, §11.1).
//!
//! **Endpoints** follow git-lfs's precedence, from operator config only
//! (never a tracked `.lfsconfig`):
//!
//! | Uploads | Downloads |
//! |---|---|
//! | `lfs.pushurl` | |
//! | `lfs.url` | `lfs.url` |
//! | `remote.<name>.lfspushurl` | |
//! | `remote.<name>.lfsurl` | `remote.<name>.lfsurl` |
//! | derived from `pushurl`, else `url` | derived from `url` |
//!
//! A flagged fork remote (`agentfsForkRemote`, design §11.5) reads its own
//! `lfspushurl` and `lfsurl` before the global keys: it must name one while
//! `lfs.url` or `lfs.pushurl` is set (the section is invalid otherwise), so
//! a fork's LFS objects never reach the source's store.
//!
//! An explicit `http(s)` URL is the endpoint as written. A remote URL
//! derives one as git-lfs does: `<url>/info/lfs` when it ends in `.git`,
//! else `<url>.git/info/lfs`. A local path or `file://` URL (explicit or
//! derived) uses git-lfs's standalone-file layout, the objects directory
//! of that repository (`<git dir>/lfs/objects`). An SSH remote's endpoint
//! is the one `git-lfs-authenticate` names ([`super::ssh_auth`]), over the
//! push URL for uploads and `url` (on the same host and port) for
//! downloads; `git://` remotes derive nothing and need an explicit
//! `lfsurl`.
//!
//! **Credentials** are bound to the endpoint's origin (scheme, host and
//! port) and offered nowhere else:
//!
//! 1. `remote.<name>.agentfsLfsCredentials`, if set;
//! 2. else, when the endpoint has the push URL's origin (as GitHub's
//!    derived endpoints do), `remote.<name>.agentfsCredentials`;
//! 3. else a password in the endpoint URL;
//! 4. else none.
//!
//! So a separate LFS host never sees the git host's credentials, and an
//! `http` endpoint never gets those of an `https` push URL.

use std::path::{Path, PathBuf};

use super::LfsError;
use crate::config::{RemoteSettings, Settings};
use crate::push::redact_url;
use crate::remote_url::{self, Transport};

/// Which way objects move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Before a push.
    Upload,
    /// For a restore.
    Download,
}

impl Direction {
    /// The batch API's `operation`.
    pub fn operation(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Download => "download",
        }
    }
}

/// What authenticates requests to an HTTP endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointAuth {
    /// Anonymous.
    None,
    /// A credential file, re-read when it changes.
    File { key: &'static str, path: PathBuf },
    /// The password in the endpoint URL.
    Url { username: String, password: String },
}

/// How an endpoint is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointKind {
    /// The batch API at `url` (without userinfo).
    Http { url: String, auth: EndpointAuth },
    /// git-lfs's standalone-file layout: `<git_dir>/lfs/objects`.
    File { git_dir: PathBuf },
    /// The batch API at the endpoint `git-lfs-authenticate <path>` names
    /// when run over `ssh_url` ([`super::ssh_auth`]), with the header it
    /// gives, else `agentfsLfsCredentials` bound to that endpoint's origin.
    SshAuthenticate {
        ssh_url: String,
        path: String,
        lfs_credentials: Option<PathBuf>,
    },
}

/// A remote's LFS endpoint for one direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsEndpoint {
    /// The endpoint URL or path, with any userinfo redacted.
    pub display_url: String,
    /// Where it came from: a config key, or `derived from remote.<n>.url`.
    pub source: String,
    pub kind: EndpointKind,
}

impl LfsEndpoint {
    /// A one-line description for `agentfs status` and messages.
    pub fn describe(&self) -> String {
        let auth = match &self.kind {
            EndpointKind::File { .. } => String::new(),
            EndpointKind::SshAuthenticate {
                lfs_credentials, ..
            } => {
                let fallback = if lfs_credentials.is_some() {
                    ", else agentfsLfsCredentials"
                } else {
                    ""
                };
                return format!(
                    "git-lfs-authenticate over {} ({}; credentials from git-lfs-authenticate{fallback})",
                    self.display_url, self.source
                );
            }
            EndpointKind::Http { auth, .. } => match auth {
                EndpointAuth::None => ", anonymous".to_owned(),
                EndpointAuth::File { key, .. } => format!(", credentials from {key}"),
                EndpointAuth::Url { .. } => ", credentials from the URL".to_owned(),
            },
        };
        format!("{} ({}{auth})", self.display_url, self.source)
    }
}

/// `remote`'s LFS endpoint for `direction` (see the module documentation).
pub fn resolve(
    settings: &Settings,
    remote: &RemoteSettings,
    direction: Direction,
) -> Result<LfsEndpoint, LfsError> {
    let name = &remote.name;
    let upload = direction == Direction::Upload;
    let global = [
        (upload, "lfs.pushurl".to_owned(), &settings.lfs.push_url),
        (true, "lfs.url".to_owned(), &settings.lfs.url),
    ];
    let own = [
        (
            upload,
            format!("remote.{name}.lfspushurl"),
            &remote.lfs_push_url,
        ),
        (true, format!("remote.{name}.lfsurl"), &remote.lfs_url),
    ];
    let explicit = if remote.fork_remote {
        [own, global].concat()
    } else {
        [global, own].concat()
    };
    for (applies, key, value) in explicit {
        if let (true, Some(url)) = (applies, value) {
            return endpoint(remote, key, url.trim_end_matches('/').to_owned());
        }
    }
    let (key, url) = match direction {
        Direction::Upload if remote.push_url.is_some() => ("pushurl", remote.push_url()),
        _ => ("url", remote.url.as_str()),
    };
    let source = format!("derived from remote.{name}.{key}");
    match remote_url::transport(url) {
        Transport::Http | Transport::Https => {
            let base = url.trim_end_matches('/');
            let derived = if base.ends_with(".git") {
                format!("{base}/info/lfs")
            } else {
                format!("{base}.git/info/lfs")
            };
            endpoint(remote, source, derived)
        }
        Transport::Local => endpoint(remote, source, url.to_owned()),
        Transport::Ssh => ssh_authenticate(remote, source, url),
        Transport::Other => Err(LfsError::local(format!(
            "remote {name} pushes over git:// or an unknown scheme, which has no LFS endpoint \
             agentfs can derive; set remote.{name}.lfsurl to the server's https LFS endpoint"
        ))),
    }
}

/// The endpoint `git-lfs-authenticate` names over the SSH URL `url` (the
/// push URL for uploads, `url` for downloads), which must be on the push
/// URL's host and port: the SSH transport is bound to them.
fn ssh_authenticate(
    remote: &RemoteSettings,
    source: String,
    url: &str,
) -> Result<LfsEndpoint, LfsError> {
    let name = &remote.name;
    // The SSH user is shown; a password (never valid for SSH) is not.
    let display_url = if remote_url::has_password(url) {
        redact_url(url)
    } else {
        url.to_owned()
    };
    let on_push_host = remote_url::ssh_endpoint(url).is_some()
        && remote_url::ssh_endpoint(url) == remote_url::ssh_endpoint(remote.push_url());
    if !on_push_host {
        return Err(LfsError::local(format!(
            "{display_url} ({source}) is not on the host and port of remote {name}'s push URL, \
             where its SSH key is used; set remote.{name}.lfsurl"
        )));
    }
    let Some(path) = super::ssh_auth::lfs_path(url) else {
        return Err(LfsError::local(format!(
            "{display_url} ({source}) has no repository path for git-lfs-authenticate; set \
             remote.{name}.lfsurl"
        )));
    };
    Ok(LfsEndpoint {
        display_url,
        source,
        kind: EndpointKind::SshAuthenticate {
            ssh_url: url.to_owned(),
            path,
            lfs_credentials: remote.lfs_credentials.clone(),
        },
    })
}

/// The endpoint `url` (an explicit key's value, or derived from the remote
/// URL), with its credentials.
fn endpoint(remote: &RemoteSettings, source: String, url: String) -> Result<LfsEndpoint, LfsError> {
    let display_url = redact_url(&url);
    match remote_url::transport(&url) {
        Transport::Http | Transport::Https => {
            let auth = auth_for(remote, &url);
            Ok(LfsEndpoint {
                display_url,
                source,
                kind: EndpointKind::Http {
                    url: remote_url::without_userinfo(&url),
                    auth,
                },
            })
        }
        Transport::Local => {
            let path = local_path(&remote_url::without_userinfo(&url));
            let git_dir = git_dir_of(&path).map_err(|e| {
                // libgit2 names the path; never echo a userinfo it held.
                let reason = e.message().replace(&*path.to_string_lossy(), "the path");
                LfsError::local(format!(
                    "the LFS endpoint {display_url} ({source}) is not a git repository agentfs \
                     can open: {reason}"
                ))
            })?;
            Ok(LfsEndpoint {
                display_url,
                source,
                kind: EndpointKind::File { git_dir },
            })
        }
        Transport::Ssh | Transport::Other => Err(LfsError::local(format!(
            "{source}: {display_url} is not an http(s) or file LFS endpoint"
        ))),
    }
}

/// The credentials for the HTTP endpoint `url` (see the module docs).
fn auth_for(remote: &RemoteSettings, url: &str) -> EndpointAuth {
    if let Some(path) = &remote.lfs_credentials {
        return EndpointAuth::File {
            key: "agentfsLfsCredentials",
            path: path.clone(),
        };
    }
    let same_origin = remote_url::origin(url).is_some()
        && remote_url::origin(url) == remote_url::origin(remote.push_url());
    if same_origin && let Some(path) = &remote.credentials {
        return EndpointAuth::File {
            key: "agentfsCredentials",
            path: path.clone(),
        };
    }
    if let Some(password) = remote_url::url_password(url) {
        return EndpointAuth::Url {
            username: remote_url::url_username(url)
                .unwrap_or(crate::credentials::DEFAULT_USERNAME)
                .to_owned(),
            password,
        };
    }
    EndpointAuth::None
}

/// The filesystem path of a local remote: a path, or a `file://` URL.
fn local_path(url: &str) -> PathBuf {
    match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("file") => {
            // `file:///srv/x` and `file://localhost/srv/x`.
            let path = rest.strip_prefix("localhost").unwrap_or(rest);
            PathBuf::from(path)
        }
        _ => PathBuf::from(url),
    }
}

/// The git directory of the repository at `path` (bare: `path` itself;
/// with a working tree: its `.git`).
fn git_dir_of(path: &Path) -> Result<PathBuf, git2::Error> {
    crate::history::init_libgit2();
    let repo = git2::Repository::open_ext(
        path,
        git2::RepositoryOpenFlags::NO_SEARCH,
        std::iter::empty::<&std::ffi::OsStr>(),
    )?;
    Ok(repo.path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LfsSettings;

    fn remote(url: &str) -> RemoteSettings {
        RemoteSettings {
            name: "origin".into(),
            url: url.into(),
            push_url: None,
            max_blob_size: None,
            max_push_size: None,
            max_lfs_object_size: None,
            proxy: None,
            credentials: None,
            ssh_key: None,
            known_hosts: None,
            lfs_url: None,
            lfs_push_url: None,
            lfs_credentials: None,
            ssh_agent: None,
            ssh_key_passphrase: None,
            fork_remote: false,
            push_source: crate::config::PushSource::Branch,
        }
    }

    fn settings(lfs: LfsSettings) -> Settings {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config"), "[agentfs]\n\tvolume = v\n").unwrap();
        let mut s = Settings::load(dir.path()).unwrap();
        s.lfs = lfs;
        s
    }

    fn http_url(e: &LfsEndpoint) -> &str {
        match &e.kind {
            EndpointKind::Http { url, .. } => url,
            other => panic!("{other:?}"),
        }
    }

    fn auth(e: &LfsEndpoint) -> &EndpointAuth {
        match &e.kind {
            EndpointKind::Http { auth, .. } => auth,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn derived_endpoints() {
        let s = settings(LfsSettings::default());
        for (url, expected) in [
            (
                "https://github.com/acme/r.git",
                "https://github.com/acme/r.git/info/lfs",
            ),
            (
                "https://github.com/acme/r",
                "https://github.com/acme/r.git/info/lfs",
            ),
            (
                "https://github.com/acme/r/",
                "https://github.com/acme/r.git/info/lfs",
            ),
            (
                "http://h:8080/git/r.git/",
                "http://h:8080/git/r.git/info/lfs",
            ),
            ("https://u:secret@h/r.git", "https://h/r.git/info/lfs"),
        ] {
            let e = resolve(&s, &remote(url), Direction::Upload).unwrap();
            assert_eq!(http_url(&e), expected, "{url}");
            assert!(!e.display_url.contains("secret"), "{e:?}");
            assert_eq!(e.source, "derived from remote.origin.url");
        }
        // pushurl for uploads, url for downloads.
        let mut r = remote("https://read.example/r");
        r.push_url = Some("https://write.example/r".into());
        let up = resolve(&s, &r, Direction::Upload).unwrap();
        assert_eq!(http_url(&up), "https://write.example/r.git/info/lfs");
        assert_eq!(up.source, "derived from remote.origin.pushurl");
        let down = resolve(&s, &r, Direction::Download).unwrap();
        assert_eq!(http_url(&down), "https://read.example/r.git/info/lfs");
    }

    #[test]
    fn precedence_follows_git_lfs() {
        let all = LfsSettings {
            threshold: None,
            url: Some("http://global/url".into()),
            push_url: Some("http://global/push".into()),
            ..LfsSettings::default()
        };
        let mut r = remote("https://h/r.git");
        r.lfs_url = Some("http://remote/url".into());
        r.lfs_push_url = Some("http://remote/push".into());
        let pick = |lfs: &LfsSettings, r: &RemoteSettings, d| {
            http_url(&resolve(&settings(lfs.clone()), r, d).unwrap()).to_owned()
        };
        use Direction::{Download, Upload};
        assert_eq!(pick(&all, &r, Upload), "http://global/push");
        assert_eq!(pick(&all, &r, Download), "http://global/url");
        let no_push = LfsSettings {
            push_url: None,
            ..all.clone()
        };
        assert_eq!(pick(&no_push, &r, Upload), "http://global/url");
        let none = LfsSettings::default();
        assert_eq!(pick(&none, &r, Upload), "http://remote/push");
        assert_eq!(pick(&none, &r, Download), "http://remote/url");
        r.lfs_push_url = None;
        assert_eq!(pick(&none, &r, Upload), "http://remote/url");
        r.lfs_url = None;
        assert_eq!(pick(&none, &r, Upload), "https://h/r.git/info/lfs");
        // lfs.pushurl never applies to downloads.
        let only_push = LfsSettings {
            push_url: Some("http://global/push".into()),
            ..LfsSettings::default()
        };
        assert_eq!(pick(&only_push, &r, Download), "https://h/r.git/info/lfs");
        // An explicit URL is used as written (no /info/lfs), less a final /.
        r.lfs_url = Some("http://lfs.example/acme/sep/".into());
        assert_eq!(pick(&none, &r, Upload), "http://lfs.example/acme/sep");
    }

    /// A flagged fork remote's own endpoints come before the global ones,
    /// which name the source's store (design §11.5).
    #[test]
    fn a_fork_remote_prefers_its_own_endpoints() {
        let all = LfsSettings {
            threshold: None,
            url: Some("http://source/url".into()),
            push_url: Some("http://source/push".into()),
            ..LfsSettings::default()
        };
        let mut r = remote("https://h/forks.git");
        r.fork_remote = true;
        r.lfs_push_url = Some("http://forks/push".into());
        let pick = |r: &RemoteSettings, d| {
            http_url(&resolve(&settings(all.clone()), r, d).unwrap()).to_owned()
        };
        assert_eq!(pick(&r, Direction::Upload), "http://forks/push");
        r.lfs_push_url = None;
        r.lfs_url = Some("http://forks/url".into());
        assert_eq!(pick(&r, Direction::Upload), "http://forks/url");
        assert_eq!(pick(&r, Direction::Download), "http://forks/url");
    }

    #[test]
    fn local_remotes_use_the_standalone_file_layout() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("bare.git");
        git2::Repository::init_bare(&bare).unwrap();
        let work = dir.path().join("work");
        git2::Repository::init(&work).unwrap();
        let s = settings(LfsSettings::default());
        for (url, git_dir) in [
            (bare.display().to_string(), bare.clone()),
            (format!("file://{}", bare.display()), bare.clone()),
            (work.display().to_string(), work.join(".git")),
        ] {
            let e = resolve(&s, &remote(&url), Direction::Upload).unwrap();
            match e.kind {
                EndpointKind::File { git_dir: got } => {
                    assert_eq!(got.canonicalize().unwrap(), git_dir.canonicalize().unwrap())
                }
                other => panic!("{url}: {other:?}"),
            }
        }
        let err = resolve(&s, &remote("/nonexistent/r.git"), Direction::Upload).unwrap_err();
        assert_eq!(err.kind, crate::events::PushFailureKind::Local);
        assert!(err.message.contains("not a git repository"), "{err}");
        // Userinfo in a file URL is ignored, and never echoed.
        let url = format!("file://u:secret@{}", bare.display());
        assert!(resolve(&s, &remote(&url), Direction::Upload).is_ok());
        let err = resolve(
            &s,
            &remote("file://u:secret@/nonexistent/r.git"),
            Direction::Upload,
        )
        .unwrap_err();
        assert!(!err.message.contains("secret"), "{err}");
    }

    #[test]
    fn ssh_remotes_use_git_lfs_authenticate() {
        let s = settings(LfsSettings::default());
        for (url, path) in [
            ("ssh://git@h/acme/r.git", "acme/r.git"),
            ("git@h:acme/r.git", "acme/r.git"),
            ("ssh://git@h:2222/home/git/r.git", "home/git/r.git"),
        ] {
            let e = resolve(&s, &remote(url), Direction::Upload).unwrap();
            assert_eq!(
                e.kind,
                EndpointKind::SshAuthenticate {
                    ssh_url: url.into(),
                    path: path.into(),
                    lfs_credentials: None,
                },
                "{url}"
            );
            assert_eq!(e.source, "derived from remote.origin.url");
            assert!(
                e.describe()
                    .starts_with(&format!("git-lfs-authenticate over {url}")),
                "{}",
                e.describe()
            );
        }
        // Uploads use the push URL; downloads `url`, which must be on the
        // push URL's host and port.
        let mut r = remote("ssh://git@h/read/r.git");
        r.push_url = Some("ssh://git@h/write/r.git".into());
        let up = resolve(&s, &r, Direction::Upload).unwrap();
        assert!(
            matches!(&up.kind, EndpointKind::SshAuthenticate { path, .. } if path == "write/r.git")
        );
        let down = resolve(&s, &r, Direction::Download).unwrap();
        assert!(
            matches!(&down.kind, EndpointKind::SshAuthenticate { path, .. } if path == "read/r.git")
        );
        r.url = "ssh://git@elsewhere/read/r.git".into();
        let err = resolve(&s, &r, Direction::Download).unwrap_err();
        assert_eq!(err.kind, crate::events::PushFailureKind::Local);
        assert!(err.message.contains("set remote.origin.lfsurl"), "{err}");
        // git:// derives nothing.
        let err = resolve(&s, &remote("git://h/r.git"), Direction::Upload).unwrap_err();
        assert!(err.message.contains("set remote.origin.lfsurl"), "{err}");
        // An explicit endpoint wins.
        let mut r = remote("ssh://git@h/r.git");
        r.lfs_url = Some("https://h/r.git/info/lfs".into());
        assert_eq!(
            http_url(&resolve(&s, &r, Direction::Upload).unwrap()),
            "https://h/r.git/info/lfs"
        );
        // The LFS credential file is the fallback when the answer has no
        // header.
        let mut r = remote("ssh://git@h/r.git");
        r.lfs_credentials = Some("/run/secrets/lfs".into());
        let e = resolve(&s, &r, Direction::Upload).unwrap();
        assert!(
            e.describe().contains("else agentfsLfsCredentials"),
            "{}",
            e.describe()
        );
    }

    #[test]
    fn credentials_are_bound_to_their_origin() {
        let s = settings(LfsSettings::default());
        let mut r = remote("https://git.example/r.git");
        r.credentials = Some("/run/secrets/git".into());
        // A derived endpoint on the push URL's origin reuses the git
        // credentials.
        let e = resolve(&s, &r, Direction::Upload).unwrap();
        assert_eq!(
            auth(&e),
            &EndpointAuth::File {
                key: "agentfsCredentials",
                path: "/run/secrets/git".into()
            }
        );
        assert!(
            e.describe().contains("credentials from agentfsCredentials"),
            "{}",
            e.describe()
        );
        // Another host, another port, or http for an https push URL: never.
        for url in [
            "https://lfs.example/r",
            "https://git.example:8443/r",
            "http://git.example/r",
        ] {
            r.lfs_url = Some(url.into());
            let e = resolve(&s, &r, Direction::Upload).unwrap();
            assert_eq!(auth(&e), &EndpointAuth::None, "{url}");
        }
        // The explicit default port is the same origin.
        r.lfs_url = Some("https://git.example:443/r/info/lfs".into());
        let e = resolve(&s, &r, Direction::Upload).unwrap();
        assert!(matches!(
            auth(&e),
            EndpointAuth::File {
                key: "agentfsCredentials",
                ..
            }
        ));
        // The LFS credential file, wherever the endpoint is.
        r.lfs_url = Some("https://lfs.example/r".into());
        r.lfs_credentials = Some("/run/secrets/lfs".into());
        let e = resolve(&s, &r, Direction::Upload).unwrap();
        assert_eq!(
            auth(&e),
            &EndpointAuth::File {
                key: "agentfsLfsCredentials",
                path: "/run/secrets/lfs".into()
            }
        );
        // A password in the URL, used only without a file.
        r.lfs_credentials = None;
        r.lfs_url = Some("https://bob:pw%21@lfs.example/r".into());
        let e = resolve(&s, &r, Direction::Upload).unwrap();
        assert_eq!(
            auth(&e),
            &EndpointAuth::Url {
                username: "bob".into(),
                password: "pw!".into()
            }
        );
        assert_eq!(http_url(&e), "https://lfs.example/r");
        assert!(!e.display_url.contains("pw"), "{}", e.display_url);
    }
}
