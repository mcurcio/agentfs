//! Credential files for pushing (design §11.1).
//!
//! `remote.<name>.agentfsCredentials` names an absolute path holding either
//!
//! - git-credential `key=value` lines (`username=…`, `password=…`; other
//!   keys are ignored), or
//! - a bare token, used as the password (one trailing newline is dropped).
//!
//! `remote.<name>.agentfsSshKey` names an SSH remote's private key: an
//! unencrypted OpenSSH (`BEGIN OPENSSH PRIVATE KEY`) or PEM (`RSA`, `EC`, or
//! PKCS#8 `PRIVATE KEY`) file, decoded by the SSH transport. Encrypted keys
//! and public keys are refused with a clear error.
//!
//! Both files are re-read whenever their stat stamp changes
//! ([`crate::watched`]), so whatever rotates them only has to rewrite or
//! replace them.
//!
//! A credential or key is bound to the host (and port) of the remote URL it
//! was configured for and is never handed to any other host, whatever the
//! transport is redirected to.

use std::io;
use std::path::{Path, PathBuf};

use base64::Engine as _;

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
    #[error(
        "the SSH key file holds no private key (a BEGIN OPENSSH, RSA, EC or PKCS#8 PRIVATE \
         KEY block; not the .pub file)"
    )]
    NotAPrivateKey,
    #[error(
        "the SSH key is encrypted with a passphrase, which is not supported; write it \
         unencrypted (ssh-keygen -p -N '' -f <file>)"
    )]
    EncryptedKey,
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

/// The username for an SSH remote whose URL names none, as GitHub, GitLab
/// and Gitea expect.
pub const DEFAULT_SSH_USERNAME: &str = "git";

/// An SSH private key, as the file holds it.
#[derive(Clone, PartialEq, Eq)]
pub struct SshKey {
    pub text: String,
}

impl std::fmt::Debug for SshKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshKey")
            .field("text", &"<redacted>")
            .finish()
    }
}

/// One SSH remote's private key file (`remote.<name>.agentfsSshKey`).
pub struct SshKeyFile {
    file: WatchedFile<SshKey>,
    bound: Option<String>,
}

impl SshKeyFile {
    /// The key at `path`, bound to the host and port of `bound_url`.
    pub fn new(path: impl Into<PathBuf>, bound_url: &str) -> Self {
        Self {
            file: WatchedFile::new(path, MAX_CREDENTIAL_FILE),
            bound: authority(bound_url),
        }
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// How many times the file has been read and parsed.
    pub fn reloads(&self) -> usize {
        self.file.reloads()
    }

    /// The key for a connection to `url`. Refuses any host other than the
    /// one the file is bound to.
    pub fn key_for(&self, url: &str) -> Result<SshKey, CredentialError> {
        check_bound(self.bound.as_deref(), url)?;
        let path = self.path();
        self.file
            .load(|bytes| parse_ssh_key(utf8(path, bytes)?))
            .map_err(|e| flatten(path, e))
    }
}

/// The magic at the start of an OpenSSH private key's body.
const OPENSSH_MAGIC: &[u8] = b"openssh-key-v1\0";

/// Accepts an unencrypted private key in OpenSSH or PEM format.
fn parse_ssh_key(text: String) -> Result<SshKey, CredentialError> {
    if text.trim().is_empty() {
        return Err(CredentialError::Empty);
    }
    if text.contains('\0') {
        return Err(CredentialError::NotAPrivateKey);
    }
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let (begin, label) = lines
        .iter()
        .enumerate()
        .find_map(|(i, l)| {
            let label = l.strip_prefix("-----BEGIN ")?.strip_suffix("-----")?;
            Some((i, label))
        })
        .ok_or(CredentialError::NotAPrivateKey)?;
    let end_line = format!("-----END {label}-----");
    let end = lines[begin..]
        .iter()
        .position(|l| *l == end_line)
        .map(|n| begin + n)
        .ok_or(CredentialError::NotAPrivateKey)?;
    let body = &lines[begin + 1..end];
    match label {
        "ENCRYPTED PRIVATE KEY" => Err(CredentialError::EncryptedKey),
        "OPENSSH PRIVATE KEY" => {
            let blob = base64::engine::general_purpose::STANDARD
                .decode(body.concat())
                .map_err(|_| CredentialError::NotAPrivateKey)?;
            let rest = blob
                .strip_prefix(OPENSSH_MAGIC)
                .ok_or(CredentialError::NotAPrivateKey)?;
            let len = rest
                .get(..4)
                .and_then(|b| b.try_into().ok())
                .map(u32::from_be_bytes)
                .ok_or(CredentialError::NotAPrivateKey)? as usize;
            let cipher = rest
                .get(4..4 + len)
                .ok_or(CredentialError::NotAPrivateKey)?;
            if cipher == b"none" {
                Ok(SshKey { text })
            } else {
                Err(CredentialError::EncryptedKey)
            }
        }
        "RSA PRIVATE KEY" | "EC PRIVATE KEY" | "DSA PRIVATE KEY" | "PRIVATE KEY" => {
            if body
                .iter()
                .any(|l| l.starts_with("Proc-Type:") && l.contains("ENCRYPTED"))
            {
                Err(CredentialError::EncryptedKey)
            } else {
                Ok(SshKey { text })
            }
        }
        _ => Err(CredentialError::NotAPrivateKey),
    }
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

    mod ssh {
        use base64::engine::general_purpose::STANDARD;

        use super::*;

        const SSH_URL: &str = "ssh://git@git.example:2222/o/r.git";

        fn ssh_string(out: &mut Vec<u8>, data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(data);
        }

        /// An OpenSSH private key file with `cipher`; the body after the
        /// cipher name is filler, which agentfs never parses.
        fn openssh(cipher: &str) -> String {
            let mut blob = OPENSSH_MAGIC.to_vec();
            ssh_string(&mut blob, cipher.as_bytes());
            ssh_string(
                &mut blob,
                if cipher == "none" { b"none" } else { b"bcrypt" },
            );
            ssh_string(&mut blob, b"");
            blob.extend_from_slice(&1u32.to_be_bytes());
            blob.extend_from_slice(&[7u8; 100]);
            let b64 = STANDARD.encode(&blob);
            let wrapped: Vec<&str> = b64
                .as_bytes()
                .chunks(70)
                .map(|c| std::str::from_utf8(c).unwrap())
                .collect();
            format!(
                "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
                wrapped.join("\n")
            )
        }

        fn pem(label: &str, headers: &str) -> String {
            format!(
                "-----BEGIN {label}-----\n{headers}MIIBOgIBAAJBAKj34GkxFhD9\n-----END {label}-----\n"
            )
        }

        fn key_file(content: &[u8]) -> (tempfile::TempDir, SshKeyFile) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("id");
            fs::write(&path, content).unwrap();
            let f = SshKeyFile::new(&path, SSH_URL);
            (dir, f)
        }

        #[test]
        fn accepts_unencrypted_keys() {
            for text in [
                openssh("none"),
                pem("RSA PRIVATE KEY", ""),
                pem("EC PRIVATE KEY", ""),
                pem("PRIVATE KEY", ""),
                format!("\n{}", openssh("none")),
            ] {
                let (_d, f) = key_file(text.as_bytes());
                let key = f.key_for(SSH_URL).unwrap_or_else(|e| panic!("{e}: {text}"));
                assert_eq!(key.text, text);
                assert!(!format!("{key:?}").contains("BEGIN"), "{key:?}");
            }
        }

        #[test]
        fn refuses_encrypted_and_public_keys() {
            for text in [
                openssh("aes256-ctr"),
                pem(
                    "RSA PRIVATE KEY",
                    "Proc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,818C7722D3B01F21\n\n",
                ),
                pem("ENCRYPTED PRIVATE KEY", ""),
            ] {
                let (_d, f) = key_file(text.as_bytes());
                let err = f.key_for(SSH_URL).unwrap_err();
                assert!(matches!(err, CredentialError::EncryptedKey), "{err:?}");
                assert!(err.to_string().contains("passphrase"), "{err}");
            }
            for text in [
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA== me@host\n".to_owned(),
                pem("PUBLIC KEY", ""),
                pem("CERTIFICATE", ""),
                // No END line.
                "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n".to_owned(),
                // Not base64, and not an OpenSSH key body.
                "-----BEGIN OPENSSH PRIVATE KEY-----\n!!!\n-----END OPENSSH PRIVATE KEY-----\n"
                    .to_owned(),
                format!(
                    "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
                    STANDARD.encode(b"not-openssh-key-v1")
                ),
                "a token, not a key\n".to_owned(),
            ] {
                let (_d, f) = key_file(text.as_bytes());
                let err = f.key_for(SSH_URL).unwrap_err();
                assert!(
                    matches!(err, CredentialError::NotAPrivateKey),
                    "{err:?}: {text}"
                );
            }
        }

        #[test]
        fn refuses_unreadable_files() {
            let (_d, f) = key_file(b"");
            assert!(matches!(f.key_for(SSH_URL), Err(CredentialError::Empty)));
            let (_d, f) = key_file(b" \n");
            assert!(matches!(f.key_for(SSH_URL), Err(CredentialError::Empty)));
            let mut nul = openssh("none").into_bytes();
            nul.insert(40, 0);
            let (_d, f) = key_file(&nul);
            assert!(matches!(
                f.key_for(SSH_URL),
                Err(CredentialError::NotAPrivateKey)
            ));
            let (_d, f) = key_file(b"\xff\xfe-----BEGIN");
            assert!(matches!(
                f.key_for(SSH_URL),
                Err(CredentialError::Io { .. })
            ));
            let big = "x".repeat(MAX_CREDENTIAL_FILE as usize + 1);
            let (_d, f) = key_file(big.as_bytes());
            assert!(matches!(
                f.key_for(SSH_URL),
                Err(CredentialError::Io { .. })
            ));
            let dir = tempfile::tempdir().unwrap();
            let f = SshKeyFile::new(dir.path().join("missing"), SSH_URL);
            assert!(matches!(
                f.key_for(SSH_URL),
                Err(CredentialError::Io { .. })
            ));
        }

        #[test]
        fn bound_to_host_and_port() {
            let (_d, f) = key_file(openssh("none").as_bytes());
            f.key_for(SSH_URL).unwrap();
            f.key_for("ssh://other-user@GIT.example:2222/x").unwrap();
            for url in [
                "ssh://git@evil.example:2222/o/r.git",
                "ssh://git@git.example/o/r.git",
                "ssh://git@git.example:22/o/r.git",
                "git@git.example:o/r.git",
            ] {
                assert!(
                    matches!(f.key_for(url), Err(CredentialError::HostMismatch { .. })),
                    "{url}"
                );
            }
            // scp-like URLs bind to their host.
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("id");
            fs::write(&path, openssh("none")).unwrap();
            let f = SshKeyFile::new(&path, "git@github.com:o/r.git");
            f.key_for("ssh://git@github.com/o/r.git").unwrap();
            assert!(f.key_for("ssh://git@gitlab.com/o/r.git").is_err());
        }

        #[test]
        fn reloads_when_the_key_is_replaced() {
            let (one, two) = (pem("EC PRIVATE KEY", ""), openssh("none"));
            let (dir, f) = key_file(one.as_bytes());
            let path = f.path().to_owned();
            assert_eq!(f.key_for(SSH_URL).unwrap().text, one);
            assert_eq!(f.key_for(SSH_URL).unwrap().text, one);
            assert_eq!(f.reloads(), 1);
            // Renamed over: a new inode.
            let tmp = dir.path().join("tmp");
            fs::write(&tmp, &two).unwrap();
            fs::rename(&tmp, &path).unwrap();
            assert_eq!(f.key_for(SSH_URL).unwrap().text, two);
            assert_eq!(f.reloads(), 2);
            // A symlink swap, as kubelet does for Secret volumes.
            let (a, b) = (dir.path().join("a"), dir.path().join("b"));
            fs::write(&a, &one).unwrap();
            fs::write(&b, &two).unwrap();
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&a, &link).unwrap();
            let f = SshKeyFile::new(&link, SSH_URL);
            assert_eq!(f.key_for(SSH_URL).unwrap().text, one);
            let tmp_link = dir.path().join("link.tmp");
            std::os::unix::fs::symlink(&b, &tmp_link).unwrap();
            fs::rename(&tmp_link, &link).unwrap();
            assert_eq!(f.key_for(SSH_URL).unwrap().text, two);
            assert_eq!(f.reloads(), 2);
        }
    }
}
