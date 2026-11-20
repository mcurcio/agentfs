//! The built log: the builder's own record of which steps are built and the
//! value each step's build gave its pointers (docs/trees.md, "History trails
//! the files"). Appended and fsynced only by the builder; the built point is
//! the last durable entry's `through`. A consumer keeping its own offset
//! (Kafka's consumer offsets), so the journal keeps one appender.
//!
//! ```text
//! frame = "AFB1" | u32le body_len | u32le crc32(body) | body (JSON BuiltEntry)
//! ```
//!
//! An entry is fsynced before anything it names is reachable, so a frame that
//! does not check is an append cut short: it and what follows are cut off.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use git2::Oid;
use serde::{Deserialize, Serialize};

use crate::tree_actor::journal::durable::RecordNo;

const MAGIC: &[u8; 4] = b"AFB1";
const HEADER: usize = 12;

/// One batch: every record through `through` is built, and each pointer a
/// step's record named as `Target::Step(step)` got `oid`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::tree_actor) struct BuiltEntry {
    pub through: RecordNo,
    pub values: Vec<BuiltValue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::tree_actor) struct BuiltValue {
    pub step: RecordNo,
    pub name: String,
    pub oid: String,
}

pub(in crate::tree_actor) struct BuiltLog {
    path: PathBuf,
    file: File,
    length: u64,
}

fn frame(entry: &BuiltEntry) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(entry)?;
    let len = u32::try_from(body.len()).map_err(|_| io::Error::from_raw_os_error(libc::EFBIG))?;
    let mut bytes = Vec::with_capacity(HEADER + body.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(value)
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("built log path has no parent"))?;
    File::open(parent)?.sync_all()
}

/// The entries at the start of `bytes` that check, and where they end.
fn parse(bytes: &[u8]) -> (Vec<BuiltEntry>, usize) {
    let mut entries = Vec::new();
    let mut at = 0;
    while bytes.len() >= at + HEADER && &bytes[at..at + 4] == MAGIC {
        let len = u32_at(bytes, at + 4) as usize;
        let end = at + HEADER + len;
        if bytes.len() < end {
            break;
        }
        let body = &bytes[at + HEADER..end];
        if crc32fast::hash(body) != u32_at(bytes, at + 8) {
            break;
        }
        match serde_json::from_slice(body) {
            Ok(entry) => entries.push(entry),
            Err(_) => break,
        }
        at = end;
    }
    (entries, at)
}

impl BuiltLog {
    /// Opens (or creates) the log and reads its entries; an unfinished append
    /// at the end is cut off.
    pub(in crate::tree_actor) fn open(path: &Path) -> io::Result<(Self, Vec<BuiltEntry>)> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        let bytes = fs::read(path)?;
        let (entries, end) = parse(&bytes);
        if end < bytes.len() {
            eprintln!("tree BuiltLogTail: {} bytes at {end}", bytes.len() - end);
            file.set_len(end as u64)?;
            file.sync_all()?;
        }
        sync_parent(path)?;
        Ok((
            Self {
                path: path.to_owned(),
                file,
                length: end as u64,
            },
            entries,
        ))
    }

    /// Appends one entry and fsyncs it: when this returns Ok, the entry stands
    /// against a power cut.
    pub(in crate::tree_actor) fn append(&mut self, entry: &BuiltEntry) -> io::Result<()> {
        let bytes = frame(entry)?;
        let written = self
            .file
            .write_all_at(&bytes, self.length)
            .and_then(|()| self.file.sync_data());
        if let Err(e) = written {
            // A cut-short frame fails its check at the next open.
            let _ = self.file.set_len(self.length);
            return Err(e);
        }
        self.length += bytes.len() as u64;
        Ok(())
    }

    /// Replaces the log with `keep`: a temporary file, fsynced, renamed, the
    /// directory fsynced. Called once a checkpoint covers what it drops.
    pub(in crate::tree_actor) fn rewrite(&mut self, keep: &[BuiltEntry]) -> io::Result<()> {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".tmp");
        let temp = PathBuf::from(name);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        let mut length = 0;
        for entry in keep {
            let bytes = frame(entry)?;
            file.write_all(&bytes)?;
            length += bytes.len() as u64;
        }
        file.sync_all()?;
        fs::rename(&temp, &self.path)?;
        sync_parent(&self.path)?;
        self.file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        self.length = length;
        Ok(())
    }
}

/// The built point the entries reach: the last one's `through`.
pub(in crate::tree_actor) fn built_through(entries: &[BuiltEntry]) -> RecordNo {
    entries.last().map_or(RecordNo::ZERO, |entry| entry.through)
}

/// Every resolved value, by step and pointer name.
pub(in crate::tree_actor) fn values(
    entries: &[BuiltEntry],
) -> io::Result<BTreeMap<(RecordNo, String), Oid>> {
    let mut values = BTreeMap::new();
    for entry in entries {
        for value in &entry.values {
            let oid = Oid::from_str(&value.oid).map_err(io::Error::other)?;
            values.insert((value.step, value.name.clone()), oid);
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(through: u64) -> BuiltEntry {
        BuiltEntry {
            through: RecordNo::new(through),
            values: vec![BuiltValue {
                step: RecordNo::new(through),
                name: "refs/heads/root".into(),
                oid: "0123456789012345678901234567890123456789".into(),
            }],
        }
    }

    #[test]
    fn entries_round_trip_and_a_cut_append_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("built.log");
        let (mut log, entries) = BuiltLog::open(&path).unwrap();
        assert!(entries.is_empty());
        log.append(&entry(3)).unwrap();
        log.append(&entry(7)).unwrap();
        drop(log);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"AFB1\x40\0\0\0cut").unwrap();
        drop(file);
        let (mut log, entries) = BuiltLog::open(&path).unwrap();
        assert_eq!(entries, vec![entry(3), entry(7)]);
        assert_eq!(built_through(&entries), RecordNo::new(7));
        assert_eq!(values(&entries).unwrap().len(), 2);
        log.rewrite(&[entry(7)]).unwrap();
        log.append(&entry(9)).unwrap();
        drop(log);
        let (_, entries) = BuiltLog::open(&path).unwrap();
        assert_eq!(entries, vec![entry(7), entry(9)]);
    }
}
