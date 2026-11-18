//! Directories, metadata and links (MODEL.md rules 13, 14 and 15): what a tree
//! holds beside its files' lines, how a landing judges it, and what a sync keeps.
//!
//! An entry is a file (a symbolic link is a file, rule 15) or a directory. Each has
//! one metadata value ([`Meta`]): its kind, mode bits, owner and group. History
//! records no file times. A file's metadata follows its identity; a directory has
//! no identity: it is its path (rule 14). The root directory is always there and
//! has no record.
//!
//! The model's layout, beside each file's blob: `meta.<identity>` holds a file's
//! metadata when it is not the default ([`Meta::FILE`]), and `dir.<path>` holds a
//! directory's (its path with `%` written `%25` and `/` written `%2F`, so every
//! record is a leaf of the commit's tree). A tree that never made a directory or
//! changed metadata holds neither, so its commits are what they were before these
//! rules.
//!
//! [`land`] and [`sync`] are pure: the world reads the entries from git
//! ([`read_raw`], [`Entries::read`]), asks them, and writes what they answer. The
//! checks read the same records from git with the same reader.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::content::Content;
use crate::git::git;

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    File,
    Symlink,
    Dir,
}

/// Rule 13: an entry's metadata, one value: its kind, its permission bits
/// (`mode & 0o7777`), its owner and its group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Meta {
    pub kind: Kind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Meta {
    /// A created file's metadata, until a call sets another.
    pub const FILE: Meta = Meta::file(0o644, 0, 0);

    pub const fn file(mode: u32, uid: u32, gid: u32) -> Self {
        Self {
            kind: Kind::File,
            mode: mode & 0o7777,
            uid,
            gid,
        }
    }

    pub const fn symlink(uid: u32, gid: u32) -> Self {
        Self {
            kind: Kind::Symlink,
            mode: 0o777,
            uid,
            gid,
        }
    }

    pub const fn dir(mode: u32, uid: u32, gid: u32) -> Self {
        Self {
            kind: Kind::Dir,
            mode: mode & 0o7777,
            uid,
            gid,
        }
    }

    /// The same metadata with these mode bits (`chmod`).
    pub const fn with_mode(self, mode: u32) -> Self {
        Self {
            mode: mode & 0o7777,
            ..self
        }
    }

    /// The same metadata with this owner and group (`chown`).
    pub const fn with_owner(self, uid: u32, gid: u32) -> Self {
        Self { uid, gid, ..self }
    }

    /// Its text in a record and in a collision: `file:0644:1000:1000`.
    pub fn text(&self) -> String {
        let kind = match self.kind {
            Kind::File => "file",
            Kind::Symlink => "symlink",
            Kind::Dir => "dir",
        };
        format!("{kind}:{:04o}:{}:{}", self.mode, self.uid, self.gid)
    }

    /// A record as [`Meta::text`] writes it.
    pub fn parse(s: &str) -> Option<Self> {
        let mut f = s.trim().split(':');
        let kind = match f.next()? {
            "file" => Kind::File,
            "symlink" => Kind::Symlink,
            "dir" => Kind::Dir,
            _ => return None,
        };
        let mode = u32::from_str_radix(f.next()?, 8).ok()?;
        let uid = f.next()?.parse().ok()?;
        let gid = f.next()?.parse().ok()?;
        f.next().is_none().then_some(Self {
            kind,
            mode,
            uid,
            gid,
        })
    }
}

/// The text of a value in a collision: its metadata, or `-` for none (the
/// entry is not there).
pub fn value_text(v: Option<Meta>) -> String {
    v.map_or_else(|| "-".to_string(), |m| m.text())
}

/// The prefix of a file's metadata record in a commit's tree.
pub const META: &str = "meta.";
/// The prefix of a directory's record in a commit's tree.
pub const DIR: &str = "dir.";

/// The record holding file `id`'s metadata.
pub fn meta_name(id: &str) -> String {
    format!("{META}{id}")
}

/// The record of directory `p`.
pub fn dir_name(p: &str) -> String {
    format!("{DIR}{}", p.replace('%', "%25").replace('/', "%2F"))
}

/// The directory a record names, if it is a directory's.
pub fn dir_of(name: &str) -> Option<String> {
    name.strip_prefix(DIR)
        .map(|e| e.replace("%2F", "/").replace("%25", "%"))
}

/// Whether a name in a commit's tree is one of these records, not a file's blob.
pub fn is_record(name: &str) -> bool {
    name.starts_with(META) || name.starts_with(DIR)
}

/// The directory holding path `p`: `""` for the root.
pub fn parent(p: &str) -> &str {
    p.rfind('/').map_or("", |i| &p[..i])
}

/// The records of one commit's tree, by name, with their text.
pub type Raw = BTreeMap<String, String>;

/// The records commit or tree `at` holds in repository `repo`, read from git.
pub fn read_raw(repo: &Path, at: &str) -> Raw {
    let mut out = Raw::new();
    for line in git(repo, &["ls-tree", at]).lines() {
        let Some((head, name)) = line.split_once('\t') else {
            continue;
        };
        if let Some(blob) = head.split_whitespace().nth(2)
            && is_record(name)
        {
            out.insert(name.to_string(), git(repo, &["cat-file", "-p", blob]));
        }
    }
    out
}

/// Writes over a tree: each record's new text, or none to remove it.
pub type Writes = BTreeMap<String, Option<String>>;

/// Rule 13's key: a file by identity, or a directory by path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    File(String),
    Dir(String),
}

impl Key {
    /// Its name in a collision record: a file's identity, a directory's path
    /// and a `/`.
    pub fn label(&self) -> String {
        match self {
            Key::File(id) => id.clone(),
            Key::Dir(p) => format!("{p}/"),
        }
    }
}

/// One entry at a path: a file (with its identity) or a directory.
pub type Entry = (String, Option<String>);

/// What a tree holds beside its files' lines: each live file's path and
/// metadata, and each directory's metadata. Read from git by the world and
/// by the checks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entries {
    /// Each live file by identity: its path and its metadata.
    pub files: BTreeMap<String, (String, Meta)>,
    /// Each directory by path, the root's excluded.
    pub dirs: BTreeMap<String, Meta>,
}

impl Entries {
    /// A tree's entries: its records `raw`, and its files `files` (by
    /// identity, as they are at that tree).
    pub fn read<'a>(raw: &Raw, files: impl IntoIterator<Item = (&'a str, Content)>) -> Self {
        let mut e = Self::default();
        for (id, c) in files {
            if let Some(p) = c.path() {
                let m = raw
                    .get(&meta_name(id))
                    .map(|t| Meta::parse(t).expect("a metadata record"))
                    .unwrap_or(Meta::FILE);
                e.files.insert(id.to_string(), (p.to_string(), m));
            }
        }
        for (name, text) in raw {
            if let Some(p) = dir_of(name) {
                e.dirs
                    .insert(p, Meta::parse(text).expect("a directory record"));
            }
        }
        e
    }

    /// Rule 13's value of `k` here: its metadata, none if it is not here.
    pub fn value(&self, k: &Key) -> Option<Meta> {
        match k {
            Key::File(id) => self.files.get(id).map(|(_, m)| *m),
            Key::Dir(p) => self.dirs.get(p).copied(),
        }
    }

    fn keys(&self) -> impl Iterator<Item = Key> + '_ {
        let files = self.files.keys().map(|id| Key::File(id.clone()));
        files.chain(self.dirs.keys().map(|p| Key::Dir(p.clone())))
    }

    /// Whether `p` is a directory here (the root always is).
    pub fn is_dir(&self, p: &str) -> bool {
        p.is_empty() || self.dirs.contains_key(p)
    }

    /// Every entry: a file's path and identity, a directory's path.
    pub fn all(&self) -> Vec<Entry> {
        let files = self
            .files
            .iter()
            .map(|(id, (p, _))| (p.clone(), Some(id.clone())));
        files
            .chain(self.dirs.keys().map(|p| (p.clone(), None)))
            .collect()
    }

    /// The entries under directory `d`, at any depth.
    pub fn under(&self, d: &str) -> BTreeSet<Entry> {
        let prefix = format!("{d}/");
        self.all()
            .into_iter()
            .filter(|(p, _)| p.starts_with(&prefix))
            .collect()
    }

    /// The entries at path `p`: one, unless invariant D1 is broken.
    pub fn at(&self, p: &str) -> Vec<Entry> {
        self.all().into_iter().filter(|(q, _)| q == p).collect()
    }

    /// Invariant D1: every entry's parent is a directory here, and no path
    /// names two entries. The first entry that breaks it, and how.
    pub fn d1(&self) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        for (p, id) in self.all() {
            if !seen.insert(p.clone()) {
                return Err(format!("{p} names two entries"));
            }
            let d = parent(&p);
            if !self.is_dir(d) {
                let what = id.map_or("directory".to_string(), |f| format!("file {f}"));
                return Err(format!("{what} at {p}: its parent {d} is not a directory"));
            }
        }
        Ok(())
    }
}

/// Why a change lost by rules 13 and 14: the cause a collision record names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// Rule 13: the upstream holds a value the writer never saw.
    Metadata,
    /// Rule 14: the change removes or moves a directory, or puts an entry onto
    /// a path, where the upstream has an entry its writer never saw.
    DirectoryNotEmpty,
    /// Rule 14: the change puts an entry into a directory the upstream no
    /// longer has, and did not make that directory itself.
    DirectoryGone,
}

/// One loss of a change by rule 13 or 14: the entry it is about, what the
/// change had there and what the upstream had that decided it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loss {
    pub cause: Cause,
    /// A file's identity, or a directory's path and a `/` ([`Key::label`]).
    pub path: String,
    pub lost: String,
    pub won: String,
}

/// The entries a change puts at a path that its first parent did not have
/// there: a file created, renamed or moved with its directory, a directory made
/// or moved.
fn arriving(base: &Entries, tip: &Entries) -> Vec<Entry> {
    let old: BTreeSet<Entry> = base.all().into_iter().collect();
    tip.all().into_iter().filter(|e| !old.contains(e)).collect()
}

/// Rules 13 and 14 at the upstream's queue: a change from `base` (its first
/// parent) to `tip` into the upstream at `ours`. Empty: what it does here may
/// land; else every loss, and the change loses whole (rule 3e).
pub fn land(base: &Entries, tip: &Entries, ours: &Entries) -> Vec<Loss> {
    let mut out = Vec::new();
    // Rule 13: a new value lands over the value its writer saw, or onto the same
    // new value; else the upstream's value wins. Making or removing an entry is a
    // value too, so two trees making one directory agree.
    let keys: BTreeSet<Key> = base.keys().chain(tip.keys()).collect();
    for k in keys {
        let (b, t, o) = (base.value(&k), tip.value(&k), ours.value(&k));
        if b != t && o != b && o != t {
            out.push(Loss {
                cause: Cause::Metadata,
                path: k.label(),
                lost: value_text(t),
                won: value_text(o),
            });
        }
    }
    // Rule 14: a directory the change removes or moves away, where the upstream has
    // an entry under it that the change's first parent lacks.
    for d in base.dirs.keys().filter(|d| !tip.dirs.contains_key(*d)) {
        let seen = base.under(d);
        if let Some((p, _)) = ours.under(d).into_iter().find(|e| !seen.contains(e)) {
            out.push(Loss {
                cause: Cause::DirectoryNotEmpty,
                path: Key::Dir(d.clone()).label(),
                lost: value_text(tip.value(&Key::Dir(d.clone()))),
                won: p,
            });
        }
    }
    for (p, id) in arriving(base, tip) {
        // Rule 14: an entry onto a path where the upstream has an entry of the other
        // kind its writer never saw (two files at one path are rule 11's); a
        // directory moved there, with what is under it, where the upstream has an
        // entry under that path its writer never saw. A directory made empty
        // agrees with the upstream's.
        let seen: BTreeSet<Entry> = base.at(&p).into_iter().chain(base.under(&p)).collect();
        let clash = ours
            .at(&p)
            .into_iter()
            .find(|e| e.1.is_some() != id.is_some() && !seen.contains(e));
        let filled = (id.is_none() && !tip.under(&p).is_empty())
            .then(|| ours.under(&p).into_iter().find(|e| !seen.contains(e)))
            .flatten();
        if let Some((q, _)) = clash.or(filled) {
            out.push(Loss {
                cause: Cause::DirectoryNotEmpty,
                path: id.clone().unwrap_or_else(|| Key::Dir(p.clone()).label()),
                lost: p.clone(),
                won: q,
            });
        }
        // Rule 14: an entry into a directory the upstream no longer has, which the
        // change did not make itself.
        let d = parent(&p);
        if !ours.is_dir(d) && base.is_dir(d) {
            out.push(Loss {
                cause: Cause::DirectoryGone,
                path: Key::Dir(d.to_string()).label(),
                lost: p,
                won: "-".to_string(),
            });
        }
    }
    out
}

/// The records a change writes: each one that differs from its first parent
/// `base` to `tip`, with `tip`'s text.
pub fn changes(base: &Raw, tip: &Raw) -> Writes {
    base.keys()
        .chain(tip.keys())
        .filter(|n| base.get(*n) != tip.get(*n))
        .map(|n| (n.clone(), tip.get(n).cloned()))
        .collect()
}

/// Rules 13 and 14, upstream into fork: the records a sync writes over the
/// upstream's `up`. It decides nothing: each record where the fork (`ours`)
/// differs from the upstream takes the upstream's, unless the fork's is
/// undecided (`decided` says no: the upstream's queue has not taken the commit
/// that wrote it), which stays. Then every directory the sync's result
/// (`files`: each live file's path there) needs for an entry stays as the fork
/// has it, until the upstream decides.
pub fn sync(
    ours: &Raw,
    up: &Raw,
    files: &BTreeSet<String>,
    decided: impl Fn(&str) -> bool,
) -> Writes {
    let mut writes: Writes = changes(up, ours)
        .into_iter()
        .filter(|(n, _)| !decided(n))
        .collect();
    let mut dirs: BTreeSet<String> = up.keys().filter_map(|n| dir_of(n)).collect();
    for (n, t) in &writes {
        if let Some(d) = dir_of(n) {
            match t {
                Some(_) => dirs.insert(d),
                None => dirs.remove(&d),
            };
        }
    }
    let paths: BTreeSet<String> = files.iter().chain(&dirs).cloned().collect();
    for p in &paths {
        let mut d = parent(p);
        while !d.is_empty() {
            if !dirs.contains(d) {
                let name = dir_name(d);
                let kept = ours.get(&name).unwrap_or_else(|| {
                    panic!(
                        "a sync puts {p} under {d}, which the fork's undecided change removed: \
                         not modelled (MODEL.md, \"Left out\")"
                    )
                });
                writes.insert(name, Some(kept.clone()));
                dirs.insert(d.to_string());
            }
            d = parent(d);
        }
    }
    writes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_reads_back_and_records_never_nest() {
        for m in [
            Meta::FILE,
            Meta::file(0o4755, 1000, 1000),
            Meta::symlink(1, 2),
            Meta::dir(0o2775, 0, 100),
        ] {
            assert_eq!(Meta::parse(&m.text()), Some(m));
        }
        assert_eq!(Meta::FILE.text(), "file:0644:0:0");
        let p = "a%b/c/d";
        assert_eq!(dir_name(p), "dir.a%25b%2Fc%2Fd");
        assert_eq!(dir_of(&dir_name(p)).as_deref(), Some(p));
        assert!(!dir_name(p).contains('/'));
        assert_eq!(parent("a/b/c"), "a/b");
        assert_eq!(parent("a"), "");
    }
}
