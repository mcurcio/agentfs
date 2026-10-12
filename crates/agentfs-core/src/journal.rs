//! The durable change log (design §3.2 steps 2–3, §3.3).
//!
//! Each record is framed as `magic | length | crc32 | JSON payload`. A torn or
//! corrupt frame ends the log: its call never returned, so nothing was
//! promised about it, and everything after it is discarded on open.
//!
//! [`Journal::record`] uses a group commit. The first caller to find no
//! flush in progress writes every queued record and `fsync`s once; callers
//! that arrive meanwhile queue behind it and are covered by the next flush.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::record::{Draft, Record};

const MAGIC: [u8; 4] = *b"AFJ1";
const HEADER_LEN: usize = 12;
/// Upper bound on one payload, so a corrupt length cannot cause a huge read.
const MAX_PAYLOAD: usize = 64 << 20;

/// Runs before each journal `fsync`, so content the new records refer to
/// (frozen copies) is durable first.
pub type SyncHook = Box<dyn Fn() -> io::Result<()> + Send + Sync>;

/// What opening the journal found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// Valid records read back.
    pub records: usize,
    /// Bytes discarded from a torn or corrupt tail.
    pub discarded_bytes: u64,
}

pub struct Journal {
    state: Mutex<State>,
    flushed: Condvar,
    path: PathBuf,
    before_sync: SyncHook,
}

struct State {
    /// Taken by whichever thread is writing.
    file: Option<File>,
    next_seq: u64,
    /// Assigned a sequence number, not yet written.
    queue: Vec<Record>,
    flushing: bool,
    /// Every sequence number up to this one has been written (or failed).
    processed_through: Option<u64>,
    /// Sequence numbers whose write failed, with the error kind.
    failed: Vec<(u64, io::ErrorKind)>,
    /// Durable, not yet committed to history. In sequence order.
    uncommitted: Vec<Record>,
}

impl Journal {
    /// Opens the journal at `path`, creating it if needed.
    ///
    /// Records found in the file become [`Journal::uncommitted`]. New
    /// sequence numbers start after both the last record found and
    /// `min_next_seq`, which callers set past the history's last committed
    /// sequence, since committed records are truncated away.
    pub fn open(
        path: impl Into<PathBuf>,
        min_next_seq: u64,
        before_sync: SyncHook,
    ) -> io::Result<(Self, OpenReport)> {
        let path = path.into();
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let (records, valid_len) = decode_all(&bytes);
        let discarded = bytes.len() - valid_len;
        if discarded > 0 {
            file.set_len(valid_len as u64)?;
            file.sync_all()?;
        }
        sync_parent(&path)?;
        let next_seq = records
            .last()
            .map_or(min_next_seq, |r| (r.seq + 1).max(min_next_seq));
        let report = OpenReport {
            records: records.len(),
            discarded_bytes: discarded as u64,
        };
        let journal = Self {
            state: Mutex::new(State {
                file: Some(file),
                next_seq,
                queue: Vec::new(),
                flushing: false,
                processed_through: next_seq.checked_sub(1),
                failed: Vec::new(),
                uncommitted: records,
            }),
            flushed: Condvar::new(),
            path,
            before_sync,
        };
        Ok((journal, report))
    }

    /// Durably records `draft`, returning its sequence number once it is on
    /// disk.
    pub fn record(&self, draft: Draft) -> io::Result<u64> {
        let mut st = self.lock();
        let seq = st.next_seq;
        st.next_seq += 1;
        st.queue.push(Record { seq, draft });
        loop {
            if let Some(i) = st.failed.iter().position(|(s, _)| *s == seq) {
                let (_, kind) = st.failed.swap_remove(i);
                return Err(io::Error::new(kind, "journal write failed"));
            }
            if st.processed_through.is_some_and(|p| p >= seq) {
                return Ok(seq);
            }
            if st.flushing {
                st = self.flushed.wait(st).expect("journal lock poisoned");
                continue;
            }
            st = self.flush_queue(st);
        }
    }

    /// Writes everything queued as one batch. Called with the lock held and
    /// no flush in progress; returns with the lock held again.
    fn flush_queue<'a>(&'a self, mut st: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        st.flushing = true;
        let batch = std::mem::take(&mut st.queue);
        let mut file = st
            .file
            .take()
            .expect("journal file present when not flushing");
        drop(st);

        let result = self.write_batch(&mut file, &batch);

        let mut st = self.lock();
        st.file = Some(file);
        let last = batch.last().map(|r| r.seq);
        match result {
            Ok(()) => st.uncommitted.extend(batch),
            Err(e) => st.failed.extend(batch.iter().map(|r| (r.seq, e.kind()))),
        }
        if last.is_some() {
            st.processed_through = last;
        }
        st.flushing = false;
        self.flushed.notify_all();
        st
    }

    fn write_batch(&self, file: &mut File, batch: &[Record]) -> io::Result<()> {
        let mut buf = Vec::new();
        for record in batch {
            encode(record, &mut buf)?;
        }
        let before = file.metadata()?.len();
        let written = (self.before_sync)()
            .and_then(|()| file.write_all(&buf))
            .and_then(|()| file.sync_data());
        if written.is_err() {
            // Leave no partial frame for the next batch to follow.
            let _ = file.set_len(before);
        }
        written
    }

    /// How many durable records are not yet committed to history, without
    /// copying them. Cheap enough for a metric callback (design §13); a
    /// poisoned lock still reports, since the count is only read.
    pub fn backlog(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .uncommitted
            .len()
    }

    /// The last sequence number handed out, whether its record is durable
    /// yet or not; `None` before the first.
    pub fn last_seq(&self) -> Option<u64> {
        self.lock().next_seq.checked_sub(1)
    }

    /// The lowest sequence number durable but not yet in history.
    pub fn oldest_uncommitted(&self) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .uncommitted
            .first()
            .map(|r| r.seq)
    }

    /// Durable records not yet committed to history, in sequence order.
    pub fn uncommitted(&self) -> Vec<Record> {
        self.lock().uncommitted.clone()
    }

    /// Drops every record up to and including `seq`, once history holds them.
    pub fn truncate_through(&self, seq: u64) -> io::Result<()> {
        let mut st = self.lock();
        while st.flushing {
            st = self.flushed.wait(st).expect("journal lock poisoned");
        }
        if st.uncommitted.first().is_none_or(|r| r.seq > seq) {
            return Ok(());
        }
        st.flushing = true;
        let remaining: Vec<Record> = st
            .uncommitted
            .iter()
            .filter(|r| r.seq > seq)
            .cloned()
            .collect();
        let old = st
            .file
            .take()
            .expect("journal file present when not flushing");
        drop(st);

        let result = rewrite(&self.path, &remaining);

        let mut st = self.lock();
        let outcome = match result {
            Ok(file) => {
                st.file = Some(file);
                st.uncommitted = remaining;
                Ok(())
            }
            Err(e) => {
                st.file = Some(old);
                Err(e)
            }
        };
        st.flushing = false;
        self.flushed.notify_all();
        outcome
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("journal lock poisoned")
    }
}

fn encode(record: &Record, out: &mut Vec<u8>) -> io::Result<()> {
    let payload = serde_json::to_vec(record).map_err(io::Error::other)?;
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|l| (*l as usize) <= MAX_PAYLOAD)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal record too large"))?;
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(())
}

/// Reads the journal at `path` without changing it: the valid records, and
/// how many bytes follow the last valid frame. Unlike [`Journal::open`] a
/// torn tail is left in place, so this is safe while another process is
/// appending (design §2.3). A missing file reads as empty.
pub fn read_records(path: &Path) -> io::Result<(Vec<Record>, u64)> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e),
    };
    let (records, valid) = decode_all(&bytes);
    Ok((records, (bytes.len() - valid) as u64))
}

/// Decodes frames until the first invalid one. Returns the records and the
/// length of the valid prefix.
fn decode_all(bytes: &[u8]) -> (Vec<Record>, usize) {
    let mut records = Vec::new();
    let mut offset = 0;
    while let Some((record, next)) = decode_one(bytes, offset) {
        // Sequence numbers only grow; anything else is corruption.
        if records.last().is_some_and(|r: &Record| r.seq >= record.seq) {
            break;
        }
        records.push(record);
        offset = next;
    }
    (records, offset)
}

fn decode_one(bytes: &[u8], offset: usize) -> Option<(Record, usize)> {
    let header = bytes.get(offset..offset + HEADER_LEN)?;
    if header[..4] != MAGIC {
        return None;
    }
    let len = u32::from_le_bytes(header[4..8].try_into().ok()?) as usize;
    let crc = u32::from_le_bytes(header[8..12].try_into().ok()?);
    if len > MAX_PAYLOAD {
        return None;
    }
    let start = offset + HEADER_LEN;
    let payload = bytes.get(start..start + len)?;
    if crc32fast::hash(payload) != crc {
        return None;
    }
    let record = serde_json::from_slice(payload).ok()?;
    Some((record, start + len))
}

/// Atomically replaces the journal with `records`, returning the new file
/// opened for appending.
fn rewrite(path: &Path, records: &[Record]) -> io::Result<File> {
    let mut buf = Vec::new();
    for record in records {
        encode(record, &mut buf)?;
    }
    let tmp = path.with_extension("rewrite");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&buf)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    sync_parent(path)?;
    OpenOptions::new().read(true).append(true).open(path)
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    File::open(parent.unwrap_or(Path::new(".")))?.sync_all()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::paths::RepoPath;
    use crate::record::{Caller, Change, Trigger};

    fn draft(path: &str) -> Draft {
        Draft {
            trigger: Trigger::Unlink,
            caller: Caller {
                uid: 1,
                gid: 1,
                pid: 1,
            },
            time_ms: 0,
            changes: vec![Change::Remove {
                path: RepoPath::new(path).unwrap(),
            }],
            operation: None,
        }
    }

    fn no_hook() -> SyncHook {
        Box::new(|| Ok(()))
    }

    #[test]
    fn records_survive_reopen_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        {
            let (j, _) = Journal::open(&path, 0, no_hook()).unwrap();
            assert_eq!(j.record(draft("a")).unwrap(), 0);
            assert_eq!(j.record(draft("b")).unwrap(), 1);
        }
        let (j, report) = Journal::open(&path, 0, no_hook()).unwrap();
        assert_eq!(
            report,
            OpenReport {
                records: 2,
                discarded_bytes: 0
            }
        );
        let seqs: Vec<u64> = j.uncommitted().iter().map(|r| r.seq).collect();
        assert_eq!(seqs, [0, 1]);
        assert_eq!(j.record(draft("c")).unwrap(), 2);
    }

    #[test]
    fn torn_tail_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        {
            let (j, _) = Journal::open(&path, 0, no_hook()).unwrap();
            j.record(draft("a")).unwrap();
            j.record(draft("b")).unwrap();
        }
        let full = fs::metadata(&path).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(full - 3)
            .unwrap();

        let (j, report) = Journal::open(&path, 0, no_hook()).unwrap();
        assert_eq!(report.records, 1);
        assert!(report.discarded_bytes > 0);
        assert_eq!(j.uncommitted()[0].seq, 0);
        // The torn record's number is reused: its call never returned.
        assert_eq!(j.record(draft("c")).unwrap(), 1);
    }

    #[test]
    fn read_records_leaves_a_torn_tail_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        assert_eq!(read_records(&path).unwrap(), (Vec::new(), 0), "missing");
        {
            let (j, _) = Journal::open(&path, 0, no_hook()).unwrap();
            j.record(draft("a")).unwrap();
            j.record(draft("b")).unwrap();
        }
        let (records, torn) = read_records(&path).unwrap();
        let (j, _) = Journal::open(&path, 0, no_hook()).unwrap();
        assert_eq!(records, j.uncommitted());
        assert_eq!(torn, 0);
        drop(j);

        let full = fs::metadata(&path).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(full - 3)
            .unwrap();
        let (records, torn) = read_records(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert!(torn > 0);
        assert_eq!(fs::metadata(&path).unwrap().len(), full - 3, "untouched");
    }

    #[test]
    fn corrupt_payload_ends_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        {
            let (j, _) = Journal::open(&path, 0, no_hook()).unwrap();
            j.record(draft("a")).unwrap();
            j.record(draft("b")).unwrap();
        }
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 2;
        bytes[last] ^= 0xff;
        fs::write(&path, &bytes).unwrap();

        let (_, report) = Journal::open(&path, 0, no_hook()).unwrap();
        assert_eq!(report.records, 1);
    }

    #[test]
    fn min_next_seq_continues_after_committed_history() {
        let dir = tempfile::tempdir().unwrap();
        let (j, _) = Journal::open(dir.path().join("journal"), 42, no_hook()).unwrap();
        assert_eq!(j.record(draft("a")).unwrap(), 42);
    }

    #[test]
    fn truncate_keeps_only_later_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        let (j, _) = Journal::open(&path, 0, no_hook()).unwrap();
        for p in ["a", "b", "c"] {
            j.record(draft(p)).unwrap();
        }
        j.truncate_through(1).unwrap();
        assert_eq!(
            j.uncommitted().iter().map(|r| r.seq).collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(j.record(draft("d")).unwrap(), 3);
        drop(j);

        let (j, _) = Journal::open(&path, 0, no_hook()).unwrap();
        assert_eq!(
            j.uncommitted().iter().map(|r| r.seq).collect::<Vec<_>>(),
            [2, 3]
        );
    }

    #[test]
    fn concurrent_records_share_syncs() {
        let dir = tempfile::tempdir().unwrap();
        let syncs = Arc::new(AtomicUsize::new(0));
        let counter = syncs.clone();
        let hook: SyncHook = Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            Ok(())
        });
        let (j, _) = Journal::open(dir.path().join("journal"), 0, hook).unwrap();
        let j = Arc::new(j);

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let j = j.clone();
                std::thread::spawn(move || j.record(draft(&format!("f{i}"))).unwrap())
            })
            .collect();
        let mut seqs: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        seqs.sort_unstable();

        assert_eq!(seqs, (0..16).collect::<Vec<_>>());
        assert_eq!(j.uncommitted().len(), 16);
        assert!(syncs.load(Ordering::SeqCst) < 16, "expected group commits");
    }

    #[test]
    fn failed_sync_fails_the_batch_and_leaves_no_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = fail.clone();
        let hook: SyncHook = Box::new(move || {
            if flag.load(Ordering::SeqCst) {
                Err(io::Error::other("disk full"))
            } else {
                Ok(())
            }
        });
        let (j, _) = Journal::open(&path, 0, hook).unwrap();

        assert!(j.record(draft("a")).is_err());
        fail.store(false, Ordering::SeqCst);
        let seq = j.record(draft("b")).unwrap();
        drop(j);

        let (j, _) = Journal::open(&path, 0, no_hook()).unwrap();
        assert_eq!(
            j.uncommitted().iter().map(|r| r.seq).collect::<Vec<_>>(),
            [seq]
        );
    }

    #[test]
    fn records_from_before_operations_still_decode() {
        // A frame as journals written before `operation` existed hold it.
        let payload = br#"{"seq":7,"trigger":"close","caller":{"uid":1,"gid":2,"pid":3},"time_ms":5,"changes":[{"op":"remove","path":"a"}]}"#;
        let mut frame = Vec::new();
        frame.extend_from_slice(&MAGIC);
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
        frame.extend_from_slice(payload);
        let (records, valid) = decode_all(&frame);
        assert_eq!(valid, frame.len());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].seq, 7);
        assert_eq!(records[0].draft.operation, None);

        // And records without an operation are written the old way.
        let mut out = Vec::new();
        encode(&records[0], &mut out).unwrap();
        assert_eq!(out, frame);
    }

    #[test]
    fn last_and_oldest_sequence_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let (j, _) = Journal::open(dir.path().join("journal"), 0, no_hook()).unwrap();
        assert_eq!((j.last_seq(), j.oldest_uncommitted()), (None, None));
        for path in ["a", "b", "c"] {
            j.record(draft(path)).unwrap();
        }
        assert_eq!((j.last_seq(), j.oldest_uncommitted()), (Some(2), Some(0)));
        j.truncate_through(1).unwrap();
        assert_eq!((j.last_seq(), j.oldest_uncommitted()), (Some(2), Some(2)));
        j.truncate_through(2).unwrap();
        assert_eq!((j.last_seq(), j.oldest_uncommitted()), (Some(2), None));
        let (j, _) = Journal::open(dir.path().join("other"), 42, no_hook()).unwrap();
        assert_eq!(j.last_seq(), Some(41));
    }

    #[test]
    fn backlog_counts_uncommitted() {
        let dir = tempfile::tempdir().unwrap();
        let (j, _) = Journal::open(dir.path().join("journal"), 0, no_hook()).unwrap();
        assert_eq!(j.backlog(), 0);
        for path in ["a", "b", "c"] {
            j.record(draft(path)).unwrap();
        }
        assert_eq!(j.backlog(), 3);
        j.truncate_through(1).unwrap();
        assert_eq!(j.backlog(), 1);
        assert_eq!(j.backlog(), j.uncommitted().len());
    }
}
