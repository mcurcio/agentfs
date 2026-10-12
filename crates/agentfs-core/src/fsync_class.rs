//! The `fsync` class for databases: which paths belong to it and how they
//! group (design §8). Pure policy, no I/O.
//!
//! A path is in the class when the operator's `info/attributes` gives it
//! `agentfs.commit=fsync` (`agentfs.*` never comes from tree files, design
//! §4.1). Class paths are recorded in **groups**: the paths in one
//! directory with the same `agentfs.group` value and the same *stem*, the
//! file name without a trailing `-wal`, `-journal` or `-shm`. So
//! `state.db`, `state.db-wal` and `state.db-journal` with
//! `agentfs.group=sqlite` are one group, captured together as one record.
//! Only candidates that themselves carry the class with the same group are
//! members, and a class path with no `agentfs.group` is a group of one.
//! `-shm` files are never members and never recorded (SQLite rebuilds
//! them).
//!
//! The FUSE layer (`agentfs_fuse::groups`) holds writes and schedules
//! captures; `agentfs record` uses [`group_members`] to record a whole
//! group at once.

use std::time::Duration;

use crate::config::{parse_duration, parse_size};
use crate::paths::RepoPath;
use crate::rules::{AttrState, Attributes};

/// `agentfs.commit`: the boundary class (design §4.2).
pub const COMMIT_ATTR: &str = "agentfs.commit";
/// `agentfs.coalesce`: the shortest interval between captures of a group.
pub const COALESCE_ATTR: &str = "agentfs.coalesce";
/// `agentfs.group`: paths with the same value and stem are one group.
pub const GROUP_ATTR: &str = "agentfs.group";
/// `agentfs.maxSize`: the largest content recorded at a path (any path).
pub const MAX_SIZE_ATTR: &str = "agentfs.maxSize";

/// `agentfs.coalesce` when unset.
pub const DEFAULT_COALESCE: Duration = Duration::from_secs(30);
/// The shortest `agentfs.coalesce` accepted.
pub const MIN_COALESCE: Duration = Duration::from_secs(1);

/// A path's fsync-class settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsyncClass {
    /// `agentfs.coalesce`, [`DEFAULT_COALESCE`] when unset.
    pub coalesce: Duration,
    /// `agentfs.group`; `None` makes the path a group of one.
    pub group: Option<String>,
}

impl FsyncClass {
    /// The class settings in `attrs`, if `agentfs.commit=fsync` is among
    /// them. Values were validated when the rules were loaded
    /// ([`validate_value`]); a value that does not parse here (rules built
    /// without validation, in tests) falls back to the default.
    pub fn from_attributes(attrs: &Attributes) -> Option<Self> {
        match attrs.get(COMMIT_ATTR) {
            Some(AttrState::Value(v)) if v == "fsync" => {}
            _ => return None,
        }
        let coalesce = match attrs.get(COALESCE_ATTR) {
            Some(AttrState::Value(v)) => parse_coalesce(v).unwrap_or(DEFAULT_COALESCE),
            _ => DEFAULT_COALESCE,
        };
        let group = match attrs.get(GROUP_ATTR) {
            Some(AttrState::Value(v)) if !v.is_empty() => Some(v.clone()),
            _ => None,
        };
        Some(Self { coalesce, group })
    }
}

/// `agentfs.maxSize` in `attrs`, in bytes, if set to a valid size.
pub fn max_size(attrs: &Attributes) -> Option<u64> {
    match attrs.get(MAX_SIZE_ATTR) {
        Some(AttrState::Value(v)) => parse_size(v),
        _ => None,
    }
}

fn parse_coalesce(value: &str) -> Option<Duration> {
    parse_duration(value).filter(|d| *d >= MIN_COALESCE)
}

/// What follows the stem in a file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suffix {
    None,
    Wal,
    Journal,
    Shm,
}

const SUFFIXES: [(&[u8], Suffix); 3] = [
    (b"-wal", Suffix::Wal),
    (b"-journal", Suffix::Journal),
    (b"-shm", Suffix::Shm),
];

/// Splits a file name into its stem and suffix: `state.db-wal` is
/// (`state.db`, [`Suffix::Wal`]). A name that is only a suffix (`-wal`)
/// has no stem and is its own stem.
pub fn stem(name: &[u8]) -> (&[u8], Suffix) {
    for (suffix, kind) in SUFFIXES {
        if let Some(stem) = name.strip_suffix(suffix)
            && !stem.is_empty()
        {
            return (stem, kind);
        }
    }
    (name, Suffix::None)
}

/// The names a group with `stem` can hold, `-shm` excepted: the stem
/// itself, `<stem>-wal` and `<stem>-journal`.
pub fn member_names(stem: &[u8]) -> [Vec<u8>; 3] {
    let with = |suffix: &[u8]| [stem, suffix].concat();
    [stem.to_vec(), with(b"-wal"), with(b"-journal")]
}

/// Whether `name` is a SQLite shared-memory file (`<stem>-shm`), which is
/// never recorded in the class.
pub fn is_shm(name: &[u8]) -> bool {
    stem(name).1 == Suffix::Shm
}

/// What identifies a group: its directory, group name and stem. A group of
/// one has no name and its whole file name as the stem.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupKey {
    /// The directory, `""` for the root.
    pub dir: String,
    pub group: Option<String>,
    pub stem: String,
}

/// A group as [`group_members`] finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMembers {
    pub key: GroupKey,
    /// Every member path, sorted; absent members included.
    pub members: Vec<RepoPath>,
    /// The smallest `agentfs.coalesce` among the members.
    pub coalesce: Duration,
}

/// The group `path` belongs to, given its class, with `class_of` telling
/// the class of each candidate sibling. `None` for a `-shm` path, which is
/// never a member.
pub fn group_members(
    path: &RepoPath,
    class: &FsyncClass,
    class_of: impl Fn(&RepoPath) -> Option<FsyncClass>,
) -> Option<GroupMembers> {
    let name = path.file_name();
    if is_shm(name.as_bytes()) {
        return None;
    }
    let dir = match path.as_str().rsplit_once('/') {
        Some((dir, _)) => dir.to_owned(),
        None => String::new(),
    };
    let Some(group) = &class.group else {
        return Some(GroupMembers {
            key: GroupKey {
                dir,
                group: None,
                stem: name.to_owned(),
            },
            members: vec![path.clone()],
            coalesce: class.coalesce,
        });
    };
    let (stem_bytes, _) = stem(name.as_bytes());
    // The stem is a prefix of a valid UTF-8 name ending before an ASCII
    // `-`, so it is valid UTF-8 too.
    let stem_text = String::from_utf8_lossy(stem_bytes).into_owned();
    let mut members = Vec::new();
    let mut coalesce = class.coalesce;
    for candidate in member_names(stem_bytes) {
        let candidate = String::from_utf8_lossy(&candidate).into_owned();
        let full = if dir.is_empty() {
            candidate
        } else {
            format!("{dir}/{candidate}")
        };
        let Ok(member) = RepoPath::new(full) else {
            continue;
        };
        if member == *path {
            members.push(member);
            continue;
        }
        if let Some(c) = class_of(&member)
            && c.group.as_ref() == Some(group)
        {
            coalesce = coalesce.min(c.coalesce);
            members.push(member);
        }
    }
    members.sort();
    Some(GroupMembers {
        key: GroupKey {
            dir,
            group: Some(group.clone()),
            stem: stem_text,
        },
        members,
        coalesce,
    })
}

/// Checks the value of an attribute the operator wrote in
/// `info/attributes` (design §4.2): `agentfs.commit` is `close` or
/// `fsync`, `agentfs.coalesce` a duration of at least [`MIN_COALESCE`],
/// `agentfs.maxSize` a positive size (`512k`, `100m`, `2g`). Other
/// attributes are not checked. `None` is an attribute set without a value
/// (`agentfs.commit` alone), which these need.
pub fn validate_value(attr: &str, value: Option<&str>) -> Result<(), String> {
    let check = |ok: bool, expected: &str| {
        if ok {
            Ok(())
        } else {
            Err(format!("expected {expected}"))
        }
    };
    match attr {
        COMMIT_ATTR => check(
            matches!(value, Some("close" | "fsync")),
            "`close` or `fsync`",
        ),
        COALESCE_ATTR => check(
            value.and_then(parse_coalesce).is_some(),
            "a duration of at least 1s (e.g. 5s, 30s, 2m)",
        ),
        MAX_SIZE_ATTR => check(
            value.and_then(parse_size).is_some(),
            "a positive size (e.g. 512k, 100m, 2g)",
        ),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn attrs(pairs: &[(&str, &str)]) -> Attributes {
        Attributes::from_map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), AttrState::Value((*v).to_owned())))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn stems_and_suffixes() {
        assert_eq!(stem(b"state.db"), (&b"state.db"[..], Suffix::None));
        assert_eq!(stem(b"state.db-wal"), (&b"state.db"[..], Suffix::Wal));
        assert_eq!(
            stem(b"state.db-journal"),
            (&b"state.db"[..], Suffix::Journal)
        );
        assert_eq!(stem(b"state.db-shm"), (&b"state.db"[..], Suffix::Shm));
        // A name that is only a suffix is its own stem.
        assert_eq!(stem(b"-wal"), (&b"-wal"[..], Suffix::None));
        assert_eq!(stem(b"x-wal-wal"), (&b"x-wal"[..], Suffix::Wal));
        assert!(is_shm(b"a-shm"));
        assert!(!is_shm(b"-shm"));
        assert!(!is_shm(b"a-shmx"));
        assert_eq!(
            member_names(b"s.db"),
            [
                b"s.db".to_vec(),
                b"s.db-wal".to_vec(),
                b"s.db-journal".to_vec()
            ]
        );
    }

    #[test]
    fn class_from_attributes() {
        assert_eq!(FsyncClass::from_attributes(&attrs(&[])), None);
        assert_eq!(
            FsyncClass::from_attributes(&attrs(&[("agentfs.commit", "close")])),
            None
        );
        assert_eq!(
            FsyncClass::from_attributes(&attrs(&[("agentfs.commit", "fsync")])),
            Some(FsyncClass {
                coalesce: DEFAULT_COALESCE,
                group: None
            })
        );
        assert_eq!(
            FsyncClass::from_attributes(&attrs(&[
                ("agentfs.commit", "fsync"),
                ("agentfs.coalesce", "5s"),
                ("agentfs.group", "sqlite"),
            ])),
            Some(FsyncClass {
                coalesce: Duration::from_secs(5),
                group: Some("sqlite".into())
            })
        );
        // Unvalidated nonsense falls back to the default.
        assert_eq!(
            FsyncClass::from_attributes(&attrs(&[
                ("agentfs.commit", "fsync"),
                ("agentfs.coalesce", "10ms"),
            ]))
            .unwrap()
            .coalesce,
            DEFAULT_COALESCE
        );
        assert_eq!(
            max_size(&attrs(&[("agentfs.maxSize", "2m")])),
            Some(2 << 20)
        );
        assert_eq!(max_size(&attrs(&[])), None);
    }

    #[test]
    fn members_of_a_named_group() {
        let sqlite = |c: u64| FsyncClass {
            coalesce: Duration::from_secs(c),
            group: Some("sqlite".into()),
        };
        // `d/state.db-journal` carries another group; `d/state.db-wal` 2s.
        let class_of = |path: &RepoPath| match path.as_str() {
            "d/state.db" => Some(sqlite(30)),
            "d/state.db-wal" => Some(sqlite(2)),
            "d/state.db-journal" => Some(FsyncClass {
                coalesce: DEFAULT_COALESCE,
                group: Some("other".into()),
            }),
            _ => None,
        };
        let g = group_members(&p("d/state.db-wal"), &sqlite(2), class_of).unwrap();
        assert_eq!(
            g.key,
            GroupKey {
                dir: "d".into(),
                group: Some("sqlite".into()),
                stem: "state.db".into()
            }
        );
        assert_eq!(g.members, vec![p("d/state.db"), p("d/state.db-wal")]);
        assert_eq!(g.coalesce, Duration::from_secs(2));
        // From the database itself, at the root.
        let g = group_members(&p("x.db"), &sqlite(30), |path| {
            (path.as_str() != "x.db-journal").then(|| sqlite(30))
        })
        .unwrap();
        assert_eq!(g.key.dir, "");
        assert_eq!(g.members, vec![p("x.db"), p("x.db-wal")]);
        // -shm is never a member.
        assert_eq!(
            group_members(&p("d/state.db-shm"), &sqlite(2), class_of),
            None
        );
    }

    #[test]
    fn a_class_path_without_a_group_is_a_group_of_one() {
        let class = FsyncClass {
            coalesce: DEFAULT_COALESCE,
            group: None,
        };
        let g = group_members(&p("a/b.db-wal"), &class, |_| Some(class.clone())).unwrap();
        assert_eq!(
            g.key,
            GroupKey {
                dir: "a".into(),
                group: None,
                stem: "b.db-wal".into()
            }
        );
        assert_eq!(g.members, vec![p("a/b.db-wal")]);
    }

    #[test]
    fn values_are_validated() {
        for (attr, value) in [
            ("agentfs.commit", "close"),
            ("agentfs.commit", "fsync"),
            ("agentfs.coalesce", "1s"),
            ("agentfs.coalesce", "2m"),
            ("agentfs.maxSize", "100"),
            ("agentfs.maxSize", "64m"),
            ("agentfs.group", "anything"),
            ("other", "x"),
        ] {
            assert_eq!(validate_value(attr, Some(value)), Ok(()), "{attr}={value}");
        }
        for (attr, value) in [
            ("agentfs.commit", Some("always")),
            ("agentfs.commit", None),
            ("agentfs.coalesce", Some("500ms")),
            ("agentfs.coalesce", Some("soon")),
            ("agentfs.coalesce", None),
            ("agentfs.maxSize", Some("0")),
            ("agentfs.maxSize", Some("-1")),
            ("agentfs.maxSize", Some("big")),
        ] {
            assert!(validate_value(attr, value).is_err(), "{attr}={value:?}");
        }
    }
}
