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
