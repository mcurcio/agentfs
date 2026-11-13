//! Shared mappings (ledger R1.4, git-model §3) through the real `Volume`, its
//! journal and its Git history, driven by a double of the kernel's one page map
//! and one size per file. The double holds this suite's beliefs about fs/fuse, so
//! it proves the logic only; ci/trees-pagecache.py judges the kernel.
use std::collections::BTreeMap;
use std::path::Path;

use agentfs_core::tree_actor::{TreeId, Volume};

const PAGE: u64 = 4096;
const TREE: TreeId = TreeId::Root;
const PATH: &str = "shared";
const SEED: &[u8] = b"abcdefgh";

struct Cached {
    bytes: Vec<u8>,
    dirty: bool,
}

/// One file's page cache, as fs/fuse keeps it.
struct Kernel {
    v: Volume,
    object: u64,
    pages: BTreeMap<u64, Cached>,
    size: u64,
    /// Handles with a writable shared mapping, the latest first: page writes use
    /// the first (`__fuse_write_file_get`).
    write_files: Vec<u64>,
}

impl Kernel {
    /// A file holding `seed`, saved, with nothing open.
    fn new(root: &Path, seed: &[u8]) -> Self {
        let mut v = Volume::open(root).unwrap();
        let h = v.write_open(TREE, PATH, "seed", true).unwrap();
        v.write(h, 0, seed, false).unwrap();
        let object = v.open_object(h).unwrap();
        v.close(h).unwrap();
        Self {
            v,
            object,
            pages: BTreeMap::new(),
            size: seed.len() as u64,
            write_files: Vec::new(),
        }
    }

    fn saved(&self) -> Vec<u8> {
        self.v.read(TREE, PATH).unwrap().to_vec()
    }

    /// `open(2)`: laundering and invalidation, then the OPEN.
    fn open(&mut self, writer: &str) -> u64 {
        self.invalidate();
        self.v.write_open(TREE, PATH, writer, false).unwrap()
    }

    /// `mmap(2)` of a direct-I/O handle: laundering and invalidation.
    fn map(&mut self, handle: u64) {
        self.invalidate();
        self.write_files.insert(0, handle);
    }

    /// `stat`: a reply with a different size drops pages past it, dirty or not.
    fn stat(&mut self) -> u64 {
        let reply = self
            .v
            .inode_size(TREE, self.object)
            .unwrap_or(self.saved().len() as u64);
        if reply != self.size {
            self.pages.retain(|page, _| page * PAGE < reply);
            self.invalidate();
            self.size = reply;
        }
        reply
    }

    fn fault(&mut self, page: u64) {
        if self.pages.contains_key(&page) {
            return;
        }
        let mut bytes = self
            .v
            .page_fill(TREE, self.object, page * PAGE, PAGE as u32)
            .unwrap();
        bytes.resize(PAGE as usize, 0);
        self.pages.insert(
            page,
            Cached {
                bytes,
                dirty: false,
            },
        );
    }

    /// A mapped store, or SIGBUS past the size. Nothing reaches the view.
    fn store(&mut self, offset: u64, data: &[u8]) {
        assert!(
            offset + data.len() as u64 <= self.size.next_multiple_of(PAGE),
            "SIGBUS: store past the kernel's size {}",
            self.size
        );
        for (index, byte) in data.iter().enumerate() {
            let at = offset + index as u64;
            self.fault(at / PAGE);
            let page = self.pages.get_mut(&(at / PAGE)).unwrap();
            page.bytes[(at % PAGE) as usize] = *byte;
            page.dirty = true;
        }
    }

    /// A mapped load, within the kernel's size.
    fn load(&mut self, offset: u64, len: u64) -> Vec<u8> {
        let end = (offset + len).min(self.size);
        (offset..end)
            .map(|at| {
                self.fault(at / PAGE);
                self.pages[&(at / PAGE)].bytes[(at % PAGE) as usize]
            })
            .collect()
    }

    /// Writeback of every dirty page, cropped to the size, through the first
    /// handle on `write_files`.
    fn flush(&mut self) {
        let dirty: Vec<u64> = self
            .pages
            .iter()
            .filter(|(_, page)| page.dirty)
            .map(|(number, _)| *number)
            .collect();
        for number in dirty {
            let handle = self.write_files[0];
            let keep = self.size.saturating_sub(number * PAGE).min(PAGE) as usize;
            let bytes = self.pages[&number].bytes[..keep].to_vec();
            self.v
                .page_writeback(handle, number * PAGE, &bytes)
                .unwrap();
            self.pages.get_mut(&number).unwrap().dirty = false;
        }
    }

    fn invalidate(&mut self) {
        self.flush();
        self.pages.clear();
    }

    /// A direct write: writeback, invalidate, send.
    fn pwrite(&mut self, handle: u64, offset: u64, data: &[u8]) {
        self.invalidate();
        self.v.write(handle, offset, data, false).unwrap();
        self.size = self.size.max(offset + data.len() as u64);
    }

    /// A direct write with a fault in its window, which caches a stale page.
    fn pwrite_with_a_fault_in_its_window(&mut self, handle: u64, offset: u64, data: &[u8]) {
        self.invalidate();
        self.fault(offset / PAGE);
        self.v.write(handle, offset, data, false).unwrap();
        self.size = self.size.max(offset + data.len() as u64);
    }

    fn ftruncate(&mut self, handle: u64, size: u64) {
        self.flush();
        self.v.truncate(handle, size).unwrap();
        self.stat();
    }

    /// The last reference goes: FLUSH writes the whole inode back, then RELEASE.
    fn close(&mut self, handle: u64) {
        self.flush();
        self.write_files.retain(|mapped| *mapped != handle);
        self.v.close(handle).unwrap();
    }
}

fn tree_with_seed(seed: &[u8]) -> (tempfile::TempDir, Kernel) {
    let tmp = tempfile::tempdir().unwrap();
    let kernel = Kernel::new(tmp.path(), seed);
    (tmp, kernel)
}

/// How many commits on the root name this open.
fn saves_of(root: &Path, open: u64) -> usize {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(root.join("history.git"))
        .args([
            "log",
            &TREE.to_string(),
            "--format=%(trailers:key=Agentfs-Open,valueonly)",
        ])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|line| *line == open.to_string())
        .count()
}

/// What the same calls leave on Linux: every byte in call order.
fn linux(calls: &[(u64, &[u8])]) -> Vec<u8> {
    let mut bytes = SEED.to_vec();
    for (offset, data) in calls {
        bytes[*offset as usize..*offset as usize + data.len()].copy_from_slice(data);
    }
    bytes
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Fd,
    Map,
}

/// Two writers, the observer's descriptor and mapping, and both closes.
fn two_writers(kinds: [Kind; 2], offsets: [u64; 2], reversed: bool) {
    const PAYLOADS: [&[u8]; 2] = [b"WXYZ", b"1234"];
    let first = linux(&[(offsets[0], PAYLOADS[0])]);
    let final_ = linux(&[(offsets[0], PAYLOADS[0]), (offsets[1], PAYLOADS[1])]);
    let label = format!(
        "fd/map {:?} offsets {offsets:?} reversed {reversed}",
        kinds.map(|kind| kind == Kind::Map)
    );
    let (_tmp, mut k) = tree_with_seed(SEED);
    let handles = [k.open("a"), k.open("b")];
    for i in [0, 1] {
        if kinds[i] == Kind::Map {
            k.map(handles[i]);
        }
    }
    let descriptor_snapshot = k.saved();
    for i in [0, 1] {
        match kinds[i] {
            Kind::Fd => k.pwrite(handles[i], offsets[i], PAYLOADS[i]),
            Kind::Map => k.store(offsets[i], PAYLOADS[i]),
        }
    }
    assert_eq!(descriptor_snapshot, SEED, "{label}: pread(C), R1.1");
    assert_eq!(k.load(0, 8), final_, "{label}: C's mapping");
    let close_order = if reversed { [1, 0] } else { [0, 1] };
    k.close(handles[close_order[0]]);
    let after_first_close = k.saved();
    k.close(handles[close_order[1]]);
    if reversed {
        let expected = if kinds == [Kind::Fd, Kind::Fd] {
            // R9.16: of descriptor calls, the later close is final.
            let mut bytes = linux(&[(offsets[1], PAYLOADS[1])]);
            bytes[offsets[0] as usize..offsets[0] as usize + 4].copy_from_slice(PAYLOADS[0]);
            bytes
        } else {
            final_
        };
        assert_eq!(k.saved(), expected, "{label}: after both closes");
    } else {
        let expected = if kinds[1] == Kind::Map {
            &final_
        } else {
            &first
        };
        assert_eq!(&after_first_close, expected, "{label}: after A closes");
        assert_eq!(k.saved(), final_, "{label}: after B closes");
    }
}

#[test]
fn two_writers_in_every_combination() {
    for kinds in [
        [Kind::Fd, Kind::Fd],
        [Kind::Fd, Kind::Map],
        [Kind::Map, Kind::Fd],
        [Kind::Map, Kind::Map],
    ] {
        for offsets in [[0, 2], [0, 4]] {
            two_writers(kinds, offsets, false);
            two_writers(kinds, offsets, true);
        }
    }
}

#[test]
fn a_fill_reads_every_opens_calls() {
    let (_tmp, mut k) = tree_with_seed(SEED);
    let (a, b) = (k.open("a"), k.open("b"));
    k.pwrite(a, 0, b"WX");
    k.pwrite(b, 6, b"YZ");
    let object = k.object;
    let filled = k.v.page_fill(TREE, object, 0, PAGE as u32).unwrap();
    assert_eq!(filled, b"WXcdefYZ");
    // A later call changes the image the next fill reads, unsaved all the same.
    k.pwrite(a, 2, b"!");
    let filled = k.v.page_fill(TREE, object, 0, 4).unwrap();
    assert_eq!(filled, b"WX!d");
    assert_eq!(k.saved(), SEED);
}

#[test]
fn a_stale_page_does_not_revert_a_descriptor_write() {
    let (_tmp, mut k) = tree_with_seed(SEED);
    let (a, b) = (k.open("a"), k.open("b"));
    k.map(b);
    k.pwrite_with_a_fault_in_its_window(a, 0, b"WX");
    k.store(6, b"12");
    k.close(b);
    k.close(a);
    assert_eq!(k.saved(), b"WXcdef12");
}

#[test]
fn the_first_close_saves_what_the_kernel_flushed() {
    let (_tmp, mut k) = tree_with_seed(SEED);
    let (a, b, c) = (k.open("a"), k.open("b"), k.open("c"));
    // Stores from one mapping go out on another's handle. The close of an open
    // with neither saves them all the same.
    k.map(a);
    k.store(0, b"12");
    k.map(b);
    k.flush();
    k.close(c);
    assert_eq!(k.saved(), b"12cdefgh");
    k.close(b);
    k.close(a);
    assert_eq!(k.saved(), b"12cdefgh");
}

#[test]
fn a_later_close_keeps_newer_mapped_stores() {
    let (_tmp, mut k) = tree_with_seed(SEED);
    let (a, b) = (k.open("a"), k.open("b"));
    k.pwrite(a, 0, b"WXYZ");
    k.map(b);
    k.store(0, b"12");
    k.close(b);
    assert_eq!(k.saved(), b"12cdefgh");
    k.close(a);
    assert_eq!(k.saved(), b"12YZefgh", "R9.16's note: Linux's bytes");
}

#[test]
fn a_later_close_does_not_revert_a_third_save() {
    let (_tmp, mut k) = tree_with_seed(SEED);
    let (a, b) = (k.open("a"), k.open("b"));
    k.pwrite(a, 7, b"Q");
    k.map(b);
    k.store(0, b"12");
    k.close(b);
    let d = k.open("d");
    k.pwrite(d, 0, b"WX");
    k.close(d);
    assert_eq!(k.saved(), b"WXcdefgh");
    k.close(a);
    assert_eq!(k.saved(), b"WXcdefgQ", "12 touches none of A's bytes");
}

#[test]
fn a_recovery_close_keeps_a_store_whose_handle_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = Kernel::new(tmp.path(), SEED);
    let (a, b) = (k.open("a"), k.open("b"));
    k.pwrite(a, 0, b"WXYZ");
    k.map(b);
    k.store(0, b"12");
    k.close(b);
    drop(k);
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read(TREE, PATH).unwrap(), b"12YZefgh", "Linux's bytes");
    assert_eq!(saves_of(tmp.path(), a), 1);
    assert_eq!(saves_of(tmp.path(), b), 1);
    drop(v);
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(saves_of(tmp.path(), a), 1, "exactly once");
    assert_eq!(v.read(TREE, PATH).unwrap(), b"12YZefgh");
}

#[test]
fn a_recovery_close_does_not_reapply_a_saved_store() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = Kernel::new(tmp.path(), SEED);
    let (a, b) = (k.open("a"), k.open("b"));
    k.pwrite(a, 7, b"Q");
    k.map(b);
    k.store(0, b"12");
    k.close(b);
    let d = k.open("d");
    k.pwrite(d, 0, b"WX");
    k.close(d);
    drop(k);
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read(TREE, PATH).unwrap(), b"WXcdefgQ");
    assert_eq!(saves_of(tmp.path(), a), 1, "exactly once");
}

#[test]
fn a_recovery_close_saves_a_flushed_store_no_close_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = Kernel::new(tmp.path(), SEED);
    let a = k.open("a");
    k.map(a);
    k.store(2, b"12");
    k.flush();
    // Not flushed: the view host's death loses it, a stated cost of R1.4.
    k.store(6, b"!!");
    drop(k);
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read(TREE, PATH).unwrap(), b"ab12efgh");
    assert_eq!(saves_of(tmp.path(), a), 1, "exactly once");
}

#[test]
fn a_page_write_records_only_changed_bytes_and_a_clean_one_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = Kernel::new(tmp.path(), SEED);
    let a = k.open("a");
    k.map(a);
    k.store(0, b"a"); // The same byte: dirty, and nothing changed.
    let journal = tmp.path().join("calls.wal");
    let before = std::fs::metadata(&journal).unwrap().len();
    k.flush();
    assert_eq!(std::fs::metadata(&journal).unwrap().len(), before);
    k.store(0, b"Z");
    k.flush();
    assert!(std::fs::metadata(&journal).unwrap().len() > before);
    k.close(a);
    assert_eq!(k.saved(), b"Zbcdefgh");
}

#[test]
fn a_page_write_splits_at_page_boundaries_and_a_tail_compares_with_zero() {
    let mut seed = vec![b'.'; PAGE as usize + 8];
    seed[PAGE as usize - 1] = b'^';
    let (_tmp, mut k) = tree_with_seed(&seed);
    let a = k.open("a");
    k.map(a);
    k.store(PAGE - 2, b"1234");
    k.flush();
    // Grown past its base: a page's old tail is zero, so only the store counts.
    k.ftruncate(a, 2 * PAGE);
    k.store(PAGE + 100, b"Q");
    k.close(a);
    let mut expected = seed.clone();
    expected.resize(2 * PAGE as usize, 0);
    expected[PAGE as usize - 2..PAGE as usize + 2].copy_from_slice(b"1234");
    expected[PAGE as usize + 100] = b'Q';
    assert_eq!(k.saved(), expected);
}

#[test]
fn a_writable_descriptor_reads_page_writes_and_a_read_only_one_keeps_its_snapshot() {
    let (_tmp, mut k) = tree_with_seed(SEED);
    let (a, b) = (k.open("a"), k.open("b"));
    k.map(a);
    k.store(0, b"12");
    k.flush();
    assert_eq!(k.v.read_open(b).unwrap(), b"12cdefgh", "an open before it");
    let c = k.open("c");
    assert_eq!(k.v.read_open(c).unwrap(), b"12cdefgh", "an open after it");
    assert_eq!(k.v.read(TREE, PATH).unwrap(), SEED, "a new read-only open");
}

#[test]
fn a_reply_keeps_the_size_the_writers_gave_the_file() {
    // Create, grow and map in one program, with another stating the path.
    let (tmp, mut k) = tree_with_seed(b"");
    let a = k.open("a");
    k.ftruncate(a, 3 * PAGE);
    assert_eq!(k.size, 3 * PAGE, "else the program's mapping dies");
    k.map(a);
    k.store(2 * PAGE + 5, b"kept");
    assert_eq!(k.stat(), 3 * PAGE);
    assert_eq!(
        k.load(2 * PAGE + 5, 4),
        b"kept",
        "else a stat drops the store"
    );
    k.close(a);
    let saved = k.saved();
    assert_eq!(saved.len() as u64, 3 * PAGE);
    assert_eq!(&saved[2 * PAGE as usize + 5..][..4], b"kept");
    assert_eq!(k.v.inode_size(TREE, k.object), None);
    drop(tmp);
}

#[test]
fn an_unlinked_file_answers_its_size_while_a_write_open_lives() {
    let (_tmp, mut k) = tree_with_seed(SEED);
    let a = k.open("a");
    k.pwrite(a, 8, b"XY");
    k.v.unlink(TREE, PATH, "w").unwrap();
    assert_eq!(k.v.inode_size(TREE, k.object), Some(10));
    k.v.close(a).unwrap();
    assert_eq!(k.v.inode_size(TREE, k.object), None);
}

#[test]
fn a_fill_with_no_write_open_reads_saved_bytes_and_keeps_no_state() {
    let (_tmp, mut k) = tree_with_seed(SEED);
    let object = k.object;
    assert_eq!(k.v.page_fill(TREE, object, 2, 3).unwrap(), b"cde");
    assert_eq!(k.v.page_fill(TREE, object, 8, 3).unwrap(), b"");
    assert_eq!(k.v.inode_size(TREE, object), None);
    assert!(matches!(
        k.v.page_fill(TREE, object + 1000, 0, 3),
        Err(agentfs_core::tree_actor::Error::NotFound)
    ));
    assert!(matches!(
        k.v.page_writeback(1 << 40, 0, b"x"),
        Err(agentfs_core::tree_actor::Error::NotFound)
    ));
}

#[test]
fn a_take_names_the_files_it_changed_and_a_save_does_not() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let h = v.write_open(TREE, PATH, "seed", true).unwrap();
    v.write(h, 0, SEED, false).unwrap();
    let object = v.open_object(h).unwrap();
    v.close(h).unwrap();
    let h = v.write_open(TREE, "other", "seed", true).unwrap();
    v.write(h, 0, b"other", false).unwrap();
    v.close(h).unwrap();
    let fork = v.fork().unwrap();
    while v.progress().unwrap() {}
    v.drain_changed();

    let h = v.write_open(TREE, PATH, "w", false).unwrap();
    v.write(h, 0, b"WX", false).unwrap();
    v.close(h).unwrap();
    assert_eq!(v.drain_changed(), [], "a save is no take");
    while v.progress().unwrap() {}
    assert_eq!(v.read(fork, PATH).unwrap(), b"WXcdefgh");
    assert_eq!(v.drain_changed(), [(fork, object)]);
    assert_eq!(v.drain_changed(), [], "drained");
}
