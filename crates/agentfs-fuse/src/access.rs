//! Mode-bit permission checks (design §6).
//!
//! Which of read, write and execute (search, for a directory) an identity
//! gets on an object, decided as the kernel's `generic_permission` decides
//! by mode bits alone: the first class that matches (owner, then group,
//! then other) gives the bits, with no fall-through to a later class; uid 0
//! may read and write anything and search any directory, but executes a
//! file only when some execute bit is set. POSIX ACLs are not consulted:
//! agentfs enforces mode bits only (`crate::fuse`).
//!
//! Pure, so every rule is unit-tested without root. Consumers:
//! [`crate::ops`] (opening an existing file on a create) and
//! [`crate::overlay`] (every access through the sandbox overlay, as the
//! view owner).

use fuser::FileAttr;
use rustix::fs::Stat;

/// What an access asks for: `access(2)`'s mask as three flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Want {
    pub read: bool,
    pub write: bool,
    /// Execute a file, or search a directory.
    pub exec: bool,
}

impl Want {
    /// `access(2)`'s `mask` (`R_OK` 4, `W_OK` 2, `X_OK` 1; `F_OK` 0 asks
    /// for nothing).
    pub fn from_mask(mask: i32) -> Self {
        let _ = mask;
        todo!("sandbox overlay slice 2")
    }

    /// What `open(2)` with `flags` asks of an existing file: read unless
    /// `O_WRONLY`, write unless `O_RDONLY` or when `O_TRUNC` is set.
    pub fn from_open_flags(flags: i32) -> Self {
        let _ = flags;
        todo!("sandbox overlay slice 2")
    }
}

/// The attributes of an object that a check reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Object {
    /// File type and permission bits, as `st_mode`.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Object {
    pub fn from_stat(st: &Stat) -> Self {
        let _ = st;
        todo!("sandbox overlay slice 2")
    }

    pub fn from_attr(attr: &FileAttr) -> Self {
        let _ = attr;
        todo!("sandbox overlay slice 2")
    }
}

/// The identity asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Who<'a> {
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups.
    pub groups: &'a [u32],
}

/// Whether `who` gets everything `want` asks of `object`.
pub fn permits(object: Object, who: Who<'_>, want: Want) -> bool {
    let _ = (object, who, want);
    todo!("sandbox overlay slice 2")
}

/// Whether `who` may remove or replace an entry owned by `entry_uid` in
/// directory `dir`, given it may write and search `dir`: in a sticky
/// directory (`S_ISVTX`), only the entry's owner, the directory's owner or
/// uid 0 may.
pub fn may_unlink_in(dir: Object, entry_uid: u32, who: Who<'_>) -> bool {
    let _ = (dir, entry_uid, who);
    todo!("sandbox overlay slice 2")
}
