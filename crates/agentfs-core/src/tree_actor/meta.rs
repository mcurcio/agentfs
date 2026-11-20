//! An entry's metadata and times as pure values: its kind, permission bits and
//! owner, which history records, and the times each tree keeps for itself,
//! which it never does (docs/trees.md, "What a commit holds").

use std::fmt;

use agentfs_contracts::clients::FileOwner;
use serde::{Deserialize, Serialize};

use super::Error;

/// What an entry is: a regular file, a symbolic link (a file whose bytes are
/// its target) or a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Symlink,
    Dir,
}

/// `mode & 0o7777`. JSON: four octal digits (`"0644"`); anything else fails
/// to parse ([`Error::Format`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Perm(u32);

impl Perm {
    /// The permission bits of `mode`: the rest (the file type) dropped.
    pub const fn from_mode(mode: u32) -> Self {
        Self(mode & 0o7777)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Perm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04o}", self.0)
    }
}

impl From<Perm> for String {
    fn from(perm: Perm) -> Self {
        perm.to_string()
    }
}

impl TryFrom<String> for Perm {
    type Error = Error;

    fn try_from(text: String) -> std::result::Result<Self, Error> {
        if text.len() != 4 || !text.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            return Err(Error::Format);
        }
        u32::from_str_radix(&text, 8)
            .map(Self)
            .map_err(|_| Error::Format)
    }
}

/// What a create or a chmod/chown gives an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attrs {
    pub perm: Perm,
    pub owner: FileOwner,
}

/// An entry's metadata as history records it; compared whole.
/// JSON: `{"kind":"file","mode":"0644","uid":1000,"gid":1000}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Meta {
    pub kind: Kind,
    pub mode: Perm,
    pub uid: u32,
    pub gid: u32,
}

/// A link's permission bits: always `0o777`, as Linux shows them.
const LINK_PERM: Perm = Perm(0o777);

impl Meta {
    /// A Symlink's mode is always `0o777`.
    pub const fn new(kind: Kind, attrs: Attrs) -> Self {
        Self {
            kind,
            mode: match kind {
                Kind::Symlink => LINK_PERM,
                Kind::File | Kind::Dir => attrs.perm,
            },
            uid: attrs.owner.uid,
            gid: attrs.owner.gid,
        }
    }

    pub const fn attrs(&self) -> Attrs {
        Attrs {
            perm: self.mode,
            owner: FileOwner {
                uid: self.uid,
                gid: self.gid,
            },
        }
    }

    /// Git's own mode for the entry: `0o120000` a link; `0o100755` a file
    /// whose owner may execute it (`S_IXUSR`), else `0o100644`; `0o040000` a
    /// directory (never a `files/` entry).
    pub const fn git_mode(&self) -> u32 {
        match self.kind {
            Kind::Symlink => 0o120000,
            Kind::Dir => 0o040000,
            Kind::File if self.mode.0 & 0o100 != 0 => 0o100755,
            Kind::File => 0o100644,
        }
    }
}

/// Each tree's own times of an entry, in nanoseconds since the epoch; never
/// in history.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Times {
    pub atime_ns: u64,
    pub mtime_ns: u64,
    pub ctime_ns: u64,
}

impl Times {
    /// A new entry's times: all `at_ns`.
    pub(super) const fn at(at_ns: u64) -> Self {
        Self {
            atime_ns: at_ns,
            mtime_ns: at_ns,
            ctime_ns: at_ns,
        }
    }

    /// Its content changed: mtime and ctime move. atime stays, as Linux
    /// leaves it at a write: it moves only when the entry is made or a time
    /// is set (reads are not tracked).
    pub(super) fn modified(&mut self, at_ns: u64) {
        self.mtime_ns = at_ns;
        self.ctime_ns = at_ns;
    }

    /// Its metadata or name changed: ctime moves.
    pub(super) fn changed(&mut self, at_ns: u64) {
        self.ctime_ns = at_ns;
    }
}

/// A time an explicit set gives: now (the step's own time) or a given time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetTime {
    Now,
    At(u64),
}

impl SetTime {
    /// The time it names, `Now` being `at_ns`.
    pub(super) const fn resolve(self, at_ns: u64) -> u64 {
        match self {
            Self::Now => at_ns,
            Self::At(ns) => ns,
        }
    }
}

/// A plain file's metadata, for unit tests.
#[cfg(test)]
pub(super) const TEST_FILE: Meta = Meta {
    kind: Kind::File,
    mode: Perm(0o644),
    uid: 1000,
    gid: 1000,
};

#[cfg(test)]
mod tests {
    use super::*;

    const ATTRS: Attrs = Attrs {
        perm: Perm::from_mode(0o100755),
        owner: FileOwner {
            uid: 1000,
            gid: 1001,
        },
    };

    #[test]
    fn meta_is_its_stored_json_and_git_modes_follow_the_owners_execute_bit() {
        let meta = Meta::new(Kind::File, ATTRS);
        let text = serde_json::to_string(&meta).unwrap();
        assert_eq!(
            text,
            r#"{"kind":"file","mode":"0755","uid":1000,"gid":1001}"#
        );
        assert_eq!(serde_json::from_str::<Meta>(&text).unwrap(), meta);
        assert_eq!(meta.git_mode(), 0o100755);
        let plain = Meta::new(
            Kind::File,
            Attrs {
                perm: Perm::from_mode(0o655),
                ..ATTRS
            },
        );
        assert_eq!(plain.git_mode(), 0o100644);
        let link = Meta::new(Kind::Symlink, ATTRS);
        assert_eq!((link.mode.bits(), link.git_mode()), (0o777, 0o120000));
        assert_eq!(Meta::new(Kind::Dir, ATTRS).git_mode(), 0o040000);
        assert_eq!(
            meta.attrs(),
            Attrs {
                perm: Perm(0o755),
                ..ATTRS
            }
        );
    }

    #[test]
    fn a_save_keeps_the_atime() {
        let mut times = Times::at(5);
        times.atime_ns = 1;
        times.modified(9);
        assert_eq!((times.atime_ns, times.mtime_ns, times.ctime_ns), (1, 9, 9));
    }

    #[test]
    fn only_four_octal_digits_parse_as_a_mode() {
        for bad in ["644", "06440", "0648", "-644", "0x64", ""] {
            assert!(Perm::try_from(bad.to_owned()).is_err(), "{bad}");
        }
        assert_eq!(Perm::try_from("4755".to_owned()).unwrap().bits(), 0o4755);
    }
}
