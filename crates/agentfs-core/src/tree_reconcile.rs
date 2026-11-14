//! Line maps for tree landing (target architecture §7): which version wrote each
//! line of a file and which line it is, never the line's bytes.
//!
//! A file's bytes have one home: its blob, its checkout or the journal frames
//! that hold them. A [`LineMap`] says how those bytes divide into lines and names
//! each line's identity (its first version) and current version, as runs of
//! consecutive lines numbered on by one writer, the way `git blame` keeps ranges
//! of lines rather than lines. A new file of any size is one run, and an append
//! adds a run, so a map costs its edits, not its file's size. The alignment of a
//! save reads both images through [`Image`], a line at a time, and keeps a
//! sampled index of where lines start ([`LineIndex`]) rather than the lines.
//!
//! The caller supplies volume-unique version IDs, numbered on within a writer
//! ([`Id`]). Metadata must be persisted with the corresponding content and all
//! ancestors retained; a line's identity is independent of a filesystem object's
//! identity.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::hash::Hash;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

pub use agentfs_align::Hunk;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// A caller's version identifier: volume-unique, written once, and numbered on
/// within its writer: a writer's new versions are `at(w, i)`, `at(w, i + 1)`, ...
/// A line's identity is its first version.
pub trait Id: Copy + Ord + Hash + fmt::Debug + fmt::Display + Serialize + DeserializeOwned {
    /// The `index`-th version of `writer`.
    fn at(writer: u64, index: u64) -> Self;
    /// The writer that numbered it.
    fn writer(&self) -> u64;
    /// Its place among the writer's versions.
    fn index(&self) -> u64;
    /// The version `n` after this one in its writer's numbering.
    fn nth(&self, n: u64) -> Self {
        Self::at(self.writer(), self.index() + n)
    }
}

/// Bytes read by offset: a blob in memory, a file, or a file map.
pub trait Image {
    /// The image's length in bytes.
    fn len(&self) -> u64;
    /// Reads at `offset`, as `pread(2)`: fewer bytes only at the end.
    ///
    /// # Errors
    ///
    /// The reader's own.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    /// True for an image of no bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Image for &[u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        let Some(rest) = self.get(start..) else {
            return Ok(0);
        };
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        Ok(n)
    }
}

impl Image for Vec<u8> {
    fn len(&self) -> u64 {
        self.as_slice().len() as u64
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.as_slice().read_at(buf, offset)
    }
}

/// A file read as an image; its length is taken once, at open.
pub struct FileImage {
    file: File,
    len: u64,
}

impl FileImage {
    /// Opens `path` for reading.
    ///
    /// # Errors
    ///
    /// The open's or the `stat`'s.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }
}

impl FileImage {
    /// An open file, its length taken now.
    ///
    /// # Errors
    ///
    /// The `stat`'s.
    pub fn of(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }
}

impl Image for FileImage {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let n = buf
            .len()
            .min(usize::try_from(self.len - offset).unwrap_or(usize::MAX));
        self.file.read_at(&mut buf[..n], offset)
    }
}

/// `len` bytes of another image from `start`.
pub struct Window<'a> {
    image: &'a dyn Image,
    start: u64,
    len: u64,
}

impl<'a> Window<'a> {
    /// The bytes `start..start + len` of `image`, cut at its end.
    pub fn new(image: &'a dyn Image, start: u64, len: u64) -> Self {
        let len = len.min(image.len().saturating_sub(start));
        Self { image, start, len }
    }
}

impl Image for Window<'_> {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let n = buf
            .len()
            .min(usize::try_from(self.len - offset).unwrap_or(usize::MAX));
        self.image.read_at(&mut buf[..n], self.start + offset)
    }
}

/// The bytes scanned at a time.
const CHUNK: usize = 64 << 10;
/// One line start kept in a [`LineIndex`] per this many lines.
const STRIDE: u64 = 64;

/// Reads `buf.len()` bytes at `offset`, fewer only at the image's end.
fn read_full(image: &dyn Image, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        let n = image.read_at(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok(done)
}

/// The offset just past `n` newlines from `from`, or the image's end if it has
/// fewer.
fn skip_lines(image: &dyn Image, from: u64, n: u64) -> io::Result<u64> {
    if n == 0 {
        return Ok(from);
    }
    let mut buf = vec![0; CHUNK];
    let (mut at, mut left) = (from, n);
    loop {
        let got = read_full(image, &mut buf, at)?;
        if got == 0 {
            return Ok(at);
        }
        for (i, b) in buf[..got].iter().enumerate() {
            if *b == b'\n' {
                left -= 1;
                if left == 0 {
                    return Ok(at + i as u64 + 1);
                }
            }
        }
        at += got as u64;
    }
}

/// Where an image's lines start, sampled: every [`STRIDE`]-th line's offset, how
/// many lines it has, and whether its last line lacks a newline. Lines split
/// after each newline, as git's text diff reads them.
#[derive(Clone, Debug)]
pub struct LineIndex {
    len: u64,
    count: u64,
    starts: Vec<u64>,
    open: bool,
}

impl LineIndex {
    /// One pass over `image`.
    ///
    /// # Errors
    ///
    /// The image's read errors.
    pub fn of(image: &dyn Image) -> io::Result<Self> {
        let len = image.len();
        let mut buf = vec![0; CHUNK];
        let (mut at, mut count, mut starts) = (0_u64, 0_u64, vec![0_u64]);
        let mut last = None;
        loop {
            let got = read_full(image, &mut buf, at)?;
            if got == 0 {
                break;
            }
            for (i, b) in buf[..got].iter().enumerate() {
                if *b == b'\n' {
                    count += 1;
                    if count % STRIDE == 0 {
                        starts.push(at + i as u64 + 1);
                    }
                }
            }
            last = buf[..got].last().copied();
            at += got as u64;
        }
        let open = len > 0 && last != Some(b'\n');
        if open {
            count += 1;
        }
        Ok(Self {
            len,
            count,
            starts,
            open,
        })
    }

    /// How many lines the image has.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Whether its last line lacks a newline.
    pub fn open(&self) -> bool {
        self.open
    }

    /// Where line `k` starts; the image's length for `k` at the count.
    fn start(&self, image: &dyn Image, k: u64) -> io::Result<u64> {
        if k >= self.count {
            return Ok(self.len);
        }
        let sample = usize::try_from(k / STRIDE).unwrap_or(usize::MAX);
        let from = *self.starts.get(sample).ok_or(io::ErrorKind::InvalidData)?;
        skip_lines(image, from, k % STRIDE)
    }

    /// Where line `k` starts and its length, its newline included.
    fn span(&self, image: &dyn Image, k: u64) -> io::Result<(u64, u64)> {
        let start = self.start(image, k)?;
        let end = if k + 1 >= self.count {
            self.len
        } else {
            skip_lines(image, start, 1)?
        };
        Ok((start, end - start))
    }
}

/// Whether `a`'s bytes at `x` equal `b`'s at `y`, `len` of them.
fn same_bytes(a: &dyn Image, x: u64, b: &dyn Image, y: u64, len: u64) -> io::Result<bool> {
    let (mut p, mut q) = (vec![0; CHUNK], vec![0; CHUNK]);
    let mut done = 0;
    while done < len {
        let n = usize::try_from(len - done).unwrap_or(usize::MAX).min(CHUNK);
        let got = read_full(a, &mut p[..n], x + done)?;
        if got != n || read_full(b, &mut q[..n], y + done)? != n || p[..n] != q[..n] {
            return Ok(false);
        }
        done += n as u64;
    }
    Ok(true)
}

/// Whether two images hold the same bytes.
fn same_image(a: &dyn Image, b: &dyn Image) -> io::Result<bool> {
    Ok(a.len() == b.len() && same_bytes(a, 0, b, 0, a.len())?)
}

/// A run of lines: `count` lines whose identities and versions number on from the
/// first's, all live or all deleted. Only a live run's last line may lack its
/// newline; `open` is then that line's length in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(serialize = "V: Serialize", deserialize = "V: DeserializeOwned"))]
pub struct Run<V> {
    pub identity: V,
    pub version: V,
    pub count: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dead: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<u64>,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_false(b: &bool) -> bool {
    !*b
}

impl<V: Id> Run<V> {
    /// Lines `from..from + n` of the run.
    fn part(&self, from: u64, n: u64) -> Self {
        Self {
            identity: self.identity.nth(from),
            version: self.version.nth(from),
            count: n,
            dead: self.dead,
            open: if from + n == self.count {
                self.open
            } else {
                None
            },
        }
    }
}

/// Versions written over others: `count` versions from `version`, each over the
/// version numbered alike from `over`, or over none (a new line).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(serialize = "V: Serialize", deserialize = "V: DeserializeOwned"))]
pub struct Over<V> {
    pub version: V,
    pub count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over: Option<V>,
}

/// One line of a map, expanded: what the landing rules read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line<V> {
    pub identity: V,
    pub version: V,
    /// False once deleted. A deleted line keeps its identity.
    pub live: bool,
    /// A live line with no newline: its length in bytes.
    pub open: Option<u64>,
}

/// A map expanded line by line, with every version's ancestor: the form the
/// landing rules (`tree_actor::referee`) work on. Its size is its file's line
/// count, so only a take builds one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expanded<V> {
    pub lines: Vec<Line<V>>,
    pub ancestors: BTreeMap<V, Option<V>>,
}

impl<V> Default for Expanded<V> {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            ancestors: BTreeMap::new(),
        }
    }
}

/// A file's lines as runs and the versions each version was written over, as
/// runs. Always canonical: runs as long as they can be, ancestors sorted by
/// writer and place, so two maps of the same lines are equal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    bound(serialize = "V: Serialize", deserialize = "V: Id"),
    try_from = "Stored<V>"
)]
pub struct LineMap<V> {
    lines: Vec<Run<V>>,
    ancestors: Vec<Over<V>>,
}

/// A map as JSON holds it, before it is made canonical and checked.
#[derive(Deserialize)]
#[serde(bound(deserialize = "V: DeserializeOwned"))]
struct Stored<V> {
    lines: Vec<Run<V>>,
    ancestors: Vec<Over<V>>,
}

impl<V: Id> TryFrom<Stored<V>> for LineMap<V> {
    type Error = Error;

    fn try_from(stored: Stored<V>) -> Result<Self, Error> {
        Self::from_runs(stored.lines, stored.ancestors)
    }
}

impl<V> Default for LineMap<V> {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            ancestors: Vec::new(),
        }
    }
}

/// Failure to produce or interpret the histogram alignment.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("histogram diff: {0}")]
    Io(#[from] io::Error),
    #[error("git diff failed: {0}")]
    Git(String),
    #[error("invalid histogram output")]
    InvalidDiff,
    #[error("duplicate identifier: {0}")]
    DuplicateId(String),
    #[error("invalid line map: {0}")]
    InvalidMap(&'static str),
}

impl From<agentfs_align::Error> for Error {
    fn from(e: agentfs_align::Error) -> Self {
        match e {
            agentfs_align::Error::Io(e) => Self::Io(e),
            agentfs_align::Error::Git(message) => Self::Git(message),
            agentfs_align::Error::Invalid => Self::InvalidDiff,
        }
    }
}

/// Runs pushed in order, each joined to the last where it numbers on from it.
struct Runs<V>(Vec<Run<V>>);

impl<V: Id> Runs<V> {
    fn push(&mut self, run: Run<V>) {
        if run.count == 0 {
            return;
        }
        if let Some(last) = self.0.last_mut()
            && last.dead == run.dead
            && last.open.is_none()
            && last.identity.nth(last.count) == run.identity
            && last.version.nth(last.count) == run.version
        {
            last.count += run.count;
            last.open = run.open;
            return;
        }
        self.0.push(run);
    }
}

/// The key that sorts versions by writer, then place.
fn key<V: Id>(v: &V) -> (u64, u64) {
    (v.writer(), v.index())
}

/// `overs` sorted by writer and place and joined where they number on;
/// [`Error::InvalidMap`] for a version written twice.
fn canonical_overs<V: Id>(mut overs: Vec<Over<V>>) -> Result<Vec<Over<V>>, Error> {
    overs.retain(|o| o.count > 0);
    overs.sort_by_key(|o| key(&o.version));
    let mut out: Vec<Over<V>> = Vec::with_capacity(overs.len());
    for o in overs {
        if let Some(last) = out.last_mut() {
            let end = last.version.nth(last.count);
            if last.version.writer() == o.version.writer() && o.version.index() < end.index() {
                return Err(Error::DuplicateId(o.version.to_string()));
            }
            let joins = match (last.over, o.over) {
                (None, None) => true,
                (Some(a), Some(b)) => a.nth(last.count) == b,
                _ => false,
            };
            if end == o.version && joins {
                last.count += o.count;
                continue;
            }
        }
        out.push(o);
    }
    Ok(out)
}

/// Ranges of versions, sorted; [`Error::InvalidMap`] where two overlap.
fn disjoint<V: Id>(mut ranges: Vec<(V, u64)>, what: &'static str) -> Result<(), Error> {
    ranges.sort_by_key(|(v, _)| key(v));
    for pair in ranges.windows(2) {
        let (a, n) = pair[0];
        let (b, _) = pair[1];
        if a.writer() == b.writer() && b.index() < a.index() + n {
            return Err(Error::InvalidMap(what));
        }
    }
    Ok(())
}

/// A walk over a map's runs by live line.
struct Cursor<'m, V> {
    runs: &'m [Run<V>],
    at: usize,
    offset: u64,
    live: u64,
}

impl<V: Id> Cursor<'_, V> {
    /// Copies every line before live line `k`, the deleted lines just before it
    /// included; at the live count, every line left.
    fn copy_until_live(&mut self, k: u64, out: &mut Runs<V>) {
        while let Some(run) = self.runs.get(self.at) {
            if run.dead {
                out.push(run.part(self.offset, run.count - self.offset));
                self.at += 1;
                self.offset = 0;
                continue;
            }
            if self.live >= k {
                return;
            }
            let n = (run.count - self.offset).min(k - self.live);
            out.push(run.part(self.offset, n));
            self.offset += n;
            self.live += n;
            if self.offset == run.count {
                self.at += 1;
                self.offset = 0;
            }
        }
    }

    /// Up to `n` live lines from here, within one run; the cursor stands on a
    /// live line ([`Cursor::copy_until_live`]).
    fn take_live(&mut self, n: u64) -> Result<Run<V>, Error> {
        let run = self
            .runs
            .get(self.at)
            .filter(|r| !r.dead)
            .ok_or(Error::InvalidMap(
                "live records do not correspond to byte lines",
            ))?;
        let n = n.min(run.count - self.offset);
        let part = run.part(self.offset, n);
        self.offset += n;
        self.live += n;
        if self.offset == run.count {
            self.at += 1;
            self.offset = 0;
        }
        Ok(part)
    }
}

/// One changed region after trimming, with its first new versions.
struct Edit<V> {
    old_start: u64,
    old_count: u64,
    new_start: u64,
    new_count: u64,
    replaced: Option<V>,
    inserted: Option<V>,
}

impl<V: Id> LineMap<V> {
    /// The runs of lines, in file order.
    pub fn runs(&self) -> &[Run<V>] {
        &self.lines
    }

    /// The runs of versions and what each was written over.
    pub fn ancestors(&self) -> &[Over<V>] {
        &self.ancestors
    }

    /// How many lines are live.
    pub fn live_lines(&self) -> u64 {
        self.lines.iter().filter(|r| !r.dead).map(|r| r.count).sum()
    }

    /// A map from runs and ancestors, made canonical and checked.
    ///
    /// # Errors
    ///
    /// As [`LineMap::validate`].
    pub fn from_runs(lines: Vec<Run<V>>, ancestors: Vec<Over<V>>) -> Result<Self, Error> {
        let mut runs = Runs(Vec::with_capacity(lines.len()));
        for run in lines {
            if run.dead && run.open.is_some() {
                return Err(Error::InvalidMap("a deleted line with a length"));
            }
            runs.push(run);
        }
        let map = Self {
            lines: runs.0,
            ancestors: canonical_overs(ancestors)?,
        };
        map.validate()?;
        Ok(map)
    }

    /// The map line by line, every version with its ancestor.
    pub fn expand(&self) -> Expanded<V> {
        let mut lines = Vec::new();
        for run in &self.lines {
            for i in 0..run.count {
                lines.push(Line {
                    identity: run.identity.nth(i),
                    version: run.version.nth(i),
                    live: !run.dead,
                    open: if i + 1 == run.count { run.open } else { None },
                });
            }
        }
        let mut ancestors = BTreeMap::new();
        for o in &self.ancestors {
            for i in 0..o.count {
                ancestors.insert(o.version.nth(i), o.over.map(|v| v.nth(i)));
            }
        }
        Expanded { lines, ancestors }
    }

    /// The map an expanded one stands for.
    ///
    /// # Errors
    ///
    /// As [`LineMap::validate`].
    pub fn from_expanded(expanded: &Expanded<V>) -> Result<Self, Error> {
        Self::from_runs(
            expanded
                .lines
                .iter()
                .map(|l| Run {
                    identity: l.identity,
                    version: l.version,
                    count: 1,
                    dead: !l.live,
                    open: if l.live { l.open } else { None },
                })
                .collect(),
            expanded
                .ancestors
                .iter()
                .map(|(version, over)| Over {
                    version: *version,
                    count: 1,
                    over: *over,
                })
                .collect(),
        )
    }

    /// Serialize metadata for storage alongside the file in the same commit.
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Decode metadata, made canonical and checked.
    ///
    /// # Errors
    ///
    /// Bad JSON, or as [`LineMap::validate`].
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let stored: Stored<V> = serde_json::from_slice(bytes)
            .map_err(|_| Error::InvalidMap("not a line map's JSON"))?;
        Self::from_runs(stored.lines, stored.ancestors)
    }

    /// Check identities and ancestry: every identity and every version once,
    /// every line's version and every ancestor known, and each version over an
    /// earlier writer's, so ancestry has no cycle.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidMap`] naming what is wrong.
    pub fn validate(&self) -> Result<(), Error> {
        disjoint(
            self.lines.iter().map(|r| (r.identity, r.count)).collect(),
            "duplicate line or version",
        )?;
        disjoint(
            self.lines.iter().map(|r| (r.version, r.count)).collect(),
            "duplicate line or version",
        )?;
        let covered = |v: V, n: u64| -> bool {
            let (mut at, mut left) = (v, n);
            while left > 0 {
                let i = self
                    .ancestors
                    .partition_point(|o| key(&o.version) <= key(&at));
                let Some(o) = i.checked_sub(1).and_then(|i| self.ancestors.get(i)) else {
                    return false;
                };
                let end = o.version.index() + o.count;
                if o.version.writer() != at.writer() || at.index() >= end {
                    return false;
                }
                let n = (end - at.index()).min(left);
                at = at.nth(n);
                left -= n;
            }
            true
        };
        for run in &self.lines {
            if !covered(run.version, run.count) {
                return Err(Error::InvalidMap("missing version ancestry"));
            }
        }
        for o in &self.ancestors {
            if let Some(over) = o.over {
                if !covered(over, o.count) {
                    return Err(Error::InvalidMap("missing ancestor"));
                }
                if over.writer() >= o.version.writer() {
                    return Err(Error::InvalidMap("cyclic ancestry"));
                }
            }
        }
        Ok(())
    }

    /// Check that the live lines are `image`'s lines, as `index` divides them:
    /// as many, and only the last unfinished, with its length.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidMap`] when they differ; the image's read errors.
    pub fn check(&self, image: &dyn Image, index: &LineIndex) -> Result<(), Error> {
        let differ = Error::InvalidMap("live records do not correspond to byte lines");
        if self.live_lines() != index.count {
            return Err(differ);
        }
        let mut live = self.lines.iter().filter(|r| !r.dead);
        let last = live.next_back();
        if live.any(|r| r.open.is_some()) {
            return Err(differ);
        }
        match last.and_then(|r| r.open) {
            None if index.open => Err(differ),
            Some(_) if !index.open => Err(differ),
            Some(len) if index.span(image, index.count - 1)?.1 != len => Err(differ),
            _ => Ok(()),
        }
    }

    /// The map `suffix` appended to the file this map describes leaves:
    /// existing lines keep their place, an unfinished last line takes a new
    /// version over its old one, and the rest are new lines at the end.
    /// `next(n)` issues `n` fresh consecutive versions and returns the first.
    ///
    /// # Errors
    ///
    /// The image's read errors; as [`LineMap::validate`].
    pub fn append(
        &self,
        suffix: &dyn Image,
        mut next: impl FnMut(u64) -> V,
    ) -> Result<Self, Error> {
        let mut lines = self.lines.clone();
        let mut ancestors = self.ancestors.clone();
        let mut consumed = 0;
        let last = lines
            .iter()
            .rposition(|r| !r.dead)
            .filter(|i| lines[*i].open.is_some());
        if let Some(i) = last
            && !suffix.is_empty()
        {
            let run = lines[i];
            let end = skip_lines(suffix, 0, 1)?;
            consumed = end;
            let joined = run.part(run.count - 1, 1);
            let version = next(1);
            ancestors.push(Over {
                version,
                count: 1,
                over: Some(joined.version),
            });
            let ends = end > 0 && read_byte(suffix, end - 1)? == Some(b'\n');
            let line = Run {
                version,
                open: if ends {
                    None
                } else {
                    joined.open.map(|len| len + end)
                },
                ..joined
            };
            let mut replaced = Vec::with_capacity(2);
            if run.count > 1 {
                replaced.push(run.part(0, run.count - 1));
            }
            replaced.push(line);
            lines.splice(i..=i, replaced);
        }
        let rest = Window::new(suffix, consumed, suffix.len());
        let index = LineIndex::of(&rest)?;
        if index.count > 0 {
            let first = next(index.count);
            lines.push(Run {
                identity: first,
                version: first,
                count: index.count,
                dead: false,
                open: if index.open {
                    Some(index.span(&rest, index.count - 1)?.1)
                } else {
                    None
                },
            });
            ancestors.push(Over {
                version: first,
                count: index.count,
                over: None,
            });
        }
        Self::from_runs(lines, ancestors)
    }

    /// Align a save once using Git's histogram diff of `old`, the image this map
    /// describes, and `new`. `diff` runs it (both images as files) and is not
    /// called when `old` is empty, where every new line is an insert, the one
    /// hunk `git diff` gives, nor when the two are the same bytes. `next(n)`
    /// issues `n` fresh consecutive versions and returns the first. `self` is the
    /// same-tree head at close: R9 holds upstream takes while a writer is open,
    /// and R9.16 orders competing closes in the same tree.
    ///
    /// # Errors
    ///
    /// The images' read errors, the diff's, and [`Error::InvalidMap`] or
    /// [`Error::InvalidDiff`] where map, images and hunks do not agree.
    pub fn align(
        &self,
        old: &dyn Image,
        new: &dyn Image,
        diff: impl FnOnce() -> Result<Vec<Hunk>, Error>,
        mut next: impl FnMut(u64) -> V,
    ) -> Result<Self, Error> {
        self.validate()?;
        let old_index = LineIndex::of(old)?;
        self.check(old, &old_index)?;
        let new_index = LineIndex::of(new)?;
        let hunks = if old.is_empty() {
            if new_index.count == 0 {
                Vec::new()
            } else {
                vec![Hunk {
                    old_start: 0,
                    old_count: 0,
                    new_start: 0,
                    new_count: usize::try_from(new_index.count).map_err(|_| Error::InvalidDiff)?,
                }]
            }
        } else if same_image(old, new)? {
            Vec::new()
        } else {
            let hunks = diff()?;
            if hunks.is_empty() {
                return Err(Error::InvalidDiff);
            }
            hunks
        };
        let same = |o: u64, n: u64| -> Result<bool, Error> {
            let (a, la) = old_index.span(old, o)?;
            let (b, lb) = new_index.span(new, n)?;
            Ok(la == lb && same_bytes(old, a, new, b, la)?)
        };
        let mut edits = Vec::with_capacity(hunks.len());
        for h in hunks {
            let (mut os, mut ns) = (h.old_start as u64, h.new_start as u64);
            let (mut on, mut nn) = (h.old_count as u64, h.new_count as u64);
            if os.checked_add(on).is_none_or(|e| e > old_index.count)
                || ns.checked_add(nn).is_none_or(|e| e > new_index.count)
            {
                return Err(Error::InvalidDiff);
            }
            while on > 0 && nn > 0 && same(os, ns)? {
                os += 1;
                ns += 1;
                on -= 1;
                nn -= 1;
            }
            while on > 0 && nn > 0 && same(os + on - 1, ns + nn - 1)? {
                on -= 1;
                nn -= 1;
            }
            let replaced = (on > 0).then(|| next(on));
            let inserted = (nn > on).then(|| next(nn - on));
            edits.push(Edit {
                old_start: os,
                old_count: on,
                new_start: ns,
                new_count: nn,
                replaced,
                inserted,
            });
        }
        // The new image's unfinished last line: its length, for the version
        // that writes it.
        let open_last = if new_index.open {
            Some(new_index.span(new, new_index.count - 1)?.1)
        } else {
            None
        };
        let open_at = |n: u64| {
            if n + 1 == new_index.count {
                open_last
            } else {
                None
            }
        };
        let mut out = Runs(Vec::with_capacity(self.lines.len() + 2 * edits.len()));
        let mut ancestors = self.ancestors.clone();
        let mut cursor = Cursor {
            runs: &self.lines,
            at: 0,
            offset: 0,
            live: 0,
        };
        for e in &edits {
            let mut j = 0;
            while j < e.old_count {
                cursor.copy_until_live(e.old_start + j, &mut out);
                let part = cursor.take_live(e.old_count - j)?;
                let first = e.replaced.ok_or(Error::InvalidDiff)?.nth(j);
                ancestors.push(Over {
                    version: first,
                    count: part.count,
                    over: Some(part.version),
                });
                let live = e.new_count.saturating_sub(j).min(part.count);
                if live > 0 {
                    out.push(Run {
                        identity: part.identity,
                        version: first,
                        count: live,
                        dead: false,
                        open: open_at(e.new_start + j + live - 1),
                    });
                }
                out.push(Run {
                    identity: part.identity.nth(live),
                    version: first.nth(live),
                    count: part.count - live,
                    dead: true,
                    open: None,
                });
                j += part.count;
            }
            if let Some(first) = e.inserted {
                let count = e.new_count - e.old_count;
                cursor.copy_until_live(e.old_start + e.old_count, &mut out);
                ancestors.push(Over {
                    version: first,
                    count,
                    over: None,
                });
                out.push(Run {
                    identity: first,
                    version: first,
                    count,
                    dead: false,
                    open: open_at(e.new_start + e.new_count - 1),
                });
            }
        }
        cursor.copy_until_live(old_index.count, &mut out);
        let result = Self {
            lines: out.0,
            ancestors: canonical_overs(ancestors)?,
        };
        result.validate()?;
        result.check(new, &new_index)?;
        Ok(result)
    }

    /// [`LineMap::align`] of two images in memory, diffed by [`histogram`].
    ///
    /// # Errors
    ///
    /// As [`LineMap::align`].
    pub fn align_bytes(
        &self,
        old: &[u8],
        new: &[u8],
        next: impl FnMut(u64) -> V,
    ) -> Result<Self, Error> {
        self.align(&old, &new, || histogram(old, new), next)
    }
}

/// The byte at `at`, if the image has one.
fn read_byte(image: &dyn Image, at: u64) -> io::Result<Option<u8>> {
    let mut b = [0];
    Ok((image.read_at(&mut b, at)? == 1).then_some(b[0]))
}

/// Where a source's live runs are in its image: each with its first version,
/// its line count and its first byte, sorted by version.
struct Placed<V> {
    version: V,
    count: u64,
    start: u64,
    open: Option<u64>,
}

/// One part of an image: `len` bytes from `offset` of source `source`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub source: usize,
    pub offset: u64,
    pub len: u64,
}

/// Where each live line of `map` is in `sources` (each a map and the image it
/// describes), as parts of their images in file order: how a file made of
/// lines from other files (a take's) is written without holding its bytes. A
/// version's bytes are the same in every file that holds it live.
///
/// # Errors
///
/// [`Error::InvalidMap`] for a live version no source holds live, or one held
/// finished in one file and unfinished in the other; the images' read errors.
pub fn pieces<V: Id>(
    map: &LineMap<V>,
    sources: &[(&LineMap<V>, &dyn Image)],
) -> Result<Vec<Piece>, Error> {
    let mut placed: Vec<(usize, Placed<V>)> = Vec::new();
    for (s, (source, image)) in sources.iter().enumerate() {
        let mut at = 0;
        for run in source.runs().iter().filter(|r| !r.dead) {
            let finished = run.count - u64::from(run.open.is_some());
            let start = at;
            at = skip_lines(*image, at, finished)?;
            if let Some(len) = run.open {
                at += len;
            }
            placed.push((
                s,
                Placed {
                    version: run.version,
                    count: run.count,
                    start,
                    open: run.open,
                },
            ));
        }
    }
    placed.sort_by_key(|(_, p)| key(&p.version));
    let missing = Error::InvalidMap("a live line no source holds");
    let mut out: Vec<Piece> = Vec::new();
    for run in map.runs().iter().filter(|r| !r.dead) {
        let (mut v, mut left) = (run.version, run.count);
        while left > 0 {
            let i = placed.partition_point(|(_, p)| key(&p.version) <= key(&v));
            let (s, p) = i
                .checked_sub(1)
                .and_then(|i| placed.get(i))
                .ok_or(Error::InvalidMap("a live line no source holds"))?;
            let skip = v.index().wrapping_sub(p.version.index());
            if p.version.writer() != v.writer() || skip >= p.count {
                return Err(missing);
            }
            let n = (p.count - skip).min(left);
            let image = sources[*s].1;
            let start = skip_lines(image, p.start, skip)?;
            let takes_last = skip + n == p.count;
            let wants_open = left == n && run.open.is_some();
            if wants_open != (takes_last && p.open.is_some()) {
                return Err(Error::InvalidMap("a line finished in one file only"));
            }
            let end = if takes_last && let Some(len) = p.open {
                skip_lines(image, start, n - 1)? + len
            } else {
                skip_lines(image, start, n)?
            };
            let piece = Piece {
                source: *s,
                offset: start,
                len: end - start,
            };
            match out.last_mut() {
                Some(last) if last.source == piece.source && last.offset + last.len == start => {
                    last.len += piece.len;
                }
                _ => out.push(piece),
            }
            v = v.nth(n);
            left -= n;
        }
    }
    Ok(out)
}

/// The bytes `pieces` stand for, read from `sources`' images: for images in
/// memory and tests.
///
/// # Errors
///
/// The images' read errors.
pub fn render(sources: &[&dyn Image], pieces: &[Piece]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for piece in pieces {
        let start = out.len();
        out.resize(start + usize::try_from(piece.len).unwrap_or(usize::MAX), 0);
        let got = read_full(sources[piece.source], &mut out[start..], piece.offset)?;
        if got as u64 != piece.len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
    Ok(out)
}

/// Git's text-mode histogram diff of two images in memory, through the one call
/// the model shares. Against an empty old image every new line is an insert,
/// the one hunk `git diff` gives (`@@ -0,0 +1,n @@`): answered with no child, so
/// a new file's first save starts no `git diff`.
///
/// # Errors
///
/// The diff's.
pub fn histogram(old: &[u8], new: &[u8]) -> Result<Vec<Hunk>, Error> {
    if old.is_empty() {
        let count = LineIndex::of(&new)?.count();
        return Ok(if count == 0 {
            Vec::new()
        } else {
            vec![Hunk {
                old_start: 0,
                old_count: 0,
                new_start: 0,
                new_count: usize::try_from(count).map_err(|_| Error::InvalidDiff)?,
            }]
        });
    }
    Ok(agentfs_align::histogram_with(
        old,
        new,
        crate::git_cli::counted,
    )?)
}

/// Git's text-mode histogram diff of two files, through the one call the model
/// shares, read as it streams: what a save's alignment runs on images too large
/// to hold.
///
/// # Errors
///
/// The diff's.
pub fn histogram_files(old: &Path, new: &Path) -> Result<Vec<Hunk>, Error> {
    Ok(agentfs_align::histogram_files(
        old,
        new,
        crate::git_cli::counted,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A version for tests: writer and place.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
    struct V(u64, u64);

    impl fmt::Display for V {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}:{}", self.0, self.1)
        }
    }

    impl Id for V {
        fn at(writer: u64, index: u64) -> Self {
            Self(writer, index)
        }
        fn writer(&self) -> u64 {
            self.0
        }
        fn index(&self) -> u64 {
            self.1
        }
    }

    fn writer(w: u64) -> impl FnMut(u64) -> V {
        let mut last = 0;
        move |n| {
            let first = V(w, last + 1);
            last += n;
            first
        }
    }

    fn save(base: &LineMap<V>, old: &[u8], new: &[u8], w: u64) -> LineMap<V> {
        base.align_bytes(old, new, writer(w)).unwrap()
    }

    fn bytes(map: &LineMap<V>, image: &[u8]) -> Vec<u8> {
        let p = pieces(map, &[(map, &image)]).unwrap();
        render(&[&image], &p).unwrap()
    }

    #[test]
    fn a_new_file_of_any_size_is_one_run() {
        let mut image = Vec::new();
        for i in 0..10_000 {
            image.extend_from_slice(format!("line {i}\n").as_bytes());
        }
        image.extend_from_slice(b"tail");
        let map = save(&LineMap::default(), b"", &image, 1);
        assert_eq!(map.runs().len(), 1);
        assert_eq!(map.ancestors().len(), 1);
        assert_eq!(map.live_lines(), 10_001);
        assert_eq!(map.runs()[0].open, Some(4));
        assert_eq!(bytes(&map, &image), image);
    }

    #[test]
    fn an_edit_keeps_identities_and_deletes_in_place() {
        let base = save(&LineMap::default(), b"", b"a\nb\nc\nd\n", 1);
        let edited = save(&base, b"a\nb\nc\nd\n", b"a\nB\nd\ne\n", 2);
        let lines = edited.expand().lines;
        let ids: Vec<_> = lines
            .iter()
            .map(|l| (l.identity, l.version, l.live))
            .collect();
        assert_eq!(
            ids,
            vec![
                (V(1, 1), V(1, 1), true),
                (V(1, 2), V(2, 1), true),
                (V(1, 3), V(2, 2), false),
                (V(1, 4), V(1, 4), true),
                (V(2, 3), V(2, 3), true),
            ]
        );
        assert_eq!(edited.expand().ancestors[&V(2, 1)], Some(V(1, 2)));
        assert_eq!(bytes(&edited, b"a\nB\nd\ne\n"), b"a\nB\nd\ne\n");
        let again = LineMap::from_expanded(&edited.expand()).unwrap();
        assert_eq!(again, edited);
        assert_eq!(LineMap::decode(&edited.encode().unwrap()).unwrap(), edited);
    }

    #[test]
    fn an_append_joins_an_unfinished_line_and_adds_one_run() {
        let base = save(&LineMap::default(), b"", b"a\nb", 1);
        let appended = base.append(&&b"c\nd\ne"[..], writer(2)).unwrap();
        let lines = appended.expand().lines;
        assert_eq!(lines.len(), 4);
        assert_eq!((lines[1].identity, lines[1].version), (V(1, 2), V(2, 1)));
        assert_eq!(lines[3].open, Some(1));
        assert_eq!(bytes(&appended, b"a\nbc\nd\ne"), b"a\nbc\nd\ne");
        // The same lines as aligning the whole new image.
        let aligned = save(&base, b"a\nb", b"a\nbc\nd\ne", 2);
        assert_eq!(appended, aligned);
    }

    /// A join (R10.8) can leave an unfinished line before another: its length,
    /// not the next newline, ends it, in its own file and wherever it is taken.
    #[test]
    fn an_unfinished_line_mid_file_keeps_its_length() {
        let joined = LineMap::from_runs(
            vec![
                Run {
                    identity: V(1, 1),
                    version: V(1, 1),
                    count: 1,
                    dead: false,
                    open: Some(2),
                },
                Run {
                    identity: V(2, 1),
                    version: V(2, 1),
                    count: 1,
                    dead: false,
                    open: None,
                },
            ],
            vec![
                Over {
                    version: V(1, 1),
                    count: 1,
                    over: None,
                },
                Over {
                    version: V(2, 1),
                    count: 1,
                    over: None,
                },
            ],
        )
        .unwrap();
        let image: &[u8] = b"abc\n";
        let parts = pieces(&joined, &[(&joined, &image)]).unwrap();
        assert_eq!(
            parts,
            vec![Piece {
                source: 0,
                offset: 0,
                len: 4
            }]
        );
        let first = LineMap::from_runs(
            joined.runs()[..1].to_vec(),
            joined.ancestors()[..1].to_vec(),
        )
        .unwrap();
        assert_eq!(
            pieces(&first, &[(&joined, &image)]).unwrap(),
            vec![Piece {
                source: 0,
                offset: 0,
                len: 2
            }]
        );
    }

    #[test]
    fn a_bad_map_is_refused() {
        let map = save(&LineMap::default(), b"", b"a\nb\n", 1);
        assert!(matches!(
            map.align_bytes(b"a\n", b"x\n", writer(2)),
            Err(Error::InvalidMap(_))
        ));
        let mut runs = map.runs().to_vec();
        runs.push(runs[0]);
        assert!(LineMap::from_runs(runs, map.ancestors().to_vec()).is_err());
        assert!(LineMap::from_runs(map.runs().to_vec(), Vec::new()).is_err());
    }

    #[test]
    fn a_sampled_index_finds_every_line() {
        let mut image = Vec::new();
        for i in 0..1000 {
            image.extend(std::iter::repeat_n(b'x', i % 7));
            image.push(b'\n');
        }
        let index = LineIndex::of(&image).unwrap();
        let mut at = 0;
        for k in 0..1000 {
            let len = k % 7 + 1;
            assert_eq!(index.span(&image, k).unwrap(), (at, len));
            at += len;
        }
    }
}
