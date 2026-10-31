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

use fuser::{FileAttr, FileType};
use rustix::fs::{OFlags, Stat};

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
        Self {
            read: mask & 4 != 0,
            write: mask & 2 != 0,
            exec: mask & 1 != 0,
        }
    }

    /// What `open(2)` with `flags` asks of an existing file: read unless
    /// `O_WRONLY`, write unless `O_RDONLY` or when `O_TRUNC` is set.
    pub fn from_open_flags(flags: i32) -> Self {
        let acc = flags & 0o3;
        let truncate = flags & OFlags::TRUNC.bits() as i32 != 0;
        Self {
            read: acc != 1,
            write: acc != 0 || truncate,
            exec: false,
        }
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
        Self {
            mode: u32::from(st.st_mode),
            uid: st.st_uid,
            gid: st.st_gid,
        }
    }

    pub fn from_attr(attr: &FileAttr) -> Self {
        Self {
            mode: type_bits(attr.kind) | u32::from(attr.perm),
            uid: attr.uid,
            gid: attr.gid,
        }
    }

    fn is_dir(self) -> bool {
        self.mode & S_IFMT == S_IFDIR
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
    if who.uid == 0 {
        return !want.exec || object.is_dir() || object.mode & 0o111 != 0;
    }
    let bits = if object.uid == who.uid {
        (object.mode >> 6) & 0o7
    } else if object.gid == who.gid || who.groups.contains(&object.gid) {
        (object.mode >> 3) & 0o7
    } else {
        object.mode & 0o7
    };
    (!want.read || bits & 0o4 != 0)
        && (!want.write || bits & 0o2 != 0)
        && (!want.exec || bits & 0o1 != 0)
}

/// Whether `who` may remove or replace an entry owned by `entry_uid` in
/// directory `dir`, given it may write and search `dir`: in a sticky
/// directory (`S_ISVTX`), only the entry's owner, the directory's owner or
/// uid 0 may.
pub fn may_unlink_in(dir: Object, entry_uid: u32, who: Who<'_>) -> bool {
    dir.mode & S_ISVTX == 0 || who.uid == 0 || who.uid == entry_uid || who.uid == dir.uid
}

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_ISVTX: u32 = 0o1000;

/// `st_mode`'s file-type bits for `kind` ([`crate::backing::file_type`]
/// read backwards).
fn type_bits(kind: FileType) -> u32 {
    match kind {
        FileType::NamedPipe => 0o010000,
        FileType::CharDevice => 0o020000,
        FileType::Directory => S_IFDIR,
        FileType::BlockDevice => 0o060000,
        FileType::RegularFile => 0o100000,
        FileType::Symlink => 0o120000,
        FileType::Socket => 0o140000,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const R: Want = Want {
        read: true,
        write: false,
        exec: false,
    };
    const W: Want = Want {
        read: false,
        write: true,
        exec: false,
    };
    const X: Want = Want {
        read: false,
        write: false,
        exec: true,
    };

    fn file(mode: u32, uid: u32, gid: u32) -> Object {
        Object {
            mode: 0o100000 | mode,
            uid,
            gid,
        }
    }

    fn dir(mode: u32, uid: u32, gid: u32) -> Object {
        Object {
            mode: S_IFDIR | mode,
            uid,
            gid,
        }
    }

    fn who(uid: u32, gid: u32) -> Who<'static> {
        Who {
            uid,
            gid,
            groups: &[],
        }
    }

    #[test]
    fn the_first_matching_class_decides_with_no_fall_through() {
        // Owner r--, other rw-: the owner gets no write.
        let f = file(0o406, 1000, 1000);
        assert!(permits(f, who(1000, 1000), R));
        assert!(!permits(f, who(1000, 1000), W));
        assert!(permits(f, who(2000, 2000), W));
        // Group ---, other r--: a group member gets nothing.
        let g = file(0o604, 1000, 50);
        assert!(!permits(g, who(2000, 50), R));
        assert!(permits(g, who(2000, 60), R));
        // Supplementary groups count.
        let groups = [7, 50];
        let member = Who {
            uid: 2000,
            gid: 60,
            groups: &groups,
        };
        assert!(!permits(g, member, R));
        assert!(permits(file(0o640, 1000, 50), member, R));
        assert!(!permits(file(0o640, 1000, 50), member, W));
        // F_OK asks for nothing.
        assert!(permits(
            file(0o000, 1000, 50),
            who(2000, 60),
            Want::default()
        ));
    }

    #[test]
    fn root_reads_writes_and_searches_anything_but_executes_only_with_an_x_bit() {
        let root = who(0, 0);
        let f = file(0o000, 1000, 1000);
        assert!(permits(f, root, R));
        assert!(permits(f, root, W));
        assert!(!permits(f, root, X));
        assert!(permits(file(0o001, 1000, 1000), root, X));
        assert!(permits(file(0o100, 1000, 1000), root, X));
        assert!(permits(dir(0o000, 1000, 1000), root, X));
    }

    #[test]
    fn a_sticky_directory_lets_only_owners_and_root_unlink() {
        let sticky = dir(0o1777, 1000, 1000);
        assert!(may_unlink_in(sticky, 2000, who(2000, 2000)));
        assert!(may_unlink_in(sticky, 2000, who(1000, 1000)));
        assert!(may_unlink_in(sticky, 2000, who(0, 0)));
        assert!(!may_unlink_in(sticky, 2000, who(3000, 3000)));
        let plain = dir(0o777, 1000, 1000);
        assert!(may_unlink_in(plain, 2000, who(3000, 3000)));
    }

    #[test]
    fn wants_from_masks_and_open_flags() {
        assert_eq!(Want::from_mask(0), Want::default());
        assert_eq!(
            Want::from_mask(7),
            Want {
                read: true,
                write: true,
                exec: true
            }
        );
        assert_eq!(Want::from_mask(2), W);
        let trunc = OFlags::TRUNC.bits() as i32;
        assert_eq!(Want::from_open_flags(0), R);
        assert_eq!(Want::from_open_flags(1), W);
        assert_eq!(
            Want::from_open_flags(2),
            Want {
                read: true,
                write: true,
                exec: false
            }
        );
        // O_RDONLY | O_TRUNC asks for write.
        assert_eq!(
            Want::from_open_flags(trunc),
            Want {
                read: true,
                write: true,
                exec: false
            }
        );
    }

    #[test]
    fn objects_from_stat_and_attr_agree() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "x").unwrap();
        let st = rustix::fs::stat(&path).unwrap();
        let attr = crate::backing::attr(5, &st);
        assert_eq!(Object::from_stat(&st), Object::from_attr(&attr));
        let d = rustix::fs::stat(dir.path()).unwrap();
        assert!(Object::from_stat(&d).is_dir());
        assert!(Object::from_attr(&crate::backing::attr(6, &d)).is_dir());
    }

    /// Migrated from `Ops`'s former `may_open`: the caller's supplementary
    /// groups count, as `Ops::create` passes them.
    #[test]
    fn supplementary_groups_count_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).unwrap();
        let st = rustix::fs::stat(&path).unwrap();
        let object = Object::from_stat(&st);
        let (uid, gid) = (st.st_uid + 1, st.st_gid + 1);
        let open = |groups: &[u32], flags: i32| {
            permits(
                object,
                Who { uid, gid, groups },
                Want::from_open_flags(flags),
            )
        };
        let trunc = OFlags::TRUNC.bits() as i32;
        assert!(!open(&[], 2));
        assert!(!open(&[st.st_gid + 7], 0));
        assert!(open(&[st.st_gid + 7, st.st_gid], 2 | trunc));
        let member = Who {
            uid,
            gid: st.st_gid,
            groups: &[],
        };
        assert!(permits(object, member, Want::from_open_flags(1)));
    }
}
