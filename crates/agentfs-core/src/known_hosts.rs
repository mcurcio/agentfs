//! Host key verification for SSH remotes (design §11.1).
//!
//! `remote.<name>.agentfsKnownHosts` names an OpenSSH known_hosts file. It
//! is parsed here, in pure Rust, following sshd(8) "SSH_KNOWN_HOSTS FILE
//! FORMAT":
//!
//! - blank lines and `#` comments are skipped;
//! - a line may start with a `@revoked` or `@cert-authority` marker;
//! - the host field is a comma-separated list of patterns with `*` and `?`
//!   wildcards and `!` negation, where a non-default port is written
//!   `[host]:port`; or a hashed name, `|1|<base64 salt>|<base64
//!   HMAC-SHA1(salt, name)>`;
//! - then the key type, the base64 key blob and an optional comment.
//!
//! A line that cannot be parsed, or whose key type disagrees with the type
//! inside its blob, is skipped and its number kept for error messages.
//!
//! [`KnownHosts::check`] decides whether a presented host key is trusted.
//! Nothing is ever learned or accepted on first use: an unknown host, a
//! changed key, a key type the file does not list and a revoked key are
//! all refused. `@cert-authority` lines (host certificates) are not
//! supported; a host only they match is unknown, with a hint.
//!
//! The file is re-read when its stat stamp changes ([`crate::watched`]).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::watched::{WatchedError, WatchedFile};

/// The largest known_hosts file read.
pub const MAX_KNOWN_HOSTS_FILE: u64 = 1 << 20;

/// Why a host key is not trusted.
#[derive(Debug, thiserror::Error)]
pub enum HostKeyError {
    #[error("reading known_hosts {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("host {name} is unknown: known_hosts has no entry for it{}", hint.map(|h| format!(" ({h})")).unwrap_or_default())]
    Unknown {
        name: String,
        key_type: String,
        hint: Option<&'static str>,
    },
    #[error(
        "host {name} presented a {key_type} key, but known_hosts lists only {} for it; add \
         its {key_type} key (ssh-keyscan lists every type the server has)",
        known_types.join(", ")
    )]
    UnknownKeyType {
        name: String,
        key_type: String,
        known_types: Vec<String>,
    },
    #[error(
        "the {key_type} host key of {name} does not match known_hosts: the key has changed, \
         or something is intercepting the connection"
    )]
    Mismatch { name: String, key_type: String },
    #[error("the presented {key_type} host key is marked @revoked in known_hosts")]
    Revoked { key_type: String },
    #[error("malformed host key: {reason}")]
    Malformed { reason: &'static str },
}

/// The hint for a host only `@cert-authority` lines match.
const CERT_AUTHORITY_HINT: &str =
    "only @cert-authority lines match it, and host certificates are not supported";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    None,
    Revoked,
    CertAuthority,
}

#[derive(Debug, Clone)]
struct Pattern {
    negated: bool,
    /// Lowercased.
    glob: String,
}

#[derive(Debug, Clone)]
enum Hosts {
    Patterns(Vec<Pattern>),
    Hashed { salt: Vec<u8>, hash: [u8; 20] },
}

impl Hosts {
    /// sshd's rule: a positive pattern matches and no negated one does.
    fn matches(&self, name: &str) -> bool {
        match self {
            Self::Patterns(patterns) => {
                let mut positive = false;
                for p in patterns {
                    if glob(p.glob.as_bytes(), name.as_bytes()) {
                        if p.negated {
                            return false;
                        }
                        positive = true;
                    }
                }
                positive
            }
            Self::Hashed { salt, hash } => hmac_sha1(salt, name.as_bytes()) == *hash,
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    marker: Marker,
    hosts: Hosts,
    key_type: String,
    blob: Vec<u8>,
}

/// A parsed known_hosts file.
#[derive(Debug, Clone, Default)]
pub struct KnownHosts {
    entries: Vec<Entry>,
    invalid: Vec<usize>,
}

impl KnownHosts {
    /// Parses a known_hosts file. Never fails: invalid lines are skipped
    /// and listed by [`KnownHosts::invalid_lines`].
    pub fn parse(text: &str) -> Self {
        let mut hosts = Self::default();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match parse_line(line) {
                Some(entry) => hosts.entries.push(entry),
                None => hosts.invalid.push(i + 1),
            }
        }
        hosts
    }

    /// The 1-based numbers of the lines that were skipped as invalid.
    pub fn invalid_lines(&self) -> &[usize] {
        &self.invalid
    }

    /// Whether `presented` (an SSH wire-format public key blob) is the
    /// trusted key of `host` on `port` (`None` for the default, 22).
    ///
    /// In order: a key listed `@revoked` on any line is refused; a matching
    /// line with the same key trusts it; a matching line with the same type
    /// and another key means the key changed; matching lines of other types
    /// only mean the type is unknown; otherwise the host is unknown.
    pub fn check(
        &self,
        host: &str,
        port: Option<u16>,
        presented: &[u8],
    ) -> Result<(), HostKeyError> {
        let key_type = key_type(presented)
            .ok_or(HostKeyError::Malformed {
                reason: "the presented host key is not an SSH public key blob",
            })?
            .to_owned();
        if self
            .entries
            .iter()
            .any(|e| e.marker == Marker::Revoked && e.blob == presented)
        {
            return Err(HostKeyError::Revoked { key_type });
        }
        let name = lookup_name(host, port);
        let mut mismatch = false;
        let mut cert_authority = false;
        let mut known_types: Vec<String> = Vec::new();
        for entry in self.entries.iter().filter(|e| e.hosts.matches(&name)) {
            match entry.marker {
                Marker::Revoked => {}
                Marker::CertAuthority => cert_authority = true,
                Marker::None if entry.blob == presented => return Ok(()),
                Marker::None if entry.key_type == key_type => mismatch = true,
                Marker::None => {
                    if !known_types.contains(&entry.key_type) {
                        known_types.push(entry.key_type.clone());
                    }
                }
            }
        }
        if mismatch {
            Err(HostKeyError::Mismatch { name, key_type })
        } else if !known_types.is_empty() {
            Err(HostKeyError::UnknownKeyType {
                name,
                key_type,
                known_types,
            })
        } else {
            Err(HostKeyError::Unknown {
                name,
                key_type,
                hint: cert_authority.then_some(CERT_AUTHORITY_HINT),
            })
        }
    }
}

impl KnownHosts {
    /// The key types listed for `host` on `port` (plain lines only), in
    /// file order without repeats: the host key algorithms an SSH client
    /// should ask for first, as OpenSSH does.
    pub fn key_types(&self, host: &str, port: Option<u16>) -> Vec<String> {
        let name = lookup_name(host, port);
        let mut types: Vec<String> = Vec::new();
        for entry in &self.entries {
            if entry.marker == Marker::None
                && entry.hosts.matches(&name)
                && !types.contains(&entry.key_type)
            {
                types.push(entry.key_type.clone());
            }
        }
        types
    }
}

/// The name a host is looked up by: the lowercased host on the default
/// port, `[host]:port` on any other.
pub fn lookup_name(host: &str, port: Option<u16>) -> String {
    let host = host.to_ascii_lowercase();
    match port {
        None | Some(22) => host,
        Some(port) => format!("[{host}]:{port}"),
    }
}

/// One non-comment line; `None` if it is invalid.
fn parse_line(line: &str) -> Option<Entry> {
    let mut fields = line.split_ascii_whitespace();
    let mut first = fields.next()?;
    let marker = match first {
        "@revoked" => Marker::Revoked,
        "@cert-authority" => Marker::CertAuthority,
        m if m.starts_with('@') => return None,
        _ => Marker::None,
    };
    if marker != Marker::None {
        first = fields.next()?;
    }
    let hosts = parse_hosts(first)?;
    let key_type = fields.next()?.to_owned();
    let blob = STANDARD.decode(fields.next()?).ok()?;
    if self::key_type(&blob)? != key_type {
        return None;
    }
    Some(Entry {
        marker,
        hosts,
        key_type,
        blob,
    })
}

fn parse_hosts(field: &str) -> Option<Hosts> {
    if let Some(rest) = field.strip_prefix("|1|") {
        let (salt, hash) = rest.split_once('|')?;
        let salt = STANDARD.decode(salt).ok()?;
        let hash: [u8; 20] = STANDARD.decode(hash).ok()?.try_into().ok()?;
        return Some(Hosts::Hashed { salt, hash });
    }
    if field.starts_with('|') {
        return None;
    }
    let patterns = field
        .split(',')
        .map(|p| {
            let (negated, glob) = match p.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, p),
            };
            (!glob.is_empty()).then(|| Pattern {
                negated,
                glob: glob.to_ascii_lowercase(),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Hosts::Patterns(patterns))
}

/// The key type string at the start of an SSH wire-format public key blob
/// (`ssh-ed25519`, `ecdsa-sha2-nistp256`, `ssh-rsa`, ...).
pub fn key_type(blob: &[u8]) -> Option<&str> {
    let len = u32::from_be_bytes(blob.get(..4)?.try_into().ok()?) as usize;
    let name = blob.get(4..4usize.checked_add(len)?)?;
    let name = std::str::from_utf8(name).ok()?;
    (!name.is_empty() && name.bytes().all(|b| b.is_ascii_graphic())).then_some(name)
}

/// `*` and `?` wildcards over bytes, as in ssh's `match_pattern`.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(&c) if c == b'?' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    p = sp + 1;
                    t = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

/// HMAC-SHA1 (RFC 2104), as OpenSSH uses for hashed host names.
fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..20].copy_from_slice(&sha1_smol::Sha1::from(key).digest().bytes());
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = sha1_smol::Sha1::new();
    inner.update(&block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = sha1_smol::Sha1::new();
    outer.update(&block.map(|b| b ^ 0x5c));
    outer.update(&inner.digest().bytes());
    outer.digest().bytes()
}

/// One remote's known_hosts file, re-read when it changes.
pub struct KnownHostsFile {
    file: WatchedFile<Arc<KnownHosts>>,
}

impl KnownHostsFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            file: WatchedFile::new(path, MAX_KNOWN_HOSTS_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// How many times the file has been read and parsed.
    pub fn reloads(&self) -> usize {
        self.file.reloads()
    }

    /// The file's current content.
    pub fn load(&self) -> Result<Arc<KnownHosts>, HostKeyError> {
        self.file
            .load(|bytes| {
                std::str::from_utf8(bytes)
                    .map(|text| Arc::new(KnownHosts::parse(text)))
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            })
            .map_err(|e| HostKeyError::Io {
                path: self.path().to_path_buf(),
                source: match e {
                    WatchedError::Io(e) | WatchedError::Parse(e) => e,
                },
            })
    }

    /// [`KnownHosts::check`] against the file's current content.
    pub fn check(
        &self,
        host: &str,
        port: Option<u16>,
        presented: &[u8],
    ) -> Result<(), HostKeyError> {
        self.load()?.check(host, port, presented)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;

    use super::*;

    fn ssh_string(out: &mut Vec<u8>, data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
    }

    /// An Ed25519 public key blob whose key bytes are all `seed`.
    pub(crate) fn ed25519(seed: u8) -> Vec<u8> {
        let mut blob = Vec::new();
        ssh_string(&mut blob, b"ssh-ed25519");
        ssh_string(&mut blob, &[seed; 32]);
        blob
    }

    /// An ECDSA P-256 public key blob whose point bytes are all `seed`.
    pub(crate) fn ecdsa(seed: u8) -> Vec<u8> {
        let mut blob = Vec::new();
        ssh_string(&mut blob, b"ecdsa-sha2-nistp256");
        ssh_string(&mut blob, b"nistp256");
        let mut point = vec![4u8];
        point.extend([seed; 64]);
        ssh_string(&mut blob, &point);
        blob
    }

    pub(crate) fn line(hosts: &str, blob: &[u8]) -> String {
        format!(
            "{hosts} {} {} comment",
            key_type(blob).unwrap(),
            STANDARD.encode(blob)
        )
    }

    fn hashed(name: &str, salt: &[u8]) -> String {
        format!(
            "|1|{}|{}",
            STANDARD.encode(salt),
            STANDARD.encode(hmac_sha1(salt, name.as_bytes()))
        )
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn hmac_sha1_rfc2202() {
        let cases: [(&[u8], &[u8], &str); 7] = [
            (
                &[0x0b; 20],
                b"Hi There",
                "b617318655057264e28bc0b6fb378c8ef146be00",
            ),
            (
                b"Jefe",
                b"what do ya want for nothing?",
                "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79",
            ),
            (
                &[0xaa; 20],
                &[0xdd; 50],
                "125d7342b9ac11cd91a39af48aa17b4f63f175d3",
            ),
            (
                &[
                    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22,
                    23, 24, 25,
                ],
                &[0xcd; 50],
                "4c9007f4026250c6bc8414f9bf50c86c2d7235da",
            ),
            (
                &[0x0c; 20],
                b"Test With Truncation",
                "4c1a03424b55e07fe7f27be1d58bb9324a9a5a04",
            ),
            (
                &[0xaa; 80],
                b"Test Using Larger Than Block-Size Key - Hash Key First",
                "aa4ae5e15272d00e95705637ce8a3b55ed402112",
            ),
            (
                &[0xaa; 80],
                b"Test Using Larger Than Block-Size Key and Larger Than One Block-Size Data",
                "e8e99d0f45237d786d6bbaa7965c7808bbff1a91",
            ),
        ];
        for (key, data, expected) in cases {
            assert_eq!(hex(&hmac_sha1(key, data)), expected);
        }
    }

    #[test]
    fn key_types() {
        assert_eq!(key_type(&ed25519(1)), Some("ssh-ed25519"));
        assert_eq!(key_type(&ecdsa(1)), Some("ecdsa-sha2-nistp256"));
        assert_eq!(key_type(&[]), None);
        assert_eq!(key_type(&[0, 0, 0, 9, b'x']), None);
        assert_eq!(key_type(&[0, 0, 0, 0]), None);
        assert_eq!(key_type(&[0xff, 0xff, 0xff, 0xff]), None);
    }

    #[test]
    fn plain_host_and_ports() {
        let k = ed25519(1);
        let kh = KnownHosts::parse(&line("host.example", &k));
        kh.check("host.example", None, &k).unwrap();
        kh.check("HOST.Example", Some(22), &k).unwrap();
        // A bare entry does not cover another port.
        assert!(matches!(
            kh.check("host.example", Some(2222), &k),
            Err(HostKeyError::Unknown { name, .. }) if name == "[host.example]:2222"
        ));
        let kh = KnownHosts::parse(&line("[Host.example]:2222", &k));
        kh.check("host.example", Some(2222), &k).unwrap();
        assert!(kh.check("host.example", None, &k).is_err());
        assert!(kh.check("host.example", Some(2223), &k).is_err());
        assert!(kh.check("other", Some(2222), &k).is_err());
    }

    #[test]
    fn lists_wildcards_and_negation() {
        let k = ed25519(1);
        let kh = KnownHosts::parse(&line("a.example,*.corp.example,!bad.corp.example,h?st", &k));
        for ok in [
            "a.example",
            "x.corp.example",
            "y.z.corp.example",
            "host",
            "hast",
        ] {
            kh.check(ok, None, &k)
                .unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in ["bad.corp.example", "corp.example", "hoost", "b.example"] {
            assert!(kh.check(bad, None, &k).is_err(), "{bad}");
        }
        // Negation alone never matches.
        let kh = KnownHosts::parse(&line("!x", &k));
        assert!(kh.check("y", None, &k).is_err());
        // Wildcards over the bracketed form, as ssh does.
        let kh = KnownHosts::parse(&line("[*.example]:*", &k));
        kh.check("a.example", Some(2222), &k).unwrap();
        assert!(kh.check("a.example", None, &k).is_err());
    }

    #[test]
    fn globs() {
        assert!(glob(b"*", b""));
        assert!(glob(b"a*b*c", b"aXXbYYc"));
        assert!(glob(b"a*c", b"abcbc"));
        assert!(!glob(b"a*c", b"abcb"));
        assert!(glob(b"??", b"ab"));
        assert!(!glob(b"??", b"abc"));
        assert!(!glob(b"", b"a"));
    }

    #[test]
    fn hashed_entries() {
        let k = ed25519(1);
        let text = format!(
            "{}\n{}\n",
            line(&hashed("host.example", b"0123456789abcdefghij"), &k),
            line(&hashed("[host.example]:2222", b"another salt"), &ecdsa(2)),
        );
        let kh = KnownHosts::parse(&text);
        assert!(kh.invalid_lines().is_empty());
        kh.check("Host.Example", None, &k).unwrap();
        kh.check("host.example", Some(2222), &ecdsa(2)).unwrap();
        assert!(kh.check("other.example", None, &k).is_err());
        assert!(kh.check("host.example", Some(2222), &k).is_err());
    }

    #[test]
    fn verdicts() {
        let (k1, k2, e1) = (ed25519(1), ed25519(2), ecdsa(1));
        let kh = KnownHosts::parse(&format!("{}\n{}\n", line("host", &k1), line("host", &e1)));
        kh.check("host", None, &k1).unwrap();
        kh.check("host", None, &e1).unwrap();
        // Same type, another key: changed.
        let err = kh.check("host", None, &k2).unwrap_err();
        assert!(
            matches!(&err, HostKeyError::Mismatch { key_type, .. } if key_type == "ssh-ed25519")
        );
        assert!(err.to_string().contains("does not match"), "{err}");
        // Only other types listed.
        let kh = KnownHosts::parse(&line("host", &e1));
        let err = kh.check("host", None, &k1).unwrap_err();
        assert!(
            matches!(&err, HostKeyError::UnknownKeyType { known_types, .. }
                if known_types == &["ecdsa-sha2-nistp256"]),
            "{err:?}"
        );
        // Several keys of one type: any listed one is trusted.
        let kh = KnownHosts::parse(&format!("{}\n{}\n", line("host", &k1), line("host", &k2)));
        kh.check("host", None, &k2).unwrap();
        // Unknown host, and an empty file.
        let err = kh.check("elsewhere", None, &k1).unwrap_err();
        assert!(matches!(&err, HostKeyError::Unknown { hint: None, .. }));
        assert!(err.to_string().contains("unknown"), "{err}");
        assert!(matches!(
            KnownHosts::parse("").check("host", None, &k1),
            Err(HostKeyError::Unknown { .. })
        ));
        // A blob that is not a key.
        assert!(matches!(
            kh.check("host", None, b"junk"),
            Err(HostKeyError::Malformed { .. })
        ));
    }

    #[test]
    fn revoked_and_cert_authority() {
        let (k1, k2) = (ed25519(1), ed25519(2));
        // Revoked wins over a valid entry, whatever host it names.
        let kh = KnownHosts::parse(&format!(
            "{}\n@revoked {}\n",
            line("host", &k1),
            line("*", &k1)
        ));
        assert!(matches!(
            kh.check("host", None, &k1),
            Err(HostKeyError::Revoked { .. })
        ));
        let kh = KnownHosts::parse(&format!("@revoked {}\n", line("nowhere", &k1)));
        assert!(matches!(
            kh.check("host", None, &k1),
            Err(HostKeyError::Revoked { .. })
        ));
        // A @cert-authority line never trusts a plain key.
        let kh = KnownHosts::parse(&format!("@cert-authority {}\n", line("*.example", &k2)));
        let err = kh.check("a.example", None, &k2).unwrap_err();
        assert!(
            matches!(err, HostKeyError::Unknown { hint: Some(_), .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    #[test]
    fn key_types_follow_the_file() {
        let (k1, k2, e1) = (ed25519(1), ed25519(2), ecdsa(1));
        let kh = KnownHosts::parse(
            &[
                line("[h]:2222", &e1),
                line("other", &k1),
                line("[h]:2222", &k1),
                line("[h]:2222", &k2),
                format!("@revoked {}", line("[h]:2222", &ecdsa(2))),
            ]
            .join("\n"),
        );
        assert_eq!(
            kh.key_types("h", Some(2222)),
            ["ecdsa-sha2-nistp256", "ssh-ed25519"]
        );
        assert_eq!(kh.key_types("other", None), ["ssh-ed25519"]);
        assert!(kh.key_types("h", None).is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let k = ed25519(1);
        let good = line("host", &k);
        let text = [
            "# a comment".to_owned(),
            String::new(),
            "   ".to_owned(),
            "host".to_owned(),
            "host ssh-ed25519".to_owned(),
            "host ssh-ed25519 !!!notbase64".to_owned(),
            // The type field disagrees with the blob.
            format!("host ssh-rsa {}", STANDARD.encode(&k)),
            format!("@unknown {good}"),
            format!("|1|bad|{}", "x"),
            format!(",host ssh-ed25519 {}", STANDARD.encode(&k)),
            format!("\t{good}"),
        ]
        .join("\n");
        let kh = KnownHosts::parse(&text);
        assert_eq!(kh.invalid_lines(), [4, 5, 6, 7, 8, 9, 10]);
        kh.check("host", None, &k).unwrap();
    }

    #[test]
    fn file_is_reread_when_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let (k1, k2) = (ed25519(1), ed25519(2));
        fs::write(&path, line("[localhost]:2222", &k1)).unwrap();
        let f = KnownHostsFile::new(&path);
        f.check("localhost", Some(2222), &k1).unwrap();
        f.check("localhost", Some(2222), &k1).unwrap();
        assert_eq!(f.reloads(), 1);
        assert!(f.check("localhost", Some(2222), &k2).is_err());
        let tmp = dir.path().join("tmp");
        fs::write(&tmp, line("[localhost]:2222", &k2)).unwrap();
        fs::rename(&tmp, &path).unwrap();
        f.check("localhost", Some(2222), &k2).unwrap();
        assert!(f.check("localhost", Some(2222), &k1).is_err());
        assert_eq!(f.reloads(), 2);
        assert_eq!(f.path(), path);

        fs::remove_file(&path).unwrap();
        let err = f.check("localhost", Some(2222), &k2).unwrap_err();
        assert!(matches!(err, HostKeyError::Io { .. }), "{err:?}");
        fs::write(&path, b"\xff\xfe").unwrap();
        assert!(matches!(
            f.check("localhost", Some(2222), &k2),
            Err(HostKeyError::Io { .. })
        ));
    }
}
