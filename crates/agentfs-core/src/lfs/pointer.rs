//! The Git LFS pointer format (design §7; git-lfs `docs/spec.md`, v1).
//!
//! A pointer is the blob history holds in place of LFS-stored content:
//!
//! ```text
//! version https://git-lfs.github.com/spec/v1
//! oid sha256:<64 lowercase hex digits>
//! size <decimal bytes>
//! ```
//!
//! each line ending in `\n`, in this order. agentfs writes exactly this
//! canonical form, and reads only it: anything else, including CRLF line
//! ends, extra keys, uppercase hex or a leading zero in the size, is not a
//! pointer to agentfs, so content a workload wrote that merely looks like
//! one is never mistaken for an object to upload.

use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

use sha2::{Digest, Sha256};

/// The pointer format's version line value.
pub const SPEC_VERSION: &str = "https://git-lfs.github.com/spec/v1";

/// The largest blob that can be a pointer, as in git-lfs.
pub const MAX_POINTER_SIZE: usize = 1024;

/// A SHA-256 object id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LfsOid([u8; 32]);

impl LfsOid {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The SHA-256 of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

impl fmt::Display for LfsOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for LfsOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LfsOid({self})")
    }
}

/// The oid is not 64 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a SHA-256 object id (64 lowercase hex digits)")]
pub struct BadOid;

impl FromStr for LfsOid {
    type Err = BadOid;

    fn from_str(s: &str) -> Result<Self, BadOid> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err(BadOid);
        }
        let digit = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(BadOid),
        };
        let mut out = [0u8; 32];
        for (i, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
            out[i] = digit(pair[0])? << 4 | digit(pair[1])?;
        }
        Ok(Self(out))
    }
}

/// An LFS pointer: the object's id and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pointer {
    pub oid: LfsOid,
    pub size: u64,
}

impl Pointer {
    /// Parses the canonical form only (see the module documentation).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_POINTER_SIZE {
            return None;
        }
        let text = std::str::from_utf8(bytes).ok()?;
        let rest = text.strip_prefix("version ")?.strip_prefix(SPEC_VERSION)?;
        let rest = rest.strip_prefix("\noid sha256:")?;
        let (oid, rest) = rest.split_once('\n')?;
        let size = rest.strip_prefix("size ")?.strip_suffix('\n')?;
        let canonical_size = !size.is_empty()
            && size.bytes().all(|b| b.is_ascii_digit())
            && (size == "0" || !size.starts_with('0'));
        if !canonical_size {
            return None;
        }
        Some(Self {
            oid: oid.parse().ok()?,
            size: size.parse().ok()?,
        })
    }

    /// The canonical pointer text.
    pub fn encode(&self) -> String {
        format!(
            "version {SPEC_VERSION}\noid sha256:{}\nsize {}\n",
            self.oid, self.size
        )
    }

    /// The pointer of everything `reader` yields, read in a streaming way.
    pub fn of_reader(mut reader: impl Read) -> io::Result<Self> {
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buf = vec![0u8; 64 << 10];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            hasher.update(&buf[..n]);
            size += n as u64;
        }
        Ok(Self {
            oid: LfsOid(hasher.finalize().into()),
            size,
        })
    }
}

/// A SHA-256 being computed while bytes are written elsewhere.
pub(crate) struct Hashing {
    hasher: Sha256,
    size: u64,
}

impl Hashing {
    pub(crate) fn new() -> Self {
        Self {
            hasher: Sha256::new(),
            size: 0,
        }
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.size += bytes.len() as u64;
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn finish(self) -> Pointer {
        Pointer {
            oid: LfsOid(self.hasher.finalize().into()),
            size: self.size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from git-lfs's `docs/spec.md`.
    const SPEC_EXAMPLE: &str = "version https://git-lfs.github.com/spec/v1\n\
        oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
        size 12345\n";

    #[test]
    fn the_spec_example_round_trips() {
        let p = Pointer::parse(SPEC_EXAMPLE.as_bytes()).unwrap();
        assert_eq!(p.size, 12345);
        assert_eq!(
            p.oid.to_string(),
            "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393"
        );
        assert_eq!(p.encode(), SPEC_EXAMPLE);
    }

    #[test]
    fn oids_are_sha256() {
        // `printf hello | sha256sum`
        assert_eq!(
            LfsOid::of(b"hello").to_string(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let p = Pointer::of_reader(&b"hello"[..]).unwrap();
        assert_eq!(p.oid, LfsOid::of(b"hello"));
        assert_eq!(p.size, 5);
        let mut h = Hashing::new();
        h.update(b"hel");
        h.update(b"lo");
        assert_eq!(h.finish(), p);
    }

    #[test]
    fn only_the_canonical_form_is_a_pointer() {
        let oid = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let v = "version https://git-lfs.github.com/spec/v1\n";
        for bad in [
            // The pre-release version line git-lfs still reads.
            format!("version https://hawser.github.com/spec/v1\noid sha256:{oid}\nsize 1\n"),
            format!("{v}oid sha256:{}\nsize 1\n", oid.to_uppercase()),
            format!("{v}oid sha256:{}\nsize 1\n", &oid[1..]),
            format!("{v}oid sha256:{oid}\n"),
            format!("{v}oid sha256:{oid}\nsize 1"),
            format!("{v}oid sha256:{oid}\nsize 01\n"),
            format!("{v}oid sha256:{oid}\nsize -1\n"),
            format!("{v}oid sha256:{oid}\nsize\n"),
            format!("{v}oid sha256:{oid}\nsize 1\next-0-foo sha256:{oid}\n"),
            format!("{v}oid sha256:{oid}\nsize 1\n\n"),
            format!("{v}oid md5:{oid}\nsize 1\n"),
            format!("version https://git-lfs.github.com/spec/v1\r\noid sha256:{oid}\r\nsize 1\r\n"),
            format!("{v}size 1\noid sha256:{oid}\n"),
            format!("{v}oid sha256:{oid}\nsize 99999999999999999999999\n"),
            String::new(),
        ] {
            assert_eq!(Pointer::parse(bad.as_bytes()), None, "{bad:?}");
        }
        let mut big = format!("{v}oid sha256:{oid}\nsize 1\n").into_bytes();
        assert!(Pointer::parse(&big).is_some());
        big.resize(MAX_POINTER_SIZE + 1, b' ');
        assert_eq!(Pointer::parse(&big), None);
        assert_eq!(
            Pointer::parse(format!("{v}oid sha256:{oid}\nsize 0\n").as_bytes())
                .unwrap()
                .size,
            0
        );
    }

    #[test]
    fn oid_parsing() {
        let oid: LfsOid = "00ff".repeat(16).parse().unwrap();
        assert_eq!(oid.as_bytes()[1], 0xff);
        assert_eq!(oid.to_string(), "00ff".repeat(16));
        assert_eq!("g".repeat(64).parse::<LfsOid>(), Err(BadOid));
        assert_eq!("0".repeat(63).parse::<LfsOid>(), Err(BadOid));
    }
}
