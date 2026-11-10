//! What each version was written over: the checks' oracle for "nothing is
//! silently lost" and "no stale line reverts a newer one", read from the
//! version records in git's blobs, not from the model's replay (MODEL.md,
//! "What must always hold").
//!
//! New objects are found by listing the repository's loose objects (the
//! model never packs: asserted), and read through one long-lived
//! `git cat-file --batch` per checker, so a check that finds nothing new
//! starts no process. Refs are read the same way, from the loose ref files.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::content::Content;

/// One `git cat-file --batch`, for the checker's lifetime.
#[derive(Debug)]
struct Batch {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
}

impl Batch {
    fn start(repo: &Path) -> Self {
        let mut child = Command::new("git")
            .current_dir(repo)
            .args(["cat-file", "--batch"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git cat-file --batch runs");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().map(BufReader::new);
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// Object `id`'s type and bytes.
    fn read(&mut self, id: &str) -> (String, Vec<u8>) {
        let stdin = self.stdin.as_mut().expect("open");
        if let Err(e) = writeln!(stdin, "{id}").and_then(|()| stdin.flush()) {
            self.fail(&format!("cat-file took no request for {id}: {e}"));
        }
        let mut header = String::new();
        if let Err(e) = self.out().read_line(&mut header) {
            self.fail(&format!("cat-file gave no answer for {id}: {e}"));
        }
        let mut f = header.split_whitespace();
        let (_, kind, size) = (f.next(), f.next(), f.next());
        let Some(size) = size.and_then(|n| n.parse::<usize>().ok()) else {
            self.fail(&format!("unparsable cat-file header {header:?} for {id}"));
        };
        let mut body = vec![0; size + 1];
        if let Err(e) = self.out().read_exact(&mut body) {
            self.fail(&format!("cat-file's object {id} is short: {e}"));
        }
        body.truncate(size);
        (kind.unwrap_or_default().to_string(), body)
    }

    fn out(&mut self) -> &mut BufReader<ChildStdout> {
        self.stdout.as_mut().expect("open")
    }

    /// Stops cat-file and panics with `what` and everything it wrote to
    /// stderr.
    fn fail(&mut self, what: &str) -> ! {
        self.stdin.take();
        self.stdout.take();
        let mut stderr = String::new();
        if let Some(mut e) = self.child.stderr.take() {
            let _ = e.read_to_string(&mut stderr);
        }
        let _ = self.child.wait();
        panic!("{what}; cat-file's stderr: {stderr:?}");
    }
}

impl Drop for Batch {
    /// Closes both pipes first, so a cat-file blocked writing an answer no
    /// one reads (a panic mid-read) ends instead of hanging the wait.
    fn drop(&mut self) {
        self.stdin.take();
        self.stdout.take();
        self.child.stderr.take();
        let _ = self.child.wait();
    }
}

/// A version `b` descends from `a` when `b` was written over `a`: `a` is the
/// version `b`'s writer saw of that line, or one that version was written
/// over, and so on. From what the writer could read (R3c.2), never from which
/// commits its version merged: a sync that kept the fork's own line over the
/// upstream's did not show the upstream's version to the fork's writer.
#[derive(Default, Debug)]
pub(crate) struct Lineage {
    /// Each version, to every version a record of it names as its base.
    over: BTreeMap<String, BTreeSet<String>>,
    /// Each version, to every version it descends from (itself excluded).
    saw: BTreeMap<String, BTreeSet<String>>,
    /// The loose objects listed so far.
    listed: BTreeSet<String>,
    batch: Option<Batch>,
}

/// The repository's object directory, after checking it holds no pack: the
/// model writes only loose objects (plumbing never packs, and it never runs
/// `gc`, `repack` or `fetch`).
fn objects(repo: &Path) -> PathBuf {
    formats(&repo.join(".git"));
    let dir = repo.join(".git/objects");
    let packs = std::fs::read_dir(dir.join("pack"))
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    assert!(
        packs == 0,
        "{} holds packs: the checker reads loose objects only",
        dir.display()
    );
    dir
}

/// Checks that git directory `git` is in the formats the checker reads:
/// SHA-1 object ids (40 hex digits) and loose ref files. Another object
/// format would leave every object unread, and reftable keeps `refs/` a
/// constant stub, so history would never be read again. The model's git
/// runner pins both (`crate::git`); this refuses any repository that
/// escaped it.
fn formats(git: &Path) {
    let config = std::fs::read_to_string(git.join("config")).unwrap_or_default();
    for line in config.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (
            key.trim().to_ascii_lowercase(),
            value.trim().to_ascii_lowercase(),
        );
        assert!(
            key != "objectformat" || value == "sha1",
            "{} has object format {value}: the checker reads SHA-1 objects only",
            git.display()
        );
        assert!(
            key != "refstorage" || value == "files",
            "{} has ref format {value}: the checker reads loose ref files only",
            git.display()
        );
    }
    assert!(
        !git.join("reftable").exists(),
        "{} has ref format reftable: the checker reads loose ref files only",
        git.display()
    );
}

impl Lineage {
    /// Version `v` was written over `over`, the version its writer saw
    /// (none: a new line).
    pub(crate) fn record(&mut self, v: &str, over: Option<&str>) {
        let bases = self.over.entry(v.to_string()).or_default();
        if let Some(o) = over
            && bases.insert(o.to_string())
            && self.saw.contains_key(v)
        {
            // A base after the closure was taken: only a version recorded
            // twice. Take every closure again.
            self.saw.clear();
        }
    }

    /// Every version `v` descends from, by its recorded bases; a cycle ends
    /// the walk.
    fn closure(&self, v: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut todo: Vec<&str> = vec![v];
        while let Some(x) = todo.pop() {
            for b in self.over.get(x).into_iter().flatten() {
                if seen.insert(b.clone()) {
                    todo.push(b);
                }
            }
        }
        seen
    }

    fn close(&mut self) {
        let ids: Vec<String> = self
            .over
            .keys()
            .filter(|v| !self.saw.contains_key(*v))
            .cloned()
            .collect();
        for v in ids {
            let c = self.closure(&v);
            self.saw.insert(v, c);
        }
    }

    /// Records every version in every blob of `repo` not read yet, reachable
    /// or not (a rolled-back transaction's included): each file's blob holds
    /// every version's record beside its bytes.
    pub(crate) fn read_new(&mut self, repo: &Path) {
        let dir = objects(repo);
        let mut new = Vec::new();
        for fan in std::fs::read_dir(&dir)
            .expect("the object directory")
            .flatten()
        {
            let name = fan.file_name().to_string_lossy().into_owned();
            if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            for o in std::fs::read_dir(fan.path())
                .expect("a fan-out directory")
                .flatten()
            {
                let id = format!("{name}{}", o.file_name().to_string_lossy());
                if id.len() == 40 && !self.listed.contains(&id) {
                    new.push(id);
                }
            }
        }
        if new.is_empty() {
            return;
        }
        let batch = self.batch.get_or_insert_with(|| Batch::start(repo));
        let mut records = Vec::new();
        for id in &new {
            let (kind, body) = batch.read(id);
            if kind == "blob"
                && let Some(c) = std::str::from_utf8(&body).ok().and_then(Content::parse)
            {
                records.extend(c.records().map(|l| (l.id.clone(), l.over.clone())));
            }
        }
        for (v, over) in records {
            self.record(&v, over.as_deref());
        }
        self.listed.extend(new);
        self.close();
    }

    /// Whether `newer` descends from `older` (or is it).
    pub(crate) fn descends(&self, newer: &str, older: &str) -> bool {
        newer == older || self.saw.get(newer).is_some_and(|s| s.contains(older))
    }
}

/// Every ref and its commit, read from the loose refs (the model never packs
/// them: asserted): what `git for-each-ref` lists, without a process.
pub(crate) fn refs(repo: &Path) -> String {
    let git = repo.join(".git");
    formats(&git);
    assert!(
        !git.join("packed-refs").exists(),
        "{} has packed refs: the checker reads loose refs only",
        git.display()
    );
    let mut out = Vec::new();
    let mut todo = vec![git.join("refs")];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).expect("a refs directory").flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else {
                let at = std::fs::read_to_string(&p).expect("a loose ref");
                out.push(format!(
                    "{} {}",
                    at.trim_end(),
                    p.strip_prefix(&git).unwrap_or(&p).display()
                ));
            }
        }
    }
    out.sort();
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lineage_follows_views() {
        let mut l = Lineage::default();
        l.record("m1.2", Some("a0"));
        l.record("m1.3", Some("m1.2"));
        l.record("c1.1", Some("a0"));
        l.record("c1.2", None);
        l.close();
        assert!(l.descends("m1.3", "a0") && l.descends("m1.3", "m1.2"));
        assert!(!l.descends("m1.2", "m1.3"));
        assert!(!l.descends("c1.1", "m1.2") && !l.descends("m1.2", "c1.1"));
        assert!(!l.descends("c1.2", "a0"));
    }

    #[test]
    fn a_cycle_ends_the_walk() {
        let mut l = Lineage::default();
        l.record("m1.2", Some("m1.3"));
        l.record("m1.3", Some("m1.2"));
        l.close();
        assert!(l.descends("m1.2", "m1.3") && !l.descends("m1.2", "a0"));
    }

    /// An empty `.git` with its object and ref folders, under a fresh name.
    fn bare_git_dir(name: &str) -> PathBuf {
        let repo = std::env::temp_dir().join(format!(
            "agentfs-model-lineage-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(repo.join(".git/objects/pack")).expect("objects");
        std::fs::create_dir_all(repo.join(".git/refs/heads")).expect("refs");
        repo
    }

    #[test]
    #[should_panic(expected = "holds packs")]
    fn a_pack_stops_the_object_read() {
        let repo = bare_git_dir("pack");
        std::fs::write(repo.join(".git/objects/pack/pack-0.pack"), "").expect("a pack");
        Lineage::default().read_new(&repo);
    }

    #[test]
    #[should_panic(expected = "has packed refs")]
    fn packed_refs_stop_the_ref_read() {
        let repo = bare_git_dir("packed-refs");
        std::fs::write(repo.join(".git/packed-refs"), "").expect("packed refs");
        refs(&repo);
    }

    #[test]
    #[should_panic(expected = "has object format sha256")]
    fn another_object_format_stops_the_object_read() {
        let repo = bare_git_dir("sha256");
        std::fs::write(
            repo.join(".git/config"),
            "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectFormat = sha256\n",
        )
        .expect("a config");
        Lineage::default().read_new(&repo);
    }

    #[test]
    #[should_panic(expected = "has ref format reftable")]
    fn another_ref_format_stops_the_ref_read() {
        let repo = bare_git_dir("reftable");
        std::fs::write(
            repo.join(".git/config"),
            "[core]\n\trepositoryformatversion = 1\n[extensions]\n\trefStorage = reftable\n",
        )
        .expect("a config");
        refs(&repo);
    }
}
