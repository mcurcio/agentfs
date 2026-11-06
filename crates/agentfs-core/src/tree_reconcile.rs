//! Byte-preserving line maps for tree landing (target architecture §7).
//!
//! The caller supplies volume-unique version IDs. Metadata must be persisted
//! with the corresponding content and all ancestors retained; a line's
//! identity is independent of a filesystem object's identity.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// An opaque, volume-unique line or version identifier.
pub type Id = String;

/// One current version of a line. A deleted line retains its identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub identity: Id,
    pub version: Id,
    pub bytes: Option<Vec<u8>>,
}

/// Stored line sequence and the versions each version was written over.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineMap {
    pub lines: Vec<Line>,
    pub ancestors: BTreeMap<Id, Option<Id>>,
}

/// A changed region of the histogram diff, with zero-based ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
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
    DuplicateId(Id),
    #[error("invalid line map: {0}")]
    InvalidMap(&'static str),
}

fn split(bytes: &[u8]) -> Vec<&[u8]> {
    bytes
        .split_inclusive(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .collect()
}

impl LineMap {
    /// Render exactly the bytes stored in live records, without adding delimiters.
    pub fn bytes(&self) -> Vec<u8> {
        self.lines
            .iter()
            .filter_map(|l| l.bytes.as_ref())
            .flatten()
            .copied()
            .collect()
    }

    /// Serialize metadata for storage alongside the file in the same commit.
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Decode metadata. Callers should validate it before use.
    pub fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Check identities, ancestry, and that records match a given file image.
    pub fn validate(&self, image: &[u8]) -> Result<(), Error> {
        let mut identities = BTreeSet::new();
        let mut versions = BTreeSet::new();
        for line in &self.lines {
            if !identities.insert(&line.identity) || !versions.insert(&line.version) {
                return Err(Error::InvalidMap("duplicate line or version"));
            }
            if !self.ancestors.contains_key(&line.version) {
                return Err(Error::InvalidMap("missing version ancestry"));
            }
        }
        let mut complete = BTreeSet::new();
        for version in self.ancestors.keys() {
            let mut seen = BTreeSet::new();
            let mut next = Some(version);
            while let Some(id) = next {
                if complete.contains(id) {
                    break;
                }
                if !seen.insert(id) {
                    return Err(Error::InvalidMap("cyclic ancestry"));
                }
                next = self
                    .ancestors
                    .get(id)
                    .ok_or(Error::InvalidMap("missing ancestor"))?
                    .as_ref();
            }
            complete.extend(seen);
        }
        if self.bytes() != image {
            return Err(Error::InvalidMap("content differs from line map"));
        }
        Ok(())
    }

    /// Apply append-only calls without realigning existing lines.
    pub fn append(&self, suffix: &[u8], mut next_id: impl FnMut() -> Id) -> Result<Self, Error> {
        let mut result = self.clone();
        let mut rest = suffix;
        if let Some(line) = result
            .lines
            .iter_mut()
            .rev()
            .find(|line| line.bytes.is_some())
            && line
                .bytes
                .as_ref()
                .is_some_and(|bytes| !bytes.ends_with(b"\n"))
            && !rest.is_empty()
        {
            let end = rest
                .iter()
                .position(|b| *b == b'\n')
                .map_or(rest.len(), |n| n + 1);
            let id = next_id();
            if result.ancestors.contains_key(&id) {
                return Err(Error::DuplicateId(id));
            }
            result
                .ancestors
                .insert(id.clone(), Some(line.version.clone()));
            line.version = id;
            if let Some(bytes) = &mut line.bytes {
                bytes.extend_from_slice(&rest[..end]);
            }
            rest = &rest[end..];
        }
        for bytes in split(rest) {
            let id = next_id();
            if result.ancestors.contains_key(&id) {
                return Err(Error::DuplicateId(id));
            }
            result.ancestors.insert(id.clone(), None);
            result.lines.push(Line {
                identity: id.clone(),
                version: id,
                bytes: Some(bytes.to_vec()),
            });
        }
        Ok(result)
    }

    /// Align a save once using Git's histogram diff. `next_id` must issue a
    /// fresh ID each time across the volume. `self` is the same-tree head at
    /// close: R9 holds upstream takes while a writer is open, and R9.16 orders
    /// competing closes in the same tree.
    pub fn align(&self, image: &[u8], mut next_id: impl FnMut() -> Id) -> Result<Self, Error> {
        let old_image = self.bytes();
        self.validate(&old_image)?;
        let old = split(&old_image);
        let new = split(image);
        let hunks = histogram(&old_image, image)?;
        let live: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.bytes.as_ref().map(|_| i))
            .collect();
        if live.len() != old.len() {
            return Err(Error::InvalidMap(
                "live records do not correspond to byte lines",
            ));
        }
        let mut result = self.clone();
        let mut replacement: Vec<Option<Line>> =
            live.iter().map(|i| Some(self.lines[*i].clone())).collect();
        let mut before: Vec<Vec<Line>> = vec![Vec::new(); old.len() + 1];
        let mut fresh = || -> Result<Id, Error> {
            let id = next_id();
            if result.ancestors.contains_key(&id) {
                return Err(Error::DuplicateId(id));
            }
            Ok(id)
        };
        // Build new records separately so every generated ID is checked.
        let mut additions: Vec<(Id, Option<Id>)> = Vec::new();
        let mut allocated = BTreeSet::new();
        for h in hunks {
            let (mut os, mut ns, mut on, mut nn) =
                (h.old_start, h.new_start, h.old_count, h.new_count);
            if os.checked_add(on).is_none_or(|e| e > old.len())
                || ns.checked_add(nn).is_none_or(|e| e > new.len())
            {
                return Err(Error::InvalidDiff);
            }
            while on > 0 && nn > 0 && old[os] == new[ns] {
                os += 1;
                ns += 1;
                on -= 1;
                nn -= 1;
            }
            while on > 0 && nn > 0 && old[os + on - 1] == new[ns + nn - 1] {
                on -= 1;
                nn -= 1;
            }
            for j in 0..on {
                let prior = &self.lines[live[os + j]];
                let id = fresh()?;
                if !allocated.insert(id.clone()) {
                    return Err(Error::DuplicateId(id));
                }
                additions.push((id.clone(), Some(prior.version.clone())));
                replacement[os + j] = Some(Line {
                    identity: prior.identity.clone(),
                    version: id,
                    bytes: (j < nn).then(|| new[ns + j].to_vec()),
                });
            }
            for bytes in &new[ns + on.min(nn)..ns + nn] {
                let id = fresh()?;
                if !allocated.insert(id.clone()) {
                    return Err(Error::DuplicateId(id));
                }
                additions.push((id.clone(), None));
                before[os + on].push(Line {
                    identity: id.clone(),
                    version: id,
                    bytes: Some(bytes.to_vec()),
                });
            }
        }
        let mut lines = Vec::new();
        let mut k = 0;
        for line in &self.lines {
            if line.bytes.is_some() {
                lines.append(&mut before[k]);
                if let Some(line) = replacement[k].take() {
                    lines.push(line);
                }
                k += 1;
            } else {
                lines.push(line.clone());
            }
        }
        lines.append(&mut before[k]);
        result.lines = lines;
        result.ancestors.extend(additions);
        result.validate(image)?;
        Ok(result)
    }
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Git's text-mode histogram diff, including NUL-containing input.
pub fn histogram(old: &[u8], new: &[u8]) -> Result<Vec<Hunk>, Error> {
    if old == new {
        return Ok(Vec::new());
    }
    let dir = loop {
        let path = std::env::temp_dir().join(format!(
            "agentfs-diff-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => break TempDir(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    let a = dir.0.join("old");
    let b = dir.0.join("new");
    std::fs::write(&a, old)?;
    std::fs::write(&b, new)?;
    // Neither the caller's directory nor its git configuration may reach the
    // alignment: a directory inside a broken worktree stops `git diff`, and a
    // diff setting would change which lines keep their identity.
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", crate::git_cli::GIT_PATH)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(&dir.0)
        .args([
            "diff",
            "--no-index",
            "--no-color",
            "--no-ext-diff",
            "--histogram",
            "--text",
            "--unified=0",
            "--",
        ])
        .arg(a)
        .arg(b)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = crate::git_cli::counted(&mut command)?.wait_with_output()?;
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Err(Error::Git(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    let mut hunks = Vec::new();
    for line in out.stdout.split(|b| *b == b'\n') {
        if !line.starts_with(b"@@ -") {
            continue;
        }
        let header = std::str::from_utf8(line).map_err(|_| Error::InvalidDiff)?;
        let (old, rest) = header[4..].split_once(" +").ok_or(Error::InvalidDiff)?;
        let new = rest.split_once(" @@").ok_or(Error::InvalidDiff)?.0;
        let parse = |range: &str| -> Result<(usize, usize), Error> {
            let (start, count) = range.split_once(',').unwrap_or((range, "1"));
            let start: usize = start.parse().map_err(|_| Error::InvalidDiff)?;
            let count: usize = count.parse().map_err(|_| Error::InvalidDiff)?;
            Ok((
                if count == 0 {
                    start
                } else {
                    start.checked_sub(1).ok_or(Error::InvalidDiff)?
                },
                count,
            ))
        };
        let (old_start, old_count) = parse(old)?;
        let (new_start, new_count) = parse(new)?;
        hunks.push(Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
        });
    }
    if hunks.is_empty() {
        return Err(Error::InvalidDiff);
    }
    Ok(hunks)
}

/// The parent's decision for one incoming version. The caller supplies the
/// version graph, including ancestry obtained from the writer's read view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The version can be applied (including agreeing deletes).
    Apply,
    /// The parent has this version or a later version.
    Drop,
    /// Concurrent versions: the parent wins.
    Lose { winner: Id },
}

/// Compare versions of one identity. `parent` is absent for a new line.
/// Both input maps' ancestry must have been combined before calling this.
pub fn verdict(
    parent: Option<&Line>,
    incoming: &Line,
    ancestry: &BTreeMap<Id, Option<Id>>,
) -> Result<Verdict, Error> {
    let Some(parent) = parent else {
        return Ok(Verdict::Apply);
    };
    if parent.identity != incoming.identity {
        return Err(Error::InvalidMap("different line identities"));
    }
    fn descends(
        start: &str,
        wanted: &str,
        graph: &BTreeMap<Id, Option<Id>>,
    ) -> Result<bool, Error> {
        let mut next = Some(start);
        let mut seen = BTreeSet::new();
        while let Some(id) = next {
            if id == wanted {
                return Ok(true);
            }
            if !seen.insert(id) {
                return Err(Error::InvalidMap("cyclic ancestry"));
            }
            next = graph
                .get(id)
                .ok_or(Error::InvalidMap("missing ancestor"))?
                .as_deref();
        }
        Ok(false)
    }
    if parent.version == incoming.version {
        return Ok(Verdict::Drop);
    }
    if descends(&incoming.version, &parent.version, ancestry)? {
        return Ok(Verdict::Apply);
    }
    if descends(&parent.version, &incoming.version, ancestry)? {
        return Ok(Verdict::Drop);
    }
    if parent.bytes.is_none() && incoming.bytes.is_none() {
        return Ok(Verdict::Apply);
    }
    Ok(Verdict::Lose {
        winner: parent.version.clone(),
    })
}

/// Why a version was lost when a whole save was judged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CollisionKind {
    /// This version collided with the parent's version.
    Line,
    /// This version would have landed, but another version lost the save.
    Whole,
}

/// A lost version and the parent version that decided against it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collision {
    pub identity: Id,
    pub lost: Id,
    pub winner: Id,
    pub kind: CollisionKind,
}

/// The result of one whole save. A losing save leaves `content` exactly the
/// parent's map; the caller keeps the incoming commit reachable in history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landing {
    pub content: LineMap,
    pub collisions: Vec<Collision>,
}

fn combined_graph(a: &LineMap, b: &LineMap) -> Result<BTreeMap<Id, Option<Id>>, Error> {
    let mut graph = a.ancestors.clone();
    for (version, over) in &b.ancestors {
        if graph.get(version).is_some_and(|existing| existing != over) {
            return Err(Error::InvalidMap("conflicting version ancestry"));
        }
        graph.insert(version.clone(), over.clone());
    }
    Ok(graph)
}

fn unfinished(line: &Line) -> bool {
    line.bytes.as_ref().is_some_and(|b| !b.ends_with(b"\n"))
}

fn parents_unfinished_tail(parent: &LineMap) -> Option<&Line> {
    parent
        .lines
        .iter()
        .rev()
        .find(|line| line.bytes.is_some())
        .filter(|line| unfinished(line))
}

// R10.8: the first selected unfinished line with a live line after it, as its
// identity and the next live line's version.
fn unfinished_before_kept(placed: &[Line], selected: &BTreeSet<Id>) -> Option<(Id, Id)> {
    let mut live = placed.iter().filter(|line| line.bytes.is_some()).peekable();
    while let Some(line) = live.next() {
        if unfinished(line)
            && selected.contains(&line.identity)
            && let Some(next) = live.peek()
        {
            return Some((line.identity.clone(), next.version.clone()));
        }
    }
    None
}

// Replace existing identities in place; insert new identities after their
// nearest predecessor in the source, past parent-only inserts at that point.
fn place(parent: &[Line], source: &[Line], selected: &BTreeSet<Id>) -> Vec<Line> {
    let mut result = parent.to_vec();
    let source_ids: BTreeSet<&str> = source.iter().map(|l| l.identity.as_str()).collect();
    for (index, line) in source.iter().enumerate() {
        if !selected.contains(&line.identity) {
            continue;
        }
        if let Some(pos) = result.iter().position(|p| p.identity == line.identity) {
            result[pos] = line.clone();
            continue;
        }
        let anchor = source[..index]
            .iter()
            .rev()
            .find_map(|p| result.iter().position(|r| r.identity == p.identity));
        let mut pos = anchor.map_or(0, |p| p + 1);
        while pos < result.len() && !source_ids.contains(result[pos].identity.as_str()) {
            pos += 1;
        }
        result.insert(pos, line.clone());
    }
    result
}

/// Judge a single file save whole at the parent's queue.
///
/// `base` is the file map at the save's first parent, `incoming` is that
/// save's map, and `parent` is the current parent map. Each changed version
/// in the incoming map is judged; one loss rejects every applicable version.
/// The actor must also account for file/name collisions, preserve the losing
/// commit, and commit the result with both histories as parents. An incoming
/// unfinished line placed in front of a line the parent keeps loses to that
/// line (R10.8), so no result holds bytes neither writer wrote.
pub fn land_save(parent: &LineMap, base: &LineMap, incoming: &LineMap) -> Result<Landing, Error> {
    let graph = combined_graph(parent, incoming)?;
    let base_versions: BTreeMap<&str, &str> = base
        .lines
        .iter()
        .map(|l| (l.identity.as_str(), l.version.as_str()))
        .collect();
    let current: BTreeMap<&str, &Line> = parent
        .lines
        .iter()
        .map(|l| (l.identity.as_str(), l))
        .collect();
    let changed: Vec<&Line> = incoming
        .lines
        .iter()
        .filter(|line| base_versions.get(line.identity.as_str()) != Some(&line.version.as_str()))
        .collect();
    let mut decisions = BTreeMap::new();
    for line in &changed {
        decisions.insert(
            line.identity.clone(),
            verdict(current.get(line.identity.as_str()).copied(), line, &graph)?,
        );
    }
    let selected: BTreeSet<Id> = decisions
        .iter()
        .filter_map(|(id, verdict)| (*verdict == Verdict::Apply).then_some(id.clone()))
        .collect();
    let placed = place(&parent.lines, &incoming.lines, &selected);
    // R10.4: an incoming line placed after a parent's unfinished last line
    // loses to that line. An edited existing line stays in its existing place.
    if let Some(partial) = parents_unfinished_tail(parent)
        && let Some(pos) = placed
            .iter()
            .position(|line| line.version == partial.version)
    {
        for line in placed[pos + 1..]
            .iter()
            .filter(|line| line.bytes.is_some() && selected.contains(&line.identity))
        {
            decisions.insert(
                line.identity.clone(),
                Verdict::Lose {
                    winner: partial.version.clone(),
                },
            );
        }
    }
    // R10.8: judged on what would still apply after R10.4.
    let applying: BTreeSet<Id> = selected
        .iter()
        .filter(|id| decisions[*id] == Verdict::Apply)
        .cloned()
        .collect();
    let kept = place(&parent.lines, &incoming.lines, &applying);
    if let Some((identity, winner)) = unfinished_before_kept(&kept, &applying) {
        decisions.insert(identity, Verdict::Lose { winner });
    }
    let decider = changed
        .iter()
        .find_map(|line| match &decisions[&line.identity] {
            Verdict::Lose { winner } => Some(winner.clone()),
            _ => None,
        });
    if let Some(winner) = decider {
        let collisions = changed
            .iter()
            .filter_map(|line| match &decisions[&line.identity] {
                Verdict::Drop => None,
                Verdict::Apply => Some(Collision {
                    identity: line.identity.clone(),
                    lost: line.version.clone(),
                    winner: winner.clone(),
                    kind: CollisionKind::Whole,
                }),
                Verdict::Lose { winner } => Some(Collision {
                    identity: line.identity.clone(),
                    lost: line.version.clone(),
                    winner: winner.clone(),
                    kind: CollisionKind::Line,
                }),
            })
            .collect();
        return Ok(Landing {
            content: parent.clone(),
            collisions,
        });
    }
    Ok(Landing {
        content: LineMap {
            lines: placed,
            ancestors: graph,
        },
        collisions: Vec::new(),
    })
}

/// Sync the current parent with exactly the sandbox versions still awaiting a
/// decision. `undecided` contains version IDs, determined by commit ancestry;
/// this function makes no collision decision. The actor must only sync a file
/// when its writer-lock protocol permits it.
pub fn sync(
    parent: &LineMap,
    sandbox: &LineMap,
    undecided: &BTreeSet<Id>,
) -> Result<LineMap, Error> {
    let graph = combined_graph(parent, sandbox)?;
    let selected: BTreeSet<Id> = sandbox
        .lines
        .iter()
        .filter(|line| undecided.contains(&line.version))
        .map(|line| line.identity.clone())
        .collect();
    let mut lines = place(&parent.lines, &sandbox.lines, &selected);
    // R10.6: keep the undecided sandbox line visible without inventing a
    // newline after an unseen unfinished parent tail.
    if let Some(partial) = parents_unfinished_tail(parent)
        && let Some(pos) = lines
            .iter()
            .position(|line| line.version == partial.version)
        && lines[pos + 1..]
            .iter()
            .any(|line| line.bytes.is_some() && selected.contains(&line.identity))
    {
        lines.remove(pos);
    }
    // R10.9, the mirror: keep an undecided unfinished line visible without
    // joining the parent's later lines to it. They arrive with the decision.
    if let Some(pos) = lines
        .iter()
        .position(|line| selected.contains(&line.identity) && unfinished(line))
    {
        let later = lines.split_off(pos + 1);
        lines.extend(
            later
                .into_iter()
                .filter(|line| line.bytes.is_none() || selected.contains(&line.identity)),
        );
    }
    Ok(LineMap {
        lines,
        ancestors: graph,
    })
}
