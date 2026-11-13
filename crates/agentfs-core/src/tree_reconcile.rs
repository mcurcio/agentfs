//! Byte-preserving line maps for tree landing (target architecture §7), generic
//! over the caller's version.
//!
//! The caller supplies volume-unique version IDs. Metadata must be persisted
//! with the corresponding content and all ancestors retained; a line's
//! identity is independent of a filesystem object's identity.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;

pub use agentfs_align::Hunk;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// A caller's version identifier: volume-unique, written once. A line's identity
/// is its first version.
pub trait Id: Clone + Ord + fmt::Debug + fmt::Display + Serialize + DeserializeOwned {}

impl<T: Clone + Ord + fmt::Debug + fmt::Display + Serialize + DeserializeOwned> Id for T {}

/// One current version of a line. A deleted line retains its identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line<V = String> {
    pub identity: V,
    pub version: V,
    pub bytes: Option<Vec<u8>>,
}

/// Stored line sequence and the versions each version was written over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(serialize = "V: Serialize", deserialize = "V: DeserializeOwned + Ord"))]
pub struct LineMap<V = String> {
    pub lines: Vec<Line<V>>,
    pub ancestors: BTreeMap<V, Option<V>>,
}

impl<V> Default for LineMap<V> {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            ancestors: BTreeMap::new(),
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

fn split(bytes: &[u8]) -> Vec<&[u8]> {
    bytes
        .split_inclusive(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .collect()
}

impl<V: Id> LineMap<V> {
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
    pub fn append(&self, suffix: &[u8], mut next_id: impl FnMut() -> V) -> Result<Self, Error> {
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
                return Err(Error::DuplicateId(id.to_string()));
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
                return Err(Error::DuplicateId(id.to_string()));
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
    pub fn align(&self, image: &[u8], mut next_id: impl FnMut() -> V) -> Result<Self, Error> {
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
        let mut replacement: Vec<Option<Line<V>>> =
            live.iter().map(|i| Some(self.lines[*i].clone())).collect();
        let mut before: Vec<Vec<Line<V>>> = vec![Vec::new(); old.len() + 1];
        let mut fresh = || -> Result<V, Error> {
            let id = next_id();
            if result.ancestors.contains_key(&id) {
                return Err(Error::DuplicateId(id.to_string()));
            }
            Ok(id)
        };
        // Build new records separately so every generated ID is checked.
        let mut additions: Vec<(V, Option<V>)> = Vec::new();
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
                    return Err(Error::DuplicateId(id.to_string()));
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
                    return Err(Error::DuplicateId(id.to_string()));
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

/// Git's text-mode histogram diff, through the one call the model shares.
pub fn histogram(old: &[u8], new: &[u8]) -> Result<Vec<Hunk>, Error> {
    agentfs_align::histogram_with(old, new, crate::git_cli::counted).map_err(|e| match e {
        agentfs_align::Error::Io(e) => Error::Io(e),
        agentfs_align::Error::Git(message) => Error::Git(message),
        agentfs_align::Error::Invalid => Error::InvalidDiff,
    })
}

/// The parent's decision for one incoming version. The caller supplies the
/// version graph, including ancestry obtained from the writer's read view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict<V = String> {
    /// The version can be applied (including agreeing deletes).
    Apply,
    /// The parent has this version or a later version.
    Drop,
    /// Concurrent versions: the parent wins.
    Lose { winner: V },
}

/// Compare versions of one identity. `parent` is absent for a new line.
/// Both input maps' ancestry must have been combined before calling this.
pub fn verdict<V: Id>(
    parent: Option<&Line<V>>,
    incoming: &Line<V>,
    ancestry: &BTreeMap<V, Option<V>>,
) -> Result<Verdict<V>, Error> {
    let Some(parent) = parent else {
        return Ok(Verdict::Apply);
    };
    if parent.identity != incoming.identity {
        return Err(Error::InvalidMap("different line identities"));
    }
    fn descends<V: Id>(
        start: &V,
        wanted: &V,
        graph: &BTreeMap<V, Option<V>>,
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
                .as_ref();
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
pub struct Collision<V = String> {
    pub identity: V,
    pub lost: V,
    pub winner: V,
    pub kind: CollisionKind,
}

/// The result of one whole save. A losing save leaves `content` exactly the
/// parent's map; the caller keeps the incoming commit reachable in history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landing<V = String> {
    pub content: LineMap<V>,
    pub collisions: Vec<Collision<V>>,
}

fn combined_graph<V: Id>(a: &LineMap<V>, b: &LineMap<V>) -> Result<BTreeMap<V, Option<V>>, Error> {
    let mut graph = a.ancestors.clone();
    for (version, over) in &b.ancestors {
        if graph.get(version).is_some_and(|existing| existing != over) {
            return Err(Error::InvalidMap("conflicting version ancestry"));
        }
        graph.insert(version.clone(), over.clone());
    }
    Ok(graph)
}

fn unfinished<V>(line: &Line<V>) -> bool {
    line.bytes.as_ref().is_some_and(|b| !b.ends_with(b"\n"))
}

fn parents_unfinished_tail<V>(parent: &LineMap<V>) -> Option<&Line<V>> {
    parent
        .lines
        .iter()
        .rev()
        .find(|line| line.bytes.is_some())
        .filter(|line| unfinished(line))
}

// R10.8: the first selected unfinished line with a live line after it, as its
// identity and the next live line's version.
fn unfinished_before_kept<V: Id>(placed: &[Line<V>], selected: &BTreeSet<V>) -> Option<(V, V)> {
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
fn place<V: Id>(parent: &[Line<V>], source: &[Line<V>], selected: &BTreeSet<V>) -> Vec<Line<V>> {
    let mut result = parent.to_vec();
    let source_ids: BTreeSet<&V> = source.iter().map(|l| &l.identity).collect();
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
        while pos < result.len() && !source_ids.contains(&result[pos].identity) {
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
pub fn land_save<V: Id>(
    parent: &LineMap<V>,
    base: &LineMap<V>,
    incoming: &LineMap<V>,
) -> Result<Landing<V>, Error> {
    let graph = combined_graph(parent, incoming)?;
    let base_versions: BTreeMap<&V, &V> = base
        .lines
        .iter()
        .map(|l| (&l.identity, &l.version))
        .collect();
    let current: BTreeMap<&V, &Line<V>> = parent.lines.iter().map(|l| (&l.identity, l)).collect();
    let changed: Vec<&Line<V>> = incoming
        .lines
        .iter()
        .filter(|line| base_versions.get(&line.identity) != Some(&&line.version))
        .collect();
    let mut decisions = BTreeMap::new();
    for line in &changed {
        decisions.insert(
            line.identity.clone(),
            verdict(current.get(&line.identity).copied(), line, &graph)?,
        );
    }
    let selected: BTreeSet<V> = decisions
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
    let applying: BTreeSet<V> = selected
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
pub fn sync<V: Id>(
    parent: &LineMap<V>,
    sandbox: &LineMap<V>,
    undecided: &BTreeSet<V>,
) -> Result<LineMap<V>, Error> {
    let graph = combined_graph(parent, sandbox)?;
    let selected: BTreeSet<V> = sandbox
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
