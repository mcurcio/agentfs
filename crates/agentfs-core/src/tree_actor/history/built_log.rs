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
//!
//! The log lives on disk only: a start reads it once, and a checkpoint's
//! rewrite reads it again ([`BuiltLog::entries_after`]); nothing keeps its
//! entries in memory.

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
    /// Stored as its hex text, as every entry before it was.
    #[serde(with = "oid_hex")]
    pub oid: Oid,
}

/// An object id as its 40-digit hex text.
mod oid_hex {
    use git2::Oid;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub(super) fn serialize<S: Serializer>(oid: &Oid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(oid)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Oid, D::Error> {
        let text = String::deserialize(deserializer)?;
        Oid::from_str(&text).map_err(D::Error::custom)
    }
}

pub(in crate::tree_actor) struct BuiltLog {
    path: PathBuf,
    file: File,
    length: u64,
    /// The first entry's `through`: whether a checkpoint lets the log drop
    /// any, without reading it.
    first: Option<RecordNo>,
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
                first: entries.first().map(|entry| entry.through),
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
        self.first.get_or_insert(entry.through);
        Ok(())
    }

    /// The first entry's `through`, if the log holds one.
    pub(in crate::tree_actor) fn first_through(&self) -> Option<RecordNo> {
        self.first
    }

    /// The entries after a checkpoint whose redo starts at `checkpointed`,
    /// read from the file: what a rewrite keeps.
    pub(in crate::tree_actor) fn entries_after(
        &self,
        checkpointed: RecordNo,
    ) -> io::Result<Vec<BuiltEntry>> {
        let mut bytes = vec![0; usize::try_from(self.length).map_err(io::Error::other)?];
        self.file.read_exact_at(&mut bytes, 0)?;
        let (mut entries, _) = parse(&bytes);
        entries.retain(|entry| entry.through > checkpointed);
        Ok(entries)
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
        self.first = keep.first().map(|entry| entry.through);
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
            values.insert((value.step, value.name.clone()), value.oid);
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
                oid: Oid::from_str("0123456789012345678901234567890123456789").unwrap(),
            }],
        }
    }

    #[test]
    fn an_entry_is_stored_as_its_pinned_line() {
        // The bytes every built.log already on disk holds: an object id as
        // its hex text.
        let line = r#"{"through":3,"values":[{"step":3,"name":"refs/heads/root","oid":"0123456789012345678901234567890123456789"}]}"#;
        assert_eq!(serde_json::to_string(&entry(3)).unwrap(), line);
        assert_eq!(serde_json::from_str::<BuiltEntry>(line).unwrap(), entry(3));
        assert!(serde_json::from_str::<BuiltEntry>(&line.replace("0123", "zz23")).is_err());
    }

    #[test]
    fn a_rewrite_keeps_the_entries_after_the_checkpoint_read_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("built.log");
        let (mut log, _) = BuiltLog::open(&path).unwrap();
        assert_eq!(log.first_through(), None);
        for through in [3, 7, 9] {
            log.append(&entry(through)).unwrap();
        }
        assert_eq!(log.first_through(), Some(RecordNo::new(3)));
        let keep = log.entries_after(RecordNo::new(7)).unwrap();
        assert_eq!(keep, vec![entry(9)]);
        log.rewrite(&keep).unwrap();
        assert_eq!(log.first_through(), Some(RecordNo::new(9)));
        log.append(&entry(11)).unwrap();
        assert_eq!(
            log.entries_after(RecordNo::ZERO).unwrap(),
            vec![entry(9), entry(11)]
        );
        drop(log);
        let (log, entries) = BuiltLog::open(&path).unwrap();
        assert_eq!(entries, vec![entry(9), entry(11)]);
        assert_eq!(log.first_through(), Some(RecordNo::new(9)));
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
