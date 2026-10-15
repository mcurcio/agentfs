//! `/proc/self/mountinfo` (proc(5)): what is mounted where, with which
//! options. The one parser of it: agentfs-fuse's mount state and views and
//! [`crate::shared_fs`] read it here (design §2.3, §5, §17.1).

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// One line of mountinfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// `major:minor`; mounts of one filesystem share it.
    pub device: String,
    /// The directory of that filesystem this mount shows (`/` for all of
    /// it).
    pub root: PathBuf,
    pub mountpoint: PathBuf,
    /// `fuse.agentfs`, `nfs4`, `nfs`, `ext4`, ...
    pub fstype: String,
    /// `agentfs:<volume>`, `<server>:<export>`, `/dev/sda1`, ...
    pub source: String,
    /// The filesystem's own options as the kernel shows them
    /// (`rw,vers=4.2,...,local_lock=none,...` on NFS).
    pub super_options: String,
}

impl MountEntry {
    /// An agentfs FUSE mount: an fstype starting with `fuse` (`fuse` with
    /// some mount helpers) and an `agentfs:` source.
    pub fn is_agentfs(&self) -> bool {
        self.fstype.starts_with("fuse") && self.source.starts_with("agentfs:")
    }

    /// The value of `key=value` among the super options, if present.
    pub fn super_option(&self, key: &str) -> Option<&str> {
        self.super_options.split(',').find_map(|option| {
            let (k, v) = option.split_once('=')?;
            (k == key).then_some(v)
        })
    }

    /// Whether the bare flag (no `=`) is among the super options.
    pub fn has_super_flag(&self, flag: &str) -> bool {
        self.super_options.split(',').any(|option| option == flag)
    }
}

/// Parses mountinfo text, in order. Lines that do not parse are skipped.
/// The root, mount point, fstype and source have the kernel's octal
/// escapes (`\040` for a space, ...) decoded.
pub fn parse(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|line| {
            // `id parent major:minor root mountpoint options [optional...]
            // - fstype source superoptions`
            let (before, after) = line.split_once(" - ")?;
            let mut fields = before.split(' ');
            let device = fields.nth(2)?.to_owned();
            let root = fields.next()?;
            let mountpoint = fields.next()?;
            let mut after = after.split(' ');
            let fstype = after.next()?;
            let source = after.next().unwrap_or("");
            let super_options = after.next().unwrap_or("");
            Some(MountEntry {
                device,
                root: path(root),
                mountpoint: path(mountpoint),
                fstype: text_of(fstype),
                source: text_of(source),
                super_options: super_options.to_owned(),
            })
        })
        .collect()
}

fn path(field: &str) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(&unescape(field)))
}

fn text_of(field: &str) -> String {
    String::from_utf8_lossy(&unescape(field)).into_owned()
}

/// `/proc/self/mountinfo`, parsed.
pub fn read() -> io::Result<Vec<MountEntry>> {
    Ok(parse(&std::fs::read_to_string("/proc/self/mountinfo")?))
}

/// The mount holding `path` (absolute, canonical): among the entries whose
/// mount point is `path` or an ancestor of it, the deepest; among equals,
/// the last (topmost).
pub fn containing<'a>(entries: &'a [MountEntry], path: &Path) -> Option<&'a MountEntry> {
    let mut best: Option<(usize, &MountEntry)> = None;
    for entry in entries.iter().filter(|e| path.starts_with(&e.mountpoint)) {
        let depth = entry.mountpoint.components().count();
        if best.is_none_or(|(deepest, _)| depth >= deepest) {
            best = Some((depth, entry));
        }
    }
    best.map(|(_, entry)| entry)
}

/// Decodes the kernel's `\ooo` escapes (space, tab, newline, backslash).
pub fn unescape(field: &str) -> Vec<u8> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..=i + 3]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let value = bytes[i + 1..=i + 3]
                .iter()
                .fold(0u32, |n, b| n * 8 + u32::from(b - b'0'));
            if let Ok(byte) = u8::try_from(value) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
35 22 0:31 / /data rw,relatime shared:2 master:7 - xfs /dev/sdb rw,attr2
40 35 0:44 / /agentfs/workspace rw,nosuid,nodev,relatime shared:9 - fuse.agentfs agentfs:hermes rw,user_id=0,group_id=0,default_permissions,allow_other
41 22 0:45 / /with\\040space rw - fuse agentfs:v\\134x rw
42 22 0:46 / /stacked rw - tmpfs tmpfs rw
43 42 0:47 / /stacked rw - fuse.agentfs agentfs:top rw
44 22 0:48 /sub\\040dir /tab\\011and\\012newline rw - fuse.sshfs host:/x rw
50 35 0:60 / /data/nfs rw - nfs4 server:/export rw,vers=4.2,hard,proto=tcp,local_lock=none,addr=10.0.0.1
garbage line
";

    #[test]
    fn parses_fields_and_unescapes() {
        let entries = parse(MOUNTINFO);
        assert_eq!(entries.len(), 8, "{entries:#?}");
        assert_eq!(
            entries[2],
            MountEntry {
                device: "0:44".into(),
                root: "/".into(),
                mountpoint: "/agentfs/workspace".into(),
                fstype: "fuse.agentfs".into(),
                source: "agentfs:hermes".into(),
                super_options: "rw,user_id=0,group_id=0,default_permissions,allow_other".into(),
            }
        );
        assert!(entries[2].is_agentfs());
        // Escapes, and a `fuse` fstype from a mount helper.
        assert_eq!(entries[3].mountpoint, Path::new("/with space"));
        assert_eq!(entries[3].source, "agentfs:v\\x");
        assert!(entries[3].is_agentfs());
        assert_eq!(entries[6].mountpoint, Path::new("/tab\tand\nnewline"));
        assert_eq!(entries[6].root, Path::new("/sub dir"));
        assert!(!entries[6].is_agentfs());
        // Optional fields before the separator do not shift anything.
        assert_eq!(entries[1].fstype, "xfs");
        assert_eq!(entries[1].device, "0:31");
        assert!(!entries[0].is_agentfs());
        // Escapes that are not one: left as they are.
        assert_eq!(unescape("a\\0"), b"a\\0");
        assert_eq!(unescape("\\777"), b"\\777");
        assert_eq!(unescape("\\101\\"), b"A\\");
    }

    #[test]
    fn containing_picks_the_deepest_topmost_mount() {
        let entries = parse(MOUNTINFO);
        let at = |p: &str| containing(&entries, Path::new(p)).map(|e| e.source.as_str());
        assert_eq!(at("/"), Some("/dev/sda1"));
        assert_eq!(at("/etc/hosts"), Some("/dev/sda1"));
        assert_eq!(at("/data/file"), Some("/dev/sdb"));
        assert_eq!(at("/data/nfs/vol/state"), Some("server:/export"));
        // A name sharing a prefix is not under the mount.
        assert_eq!(at("/data/nfsother"), Some("/dev/sdb"));
        // Two mounts at one point: the later one is on top.
        assert_eq!(at("/stacked/x"), Some("agentfs:top"));
        assert_eq!(at("/with space/y"), Some("agentfs:v\\x"));
        assert_eq!(containing(&[], Path::new("/")), None);
        // This process's own mountinfo has a mount holding /.
        assert!(containing(&read().unwrap(), Path::new("/")).is_some());
    }

    #[test]
    fn super_options_are_read() {
        let entries = parse(MOUNTINFO);
        let nfs = &entries[7];
        assert_eq!(nfs.super_option("vers"), Some("4.2"));
        assert_eq!(nfs.super_option("local_lock"), Some("none"));
        assert_eq!(nfs.super_option("addr"), Some("10.0.0.1"));
        assert_eq!(nfs.super_option("hard"), None, "a flag has no value");
        assert!(nfs.has_super_flag("hard"));
        assert!(
            !nfs.has_super_flag("vers"),
            "an option with a value is no flag"
        );
        assert!(!nfs.has_super_flag("nolock"));
        assert_eq!(nfs.super_option("lock"), None, "keys match whole");
        assert_eq!(entries[0].super_options, "rw");
    }
}
