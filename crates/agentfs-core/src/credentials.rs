//! Credential files for pushing (design §11.1).
//!
//! `remote.<name>.agentfsCredentials` names an absolute path holding either
//!
//! - git-credential `key=value` lines (`username=…`, `password=…`; other
//!   keys are ignored), or
//! - a bare token, used as the password (one trailing newline is dropped).
//!
//! The file is re-read whenever its stat stamp changes
//! ([`crate::watched`]), so whatever rotates short-lived tokens only has to
//! rewrite or replace it.
//!
//! A credential is bound to the host of the remote URL it was configured
//! for and is never handed to any other host, whatever the transport is
//! redirected to.

use std::io;
use std::path::{Path, PathBuf};

pub use crate::remote_url::authority;
use crate::watched::{WatchedError, WatchedFile};

/// The largest credential file read.
pub const MAX_CREDENTIAL_FILE: u64 = 64 << 10;

/// The username used with a bare token when neither the file nor the URL
/// names one. GitHub and Gitea accept any username with a token.
pub const DEFAULT_USERNAME: &str = "x-access-token";

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("reading credentials {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("credential file is empty")]
    Empty,
    #[error("credential file has no password= line")]
    MissingPassword,
    #[error("credentials are bound to host {expected:?}, not {got:?}")]
    HostMismatch {
        expected: Option<String>,
        got: Option<String>,
    },
}

/// A username and password (or token) for one connection.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The file's content, parsed.
#[derive(Clone)]
struct Parsed {
    username: Option<String>,
    password: String,
}

/// One remote's credential file.
pub struct CredentialFile {
    file: WatchedFile<Parsed>,
    bound: Option<String>,
}

impl CredentialFile {
    /// The file at `path`, bound to the host of `bound_url`.
    pub fn new(path: impl Into<PathBuf>, bound_url: &str) -> Self {
        Self {
            file: WatchedFile::new(path, MAX_CREDENTIAL_FILE),
            bound: authority(bound_url),
        }
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// The credential for a connection to `url`. `username_from_url` is the
    /// user the URL names, if any. Refuses any host other than the one the
    /// file is bound to.
    pub fn credential_for(
        &self,
        url: &str,
        username_from_url: Option<&str>,
    ) -> Result<Credential, CredentialError> {
        check_bound(self.bound.as_deref(), url)?;
        let parsed = self.load()?;
        let username = parsed
            .username
            .or_else(|| {
                username_from_url
                    .filter(|u| !u.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| DEFAULT_USERNAME.to_owned());
        Ok(Credential {
            username,
            password: parsed.password,
        })
    }

    /// How many times the file has been read and parsed.
    pub fn reloads(&self) -> usize {
        self.file.reloads()
    }

    fn load(&self) -> Result<Parsed, CredentialError> {
        let path = self.path();
        self.file
            .load(|bytes| parse(&utf8(path, bytes)?))
            .map_err(|e| flatten(path, e))
    }
}

/// Refuses a URL whose authority is not the one a file is bound to.
fn check_bound(bound: Option<&str>, url: &str) -> Result<(), CredentialError> {
    let got = authority(url);
    if got.as_deref() != bound {
        return Err(CredentialError::HostMismatch {
            expected: bound.map(str::to_owned),
            got,
        });
    }
    Ok(())
}

/// The content as UTF-8; anything else is an IO error, as for a file that
/// cannot be read.
fn utf8(path: &Path, bytes: &[u8]) -> Result<String, CredentialError> {
    String::from_utf8(bytes.to_vec()).map_err(|e| CredentialError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, e),
    })
}

/// A [`WatchedError`] as a [`CredentialError`].
fn flatten(path: &Path, e: WatchedError<CredentialError>) -> CredentialError {
    match e {
        WatchedError::Io(source) => CredentialError::Io {
            path: path.to_path_buf(),
            source,
        },
        WatchedError::Parse(e) => e,
    }
}

fn parse(content: &str) -> Result<Parsed, CredentialError> {
    if content.trim().is_empty() {
        return Err(CredentialError::Empty);
    }
    let lines = || content.lines().map(|l| l.strip_suffix('\r').unwrap_or(l));
    let key_value = lines().any(|l| l.starts_with("username=") || l.starts_with("password="));
    if !key_value {
        let token = content.strip_suffix('\n').unwrap_or(content);
        let token = token.strip_suffix('\r').unwrap_or(token);
        return Ok(Parsed {
            username: None,
            password: token.to_owned(),
        });
    }
    let mut username = None;
    let mut password = None;
    for line in lines() {
        if let Some(v) = line.strip_prefix("username=") {
            username = Some(v.to_owned());
        } else if let Some(v) = line.strip_prefix("password=") {
            password = Some(v.to_owned());
        }
    }
    Ok(Parsed {
        username: username.filter(|u| !u.is_empty()),
        password: password.ok_or(CredentialError::MissingPassword)?,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const URL: &str = "https://github.com/o/r.git";

    fn file_with(content: &str) -> (tempfile::TempDir, CredentialFile) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cred");
        fs::write(&path, content).unwrap();
        let cf = CredentialFile::new(&path, URL);
        (dir, cf)
    }

    #[test]
    fn token_file() {
        let (_d, cf) = file_with("ghp_abc\n");
        let c = cf.credential_for(URL, None).unwrap();
        assert_eq!(c.username, DEFAULT_USERNAME);
        assert_eq!(c.password, "ghp_abc");
        let c = cf.credential_for(URL, Some("bot")).unwrap();
        assert_eq!(c.username, "bot");
        assert!(!format!("{c:?}").contains("ghp_abc"));

        // Only one trailing newline is dropped; a token ending in `=` with
        // no key line is still a token.
        let (_d, cf) = file_with("dG9rZW4=\r\n");
        assert_eq!(cf.credential_for(URL, None).unwrap().password, "dG9rZW4=");
    }

    #[test]
    fn key_value_file() {
        let (_d, cf) = file_with("protocol=https\nhost=github.com\nusername=me\npassword=s3cret\n");
        assert_eq!(
            cf.credential_for(URL, Some("other")).unwrap(),
            Credential {
                username: "me".into(),
                password: "s3cret".into()
            }
        );
        let (_d, cf) = file_with("password=p\n");
        let c = cf.credential_for(URL, Some("url-user")).unwrap();
        assert_eq!(
            (c.username.as_str(), c.password.as_str()),
            ("url-user", "p")
        );
    }

    #[test]
    fn malformed_files() {
        let (_d, cf) = file_with("");
        assert!(matches!(
            cf.credential_for(URL, None),
            Err(CredentialError::Empty)
        ));
        let (_d, cf) = file_with(" \n\n");
        assert!(matches!(
            cf.credential_for(URL, None),
            Err(CredentialError::Empty)
        ));
        let (_d, cf) = file_with("username=me\n");
        assert!(matches!(
            cf.credential_for(URL, None),
            Err(CredentialError::MissingPassword)
        ));
        let (_d, cf) = file_with(&"x".repeat(MAX_CREDENTIAL_FILE as usize + 1));
        assert!(matches!(
            cf.credential_for(URL, None),
            Err(CredentialError::Io { .. })
        ));
        let dir = tempfile::tempdir().unwrap();
        let cf = CredentialFile::new(dir.path().join("missing"), URL);
        assert!(matches!(
            cf.credential_for(URL, None),
            Err(CredentialError::Io { .. })
        ));
    }

    #[test]
    fn reloads_when_the_file_changes() {
        let (dir, cf) = file_with("one\n");
        let path = cf.path().to_owned();
        assert_eq!(cf.credential_for(URL, None).unwrap().password, "one");
        assert_eq!(cf.credential_for(URL, None).unwrap().password, "one");
        assert_eq!(cf.reloads(), 1, "an unchanged file is served from cache");

        // Rewritten in place with a different length.
        fs::write(&path, "second\n").unwrap();
        assert_eq!(cf.credential_for(URL, None).unwrap().password, "second");
        assert_eq!(cf.reloads(), 2);

        // Renamed over with the same length: a new inode.
        let tmp = dir.path().join("tmp");
        fs::write(&tmp, "SECOND\n").unwrap();
        fs::rename(&tmp, &path).unwrap();
        assert_eq!(cf.credential_for(URL, None).unwrap().password, "SECOND");
        assert_eq!(cf.reloads(), 3);

        // A symlink swap, as kubelet does for Secret volumes.
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, "via-a\n").unwrap();
        fs::write(&b, "via-b\n").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let cf = CredentialFile::new(&link, URL);
        assert_eq!(cf.credential_for(URL, None).unwrap().password, "via-a");
        let tmp_link = dir.path().join("link.tmp");
        std::os::unix::fs::symlink(&b, &tmp_link).unwrap();
        fs::rename(&tmp_link, &link).unwrap();
        assert_eq!(cf.credential_for(URL, None).unwrap().password, "via-b");
    }

    #[test]
    fn bound_to_host() {
        let (_d, cf) = file_with("t\n");
        cf.credential_for("https://github.com/o/r", None).unwrap();
        cf.credential_for("https://user@GitHub.com/other/repo.git", None)
            .unwrap();
        let err = cf
            .credential_for("https://evil.example/o/r", None)
            .unwrap_err();
        assert!(
            matches!(&err, CredentialError::HostMismatch { expected: Some(e), got: Some(g) }
                if e == "github.com" && g == "evil.example"),
            "{err:?}"
        );
        // A different port is a different authority.
        assert!(
            cf.credential_for("https://github.com:8443/o/r", None)
                .is_err()
        );
    }
}
