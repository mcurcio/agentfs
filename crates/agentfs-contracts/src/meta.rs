//! An entry's metadata as history records it: its kind, permission bits and
//! owner (docs/trees.md, "What a commit holds").

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::clients::FileOwner;

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
/// to parse ([`NotAPerm`]).
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

/// Text that is not a permission: four octal digits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a permission: four octal digits")]
pub struct NotAPerm(pub String);

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
    type Error = NotAPerm;

    fn try_from(text: String) -> Result<Self, NotAPerm> {
        if text.len() != 4 || !text.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            return Err(NotAPerm(text));
        }
        match u32::from_str_radix(&text, 8) {
            Ok(bits) => Ok(Self(bits)),
            Err(_) => Err(NotAPerm(text)),
        }
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
}

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
    fn meta_is_its_stored_json_and_a_link_is_always_0777() {
        let meta = Meta::new(Kind::File, ATTRS);
        let text = serde_json::to_string(&meta).unwrap();
        assert_eq!(
            text,
            r#"{"kind":"file","mode":"0755","uid":1000,"gid":1001}"#
        );
        assert_eq!(serde_json::from_str::<Meta>(&text).unwrap(), meta);
        assert_eq!(Meta::new(Kind::Symlink, ATTRS).mode.bits(), 0o777);
        assert_eq!(
            meta.attrs(),
            Attrs {
                perm: Perm(0o755),
                ..ATTRS
            }
        );
    }

    #[test]
    fn only_four_octal_digits_parse_as_a_mode() {
        for bad in ["644", "06440", "0648", "-644", "0x64", ""] {
            assert_eq!(
                Perm::try_from(bad.to_owned()),
                Err(NotAPerm(bad.to_owned())),
                "{bad}"
            );
        }
        assert_eq!(Perm::try_from("4755".to_owned()).unwrap().bits(), 0o4755);
    }
}
