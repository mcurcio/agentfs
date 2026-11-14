//! The builder's packer: one `git fast-import` child per batch, fed blobs
//! and commits on its stdin, which writes one pack (docs/architecture.md,
//! "Technology choices"). Its durability is ours: [`Feed::finish`] fsyncs the
//! pack files it installed and `objects/pack` before anything names them.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin};

use git2::Oid;

use super::built::BuildError;
use crate::tree_actor::TreeId;
use crate::tree_git::TreeGit;

/// A commit's parent: one built already, or one this batch made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) enum Parent {
    Built(Oid),
    Mark(u64),
}

impl Parent {
    fn dataref(self) -> String {
        match self {
            Self::Built(oid) => oid.to_string(),
            Self::Mark(mark) => format!(":{mark}"),
        }
    }
}

/// One change of a commit's tree, relative to its first parent's.
#[derive(Clone, Debug)]
pub(in crate::tree_actor) enum Edit {
    /// A blob this batch fed.
    Blob {
        path: String,
        mark: u64,
    },
    /// A blob already in the store.
    Stored {
        path: String,
        oid: Oid,
    },
    Inline {
        path: String,
        bytes: Vec<u8>,
    },
    Delete {
        path: String,
    },
    /// Empties the tree first: what follows is the whole tree.
    DeleteAll,
}

pub(in crate::tree_actor) struct CommitSpec<'a> {
    pub tree: TreeId,
    pub parents: &'a [Parent],
    pub at_ns: u64,
    pub message: &'a str,
    pub edits: &'a [Edit],
}

pub(in crate::tree_actor) struct Feed {
    child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
    next: u64,
    marks: PathBuf,
    pack_dir: PathBuf,
    /// The pack directory's entries before the child started.
    before: BTreeSet<std::ffi::OsString>,
}

/// A path as fast-import reads it: C-quoted when it needs to be.
fn quoted(path: &str) -> String {
    if !path.starts_with('"') && !path.contains(['\\', '\n', '"']) {
        return path.to_owned();
    }
    let mut out = String::from('"');
    for c in path.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn packer(e: impl std::fmt::Display) -> BuildError {
    BuildError::Packer(e.to_string())
}

fn entries(dir: &Path) -> Result<BTreeSet<std::ffi::OsString>, BuildError> {
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(dir).map_err(BuildError::from)? {
        names.insert(entry.map_err(BuildError::from)?.file_name());
    }
    Ok(names)
}

impl Feed {
    /// Starts `git fast-import` in the repository, its marks exported to `marks`.
    pub fn start(repo: &TreeGit, marks: &Path) -> Result<Self, BuildError> {
        let pack_dir = repo.path().join("objects").join("pack");
        fs::create_dir_all(&pack_dir).map_err(BuildError::from)?;
        let before = entries(&pack_dir)?;
        // A batch cut short (a kill, a power cut) leaves its marks and git's
        // lock of them: neither names anything.
        let _ = fs::remove_file(marks);
        let mut lock = marks.as_os_str().to_owned();
        lock.push(".lock");
        let _ = fs::remove_file(PathBuf::from(lock));
        let mut child = repo.fast_import(marks).map_err(packer)?;
        let stdin = child.stdin.take().map(BufWriter::new);
        Ok(Self {
            child,
            stdin,
            next: 1,
            marks: marks.to_owned(),
            pack_dir,
            before,
        })
    }

    fn out(&mut self) -> Result<&mut BufWriter<ChildStdin>, BuildError> {
        self.stdin
            .as_mut()
            .ok_or_else(|| packer("fast-import's input is closed"))
    }

    fn mark(&mut self) -> u64 {
        let mark = self.next;
        self.next += 1;
        FED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        mark
    }

    /// A blob of `bytes`: its mark.
    #[cfg(test)]
    pub fn blob(&mut self, bytes: &[u8]) -> Result<u64, BuildError> {
        self.blob_from(bytes.len() as u64, bytes)
    }

    /// A blob of the `len` bytes `reader` gives, streamed: its mark. A file
    /// map's bytes go to fast-import without being held in memory.
    pub fn blob_from(&mut self, len: u64, mut reader: impl Read) -> Result<u64, BuildError> {
        let mark = self.mark();
        let out = self.out()?;
        write!(out, "blob\nmark :{mark}\ndata {len}\n").map_err(packer)?;
        let copied = std::io::copy(&mut (&mut reader).take(len), out).map_err(packer)?;
        if copied != len {
            return Err(packer("a file map ended before its length"));
        }
        out.write_all(b"\n").map_err(packer)?;
        Ok(mark)
    }

    /// `commit refs/build/<tree>`, author and committer `agentfs
    /// <agentfs@localhost>` at `at_ns` in `+0000`, `from` and `merge` from
    /// `parents`, then the edits: the bytes `git commit-tree` would write.
    pub fn commit(&mut self, spec: &CommitSpec<'_>) -> Result<u64, BuildError> {
        let mark = self.mark();
        let seconds = spec.at_ns / 1_000_000_000;
        let ident = format!("agentfs <agentfs@localhost> {seconds} +0000");
        let out = self.out()?;
        write!(
            out,
            "commit refs/build/{}\nmark :{mark}\nauthor {ident}\ncommitter {ident}\ndata {}\n",
            spec.tree,
            spec.message.len()
        )
        .map_err(packer)?;
        out.write_all(spec.message.as_bytes()).map_err(packer)?;
        out.write_all(b"\n").map_err(packer)?;
        let mut parents = spec.parents.iter();
        let first = parents
            .next()
            .ok_or_else(|| packer("a built commit has a parent"))?;
        writeln!(out, "from {}", first.dataref()).map_err(packer)?;
        for parent in parents {
            writeln!(out, "merge {}", parent.dataref()).map_err(packer)?;
        }
        for edit in spec.edits {
            match edit {
                Edit::Blob { path, mark } => {
                    writeln!(out, "M 100644 :{mark} {}", quoted(path)).map_err(packer)?;
                }
                Edit::Stored { path, oid } => {
                    writeln!(out, "M 100644 {oid} {}", quoted(path)).map_err(packer)?;
                }
                Edit::Inline { path, bytes } => {
                    writeln!(
                        out,
                        "M 100644 inline {}\ndata {}",
                        quoted(path),
                        bytes.len()
                    )
                    .map_err(packer)?;
                    out.write_all(bytes).map_err(packer)?;
                    out.write_all(b"\n").map_err(packer)?;
                }
                Edit::Delete { path } => writeln!(out, "D {}", quoted(path)).map_err(packer)?,
                Edit::DeleteAll => out.write_all(b"deleteall\n").map_err(packer)?,
            }
        }
        out.write_all(b"\n").map_err(packer)?;
        Ok(mark)
    }

    /// `done`, waits for the child, reads the marks; then fsyncs every new
    /// file in `objects/pack` (pack, rev, idx) and the directory. Each mark's
    /// object.
    pub fn finish(mut self) -> Result<BTreeMap<u64, Oid>, BuildError> {
        if let Some(mut out) = self.stdin.take() {
            out.write_all(b"done\n").map_err(packer)?;
            out.flush().map_err(packer)?;
        }
        let mut stderr = String::new();
        if let Some(mut err) = self.child.stderr.take() {
            let _ = err.read_to_string(&mut stderr);
        }
        let status = self.child.wait().map_err(packer)?;
        if !status.success() {
            let first = stderr.lines().next().unwrap_or("").to_owned();
            if stderr.contains("No space left") {
                return Err(BuildError::NoSpace);
            }
            return Err(BuildError::Packer(format!("{status}: {first}")));
        }
        let mut marks = BTreeMap::new();
        if self.next > 1 {
            let text = fs::read_to_string(&self.marks).map_err(BuildError::from)?;
            for line in text.lines() {
                let (mark, oid) = line
                    .strip_prefix(':')
                    .and_then(|rest| rest.split_once(' '))
                    .ok_or_else(|| packer("a marks line"))?;
                let mark: u64 = mark.parse().map_err(packer)?;
                marks.insert(mark, Oid::from_str(oid.trim()).map_err(packer)?);
            }
        }
        let _ = fs::remove_file(&self.marks);
        let after = entries(&self.pack_dir)?;
        let mut synced = 0_u64;
        for name in after.difference(&self.before) {
            if name.to_string_lossy().starts_with("tmp_") {
                continue;
            }
            File::open(self.pack_dir.join(name))
                .and_then(|f| f.sync_all())
                .map_err(BuildError::from)?;
            synced += 1;
        }
        File::open(&self.pack_dir)
            .and_then(|d| d.sync_all())
            .map_err(BuildError::from)?;
        PACK_SYNCS.fetch_add(synced, std::sync::atomic::Ordering::Relaxed);
        #[cfg(test)]
        SYNCED_HERE.with(|here| here.set(here.get() + synced));
        Ok(marks)
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        // A batch dropped before `finish`: end the child, which writes nothing
        // anything names.
        if self.stdin.take().is_some() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Pack files this thread's feeds fsynced: a unit test's exact count.
    static SYNCED_HERE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

static PACK_SYNCS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Blobs and commits the builder has fed fast-import in this process: a
/// measurement (the trees it makes of them are fast-import's).
pub(in crate::tree_actor) fn fed() -> u64 {
    FED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Pack files the builder has fsynced in this process: a measurement.
pub(in crate::tree_actor) fn pack_syncs() -> u64 {
    PACK_SYNCS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "a test makes a store of its own")]
mod tests {
    use super::*;

    #[test]
    fn feed_writes_a_commit_git_reads_and_fsyncs_what_it_installed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = TreeGit::init(dir.path().join("r.git")).unwrap();
        repo.write_config("refs/heads/root", &BTreeMap::new())
            .unwrap();
        let empty = repo.write_tree(&[]).unwrap();
        let root = repo.write_commit(empty, &[], "Root\n").unwrap();
        repo.sync_objects().unwrap();
        let marks = dir.path().join("build.marks");
        let before = pack_syncs();
        let mut feed = Feed::start(&repo, &marks).unwrap();
        let blob = feed.blob(b"hello\n").unwrap();
        let first = feed
            .commit(&CommitSpec {
                tree: TreeId::Root,
                parents: &[Parent::Built(root)],
                at_ns: 1_700_000_000_000_000_000,
                message: "Direct by w\n",
                edits: &[
                    Edit::Blob {
                        path: "files/a b".into(),
                        mark: blob,
                    },
                    Edit::Inline {
                        path: "\"odd".into(),
                        bytes: b"x".to_vec(),
                    },
                ],
            })
            .unwrap();
        let second = feed
            .commit(&CommitSpec {
                tree: TreeId::Fork(3),
                parents: &[Parent::Mark(first)],
                at_ns: 1_700_000_001_000_000_000,
                message: "next\n",
                edits: &[Edit::Delete {
                    path: "\"odd".into(),
                }],
            })
            .unwrap();
        let marks = feed.finish().unwrap();
        assert!(pack_syncs() > before, "the new pack files are fsynced");
        repo.refresh().unwrap();
        let commit = repo.read_commit(marks[&second]).unwrap();
        assert_eq!(commit.parents, vec![marks[&first]]);
        assert_eq!(commit.message, b"next\n");
        let names: Vec<_> = repo
            .list_tree(commit.tree)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec![b"files".to_vec()]);
        let first = repo.read_commit(marks[&first]).unwrap();
        let files = repo.list_tree(first.tree).unwrap();
        assert_eq!(files.len(), 2, "files/ and the quoted name");
        // fast-import's commit is what commit-tree writes: tree, parent, author, committer.
        let fsck = std::process::Command::new("git")
            .args(["fsck", "--strict"])
            .env("GIT_DIR", dir.path().join("r.git"))
            .output()
            .unwrap();
        assert!(fsck.status.success(), "{fsck:?}");
    }

    #[test]
    fn feed_fsyncs_what_it_installed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = TreeGit::init(dir.path().join("r.git")).unwrap();
        let pack_dir = dir.path().join("r.git/objects/pack");
        let before = entries(&pack_dir).unwrap();
        let synced = SYNCED_HERE.with(std::cell::Cell::get);
        let mut feed = Feed::start(&repo, &dir.path().join("build.marks")).unwrap();
        feed.blob(b"one\n").unwrap();
        feed.blob(b"two\n").unwrap();
        feed.finish().unwrap();
        let installed: Vec<_> = entries(&pack_dir)
            .unwrap()
            .difference(&before)
            .filter(|name| !name.to_string_lossy().starts_with("tmp_"))
            .cloned()
            .collect();
        assert!(
            installed
                .iter()
                .any(|n| n.to_string_lossy().ends_with(".pack"))
                && installed
                    .iter()
                    .any(|n| n.to_string_lossy().ends_with(".idx")),
            "{installed:?}"
        );
        assert_eq!(
            SYNCED_HERE.with(std::cell::Cell::get) - synced,
            installed.len() as u64,
            "every file fast-import installed is fsynced"
        );
    }
}
