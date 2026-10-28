//! The local LFS object cache (design §2.2, §7): `<state>/lfs/objects/aa/bb/<oid>`,
//! the layout git-lfs uses in `.git/lfs/objects`, plus `<state>/lfs/tmp`
//! for objects being written and `<state>/lfs/incomplete/<oid>` for
//! downloads that can resume.
//!
//! Objects are written to `tmp/`, fsynced, renamed into place and their
//! directories fsynced, so an object that exists is whole and durable.
//! Commits insert frozen copies from `pending/` (a reflink where the
//! filesystem allows, as for freezing); downloads are hashed while they
//! are written and refused unless they match.
//!
//! **Resumable downloads** ([`Partial`]) write into `incomplete/<oid>`
//! (mode 0600) under an exclusive `flock` on that file
//! ([`crate::file_lock`]), so one process and thread at a time downloads
//! an object; another waits, then finds it cached. A partial survives failures, commands and restarts: the next
//! download hashes what is there and asks the server for the rest (HTTP
//! `Range`). On completion its size and SHA-256 are checked, and it is
//! fsynced and renamed into `objects/`; a mismatch deletes it. `open`
//! clears `tmp/` but never `incomplete/`; only `agentfs lfs prune` removes
//! partials nobody holds.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::LfsError;
use super::pointer::{Hashing, LfsOid, Pointer};
use crate::events::PushFailureKind;
use crate::file_lock::{Attempt, FileLock, Mode};

/// The object cache of one state area.
#[derive(Debug)]
pub struct ObjectCache {
    dir: PathBuf,
    objects: PathBuf,
    tmp: PathBuf,
    incomplete: PathBuf,
}

impl ObjectCache {
    /// Opens (creating if needed) the cache at `dir` (`<state>/lfs`),
    /// removing whatever a crash left in `tmp/`.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let cache = Self::at(dir)?;
        let tmp = &cache.tmp;
        for entry in fs::read_dir(tmp)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(cache)
    }

    /// The cache at `dir` as another user of the same state area sees it
    /// (a pusher, a restore): created if missing, but `tmp/` is left alone,
    /// since the committer may be writing there.
    pub fn at(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        let objects = dir.join("objects");
        let tmp = dir.join("tmp");
        let incomplete = dir.join("incomplete");
        fs::create_dir_all(&objects)?;
        fs::create_dir_all(&tmp)?;
        fs::create_dir_all(&incomplete)?;
        Ok(Self {
            dir,
            objects,
            tmp,
            incomplete,
        })
    }

    /// The cache's directory, `<state>/lfs`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every cached object, with its modification time (when it was
    /// cached): files at `objects/aa/bb/<oid>` whose name is an oid that
    /// starts with `aabb`. Anything else there is left alone.
    pub fn list(&self) -> io::Result<Vec<(Pointer, std::time::SystemTime)>> {
        let mut out = Vec::new();
        for aa in read_dir_sorted(&self.objects)? {
            if !aa.is_dir() {
                continue;
            }
            for bb in read_dir_sorted(&aa)? {
                if !bb.is_dir() {
                    continue;
                }
                for path in read_dir_sorted(&bb)? {
                    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    let Ok(oid) = name.parse::<LfsOid>() else {
                        continue;
                    };
                    if object_path(&self.objects, &oid) != path {
                        continue;
                    }
                    let meta = match fs::symlink_metadata(&path) {
                        Ok(meta) if meta.is_file() => meta,
                        Ok(_) => continue,
                        Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                        Err(e) => return Err(e),
                    };
                    out.push((
                        Pointer {
                            oid,
                            size: meta.len(),
                        },
                        meta.modified()?,
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Deletes the cached object `pointer` (if it is still there with that
    /// size), removing its `aa/bb` directories once empty and syncing the
    /// directories above. Whether it was deleted.
    pub fn remove(&self, pointer: &Pointer) -> io::Result<bool> {
        let path = self.path(&pointer.oid);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() && meta.len() == pointer.size => {}
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        }
        fs::remove_file(&path)?;
        let bb = path.parent().expect("an object path has a parent");
        let aa = bb.parent().expect("an object path has a grandparent");
        let _ = fs::remove_dir(bb);
        let _ = fs::remove_dir(aa);
        for dir in [bb, aa, &self.objects] {
            if let Ok(dir) = File::open(dir) {
                dir.sync_all()?;
            }
        }
        Ok(true)
    }

    /// Deletes every partial download in `incomplete/` that no download
    /// holds (its lock is free); those in use are kept. The number
    /// deleted and their bytes.
    pub fn remove_free_partials(&self) -> io::Result<(usize, u64)> {
        let mut removed = (0, 0);
        for path in read_dir_sorted(&self.incomplete)? {
            let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
                continue;
            };
            let Ok(Attempt::Taken(lock)) = FileLock::try_lock(file, Mode::Exclusive) else {
                continue; // A download holds it.
            };
            let len = lock.file().metadata()?.len();
            match fs::remove_file(&path) {
                Ok(()) => {
                    removed.0 += 1;
                    removed.1 += len;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        File::open(&self.incomplete)?.sync_all()?;
        Ok(removed)
    }

    /// `<state>/lfs/incomplete`, where resumable downloads are written.
    pub fn incomplete_dir(&self) -> &Path {
        &self.incomplete
    }

    /// The resumable download of `pointer`'s object: its partial file,
    /// locked for this caller (waiting while another holds it), with what
    /// it holds already hashed. `None` once the object is cached (another
    /// download finished it meanwhile).
    pub fn partial(&self, pointer: &Pointer) -> Result<Option<Partial>, LfsError> {
        let local = |e: io::Error| {
            LfsError::local(format!(
                "resuming the download of LFS object {}: {e}",
                pointer.oid
            ))
        };
        let path = self.incomplete.join(pointer.oid.to_string());
        loop {
            if self.contains(pointer) {
                return Ok(None);
            }
            let file = FileLock::open(&path).map_err(local)?;
            let lock = FileLock::wait(file, Mode::Exclusive).map_err(local)?;
            if self.contains(pointer) {
                return Ok(None);
            }
            // The file locked must still be the one at the path: a holder
            // that finished renamed it into objects/, or a prune removed
            // it, while this caller waited.
            let held = lock.file().metadata().map_err(local)?;
            match fs::metadata(&path) {
                Ok(now) if now.ino() == held.ino() && now.dev() == held.dev() => {}
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(local(e)),
            }
            let mut partial = Partial {
                lock,
                path,
                pointer: *pointer,
                hashing: Hashing::new(),
            };
            partial.rehash().map_err(local)?;
            return Ok(Some(partial));
        }
    }

    /// Where the object `oid` is (or would be).
    pub fn path(&self, oid: &LfsOid) -> PathBuf {
        object_path(&self.objects, oid)
    }

    /// Whether the object is cached with the pointer's size.
    pub fn contains(&self, pointer: &Pointer) -> bool {
        fs::metadata(self.path(&pointer.oid)).is_ok_and(|m| m.is_file() && m.len() == pointer.size)
    }

    /// Opens a cached object, checking its size.
    pub fn open_object(&self, pointer: &Pointer) -> io::Result<File> {
        let file = File::open(self.path(&pointer.oid))?;
        let len = file.metadata()?.len();
        if len != pointer.size {
            return Err(io::Error::other(format!(
                "LFS object {} in the cache is {len} bytes, not {}",
                pointer.oid, pointer.size
            )));
        }
        Ok(file)
    }

    /// Adds the content of the file at `src` (a frozen copy, which nothing
    /// changes) and returns its pointer. An object already cached is kept.
    pub fn insert_file(&self, src: &Path) -> io::Result<Pointer> {
        let pointer = Pointer::of_reader(File::open(src)?)?;
        if self.contains(&pointer) {
            return Ok(pointer);
        }
        let tmp = self.tmp_path(&pointer.oid);
        if let Err(e) = crate::pending::copy_into(src, &tmp) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        if fs::metadata(&tmp)?.len() != pointer.size {
            let _ = fs::remove_file(&tmp);
            return Err(io::Error::other(format!(
                "{} changed while it was cached",
                src.display()
            )));
        }
        self.publish(&tmp, &pointer.oid)?;
        Ok(pointer)
    }

    /// Adds an object read from `reader` (a download), hashing it as it is
    /// written: it is kept only if its SHA-256 and size are the pointer's.
    pub fn insert_verified(
        &self,
        pointer: &Pointer,
        reader: &mut dyn Read,
    ) -> Result<(), LfsError> {
        let local = |what: &str, e: io::Error| LfsError::local(format!("{what}: {e}"));
        let tmp = self.tmp_path(&pointer.oid);
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| local("writing to the LFS cache", e))?;
            let mut hashing = Hashing::new();
            let mut buf = vec![0u8; 64 << 10];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        return Err(LfsError::new(
                            PushFailureKind::Unreachable,
                            format!("downloading LFS object {}: {e}", pointer.oid),
                        ));
                    }
                };
                if hashing.size() + n as u64 > pointer.size {
                    return Err(mismatch(pointer, "more bytes than its size"));
                }
                hashing.update(&buf[..n]);
                file.write_all(&buf[..n])
                    .map_err(|e| local("writing to the LFS cache", e))?;
            }
            let got = hashing.finish();
            if got != *pointer {
                return Err(mismatch(
                    pointer,
                    &format!("{} bytes with oid {}", got.size, got.oid),
                ));
            }
            file.sync_all()
                .map_err(|e| local("writing to the LFS cache", e))?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        self.publish(&tmp, &pointer.oid)
            .map_err(|e| local("writing to the LFS cache", e))
    }

    fn tmp_path(&self, oid: &LfsOid) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        self.tmp.join(format!("{oid}-{}-{n}", std::process::id()))
    }

    /// Renames `tmp` into place as `oid` and makes it durable.
    fn publish(&self, tmp: &Path, oid: &LfsOid) -> io::Result<()> {
        let result = publish_into(&self.objects, tmp, oid);
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result
    }
}

/// A resumable download in `incomplete/<oid>`, locked by its holder
/// ([`ObjectCache::partial`]).
pub struct Partial {
    lock: FileLock,
    path: PathBuf,
    pointer: Pointer,
    hashing: Hashing,
}

impl Partial {
    /// How many bytes it holds: where the download resumes.
    pub fn offset(&self) -> u64 {
        self.hashing.size()
    }

    /// Hashes what the file holds; one longer than the object cannot be
    /// the object's start, so it is emptied.
    fn rehash(&mut self) -> io::Result<()> {
        self.hashing = Hashing::new();
        let len = self.lock.file().metadata()?.len();
        if len > self.pointer.size {
            return self.restart();
        }
        self.lock.file_mut().seek(SeekFrom::Start(0))?;
        let mut buf = vec![0u8; 64 << 10];
        let mut left = len;
        while left > 0 {
            let want = buf.len().min(left as usize);
            let n = self.lock.file_mut().read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            self.hashing.update(&buf[..n]);
            left -= n as u64;
        }
        // Whatever could not be read back is dropped.
        let held = self.hashing.size();
        self.lock.file().set_len(held)?;
        self.lock.file_mut().seek(SeekFrom::Start(held))?;
        Ok(())
    }

    /// Empties it, for a download that starts again from the first byte.
    pub fn restart(&mut self) -> io::Result<()> {
        self.lock.file().set_len(0)?;
        self.lock.file_mut().seek(SeekFrom::Start(0))?;
        self.hashing = Hashing::new();
        Ok(())
    }

    /// Appends everything `reader` gives, hashing it. A read error is
    /// `unreachable` and keeps what arrived, for the next attempt; more
    /// bytes than the object's size discard the partial (`refused`).
    pub fn append(&mut self, reader: &mut dyn Read) -> Result<(), LfsError> {
        let oid = self.pointer.oid;
        let mut buf = vec![0u8; 64 << 10];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(LfsError::new(
                        PushFailureKind::Unreachable,
                        format!(
                            "downloading LFS object {oid}: {e} (after {} of {} bytes; the \
                             next attempt resumes)",
                            self.offset(),
                            self.pointer.size
                        ),
                    ));
                }
            };
            if self.offset() + n as u64 > self.pointer.size {
                self.discard();
                return Err(mismatch(&self.pointer, "more bytes than its size"));
            }
            self.lock.file_mut().write_all(&buf[..n]).map_err(|e| {
                LfsError::local(format!("writing LFS object {oid} to the cache: {e}"))
            })?;
            self.hashing.update(&buf[..n]);
        }
    }

    /// Checks the whole object and moves it into the cache. Short: the
    /// partial is kept (`unreachable`, to resume); the wrong content: it
    /// is deleted (`refused`).
    pub fn finish(mut self, cache: &ObjectCache) -> Result<(), LfsError> {
        let pointer = self.pointer;
        if self.offset() < pointer.size {
            return Err(LfsError::new(
                PushFailureKind::Unreachable,
                format!(
                    "the download of LFS object {} ended after {} of {} bytes; the next \
                     attempt resumes",
                    pointer.oid,
                    self.offset(),
                    pointer.size
                ),
            ));
        }
        let got = std::mem::replace(&mut self.hashing, Hashing::new()).finish();
        if got != pointer {
            self.discard();
            return Err(mismatch(
                &pointer,
                &format!("{} bytes with oid {}", got.size, got.oid),
            ));
        }
        let local = |e: io::Error| LfsError::local(format!("writing to the LFS cache: {e}"));
        self.lock.file().sync_all().map_err(local)?;
        // Renamed while still locked: a waiter then finds the object cached.
        publish_into(&cache.objects, &self.path, &pointer.oid).map_err(local)
    }

    /// Deletes the partial (while still holding its lock).
    fn discard(&mut self) {
        let _ = fs::remove_file(&self.path);
        self.hashing = Hashing::new();
    }
}

fn mismatch(pointer: &Pointer, got: &str) -> LfsError {
    LfsError::new(
        PushFailureKind::Refused,
        format!(
            "LFS object {} ({} bytes) arrived as {got}; it was not kept",
            pointer.oid, pointer.size
        ),
    )
}

/// `<objects>/aa/bb/<oid>`, git-lfs's layout.
pub(crate) fn object_path(objects: &Path, oid: &LfsOid) -> PathBuf {
    let hex = oid.to_string();
    objects.join(&hex[0..2]).join(&hex[2..4]).join(hex)
}

/// Renames the complete, fsynced file `tmp` to `<objects>/aa/bb/<oid>`,
/// creating and fsyncing the directories as needed.
pub(crate) fn publish_into(objects: &Path, tmp: &Path, oid: &LfsOid) -> io::Result<()> {
    let dest = object_path(objects, oid);
    let bb = dest.parent().expect("an object path has a parent");
    let aa = bb.parent().expect("an object path has a grandparent");
    create_dir(aa)?;
    create_dir(bb)?;
    fs::rename(tmp, &dest)?;
    // Every level, not only the ones this call created: a crash after an
    // earlier call created a directory and before it was synced leaves
    // an entry no later call would make durable.
    File::open(bb)?.sync_all()?;
    File::open(aa)?.sync_all()?;
    File::open(objects)?.sync_all()?;
    Ok(())
}

/// The entries of `dir`, sorted; none if it is missing.
fn read_dir_sorted(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                out.push(entry?.path());
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    out.sort();
    Ok(out)
}

/// Creates `dir` if it is missing; whether it was created.
fn create_dir(dir: &Path) -> io::Result<bool> {
    match fs::create_dir(dir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> (tempfile::TempDir, ObjectCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = ObjectCache::open(dir.path().join("lfs")).unwrap();
        (dir, cache)
    }

    /// A partial download let go is free at once, though another thread
    /// keeps spawning processes that share its file until they exec.
    #[test]
    fn a_partial_let_go_is_free_while_children_spawn() {
        let (_dir, cache) = cache();
        let pointer = Pointer {
            oid: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
                .parse()
                .unwrap(),
            size: 5,
        };
        let (busy, takes) = crate::file_lock::takes_while_children_spawn(|| {
            drop(cache.partial(&pointer).unwrap().unwrap());
            cache.remove_free_partials().unwrap().0 == 1
        });
        assert_eq!(busy, 0, "busy with no holder, of {takes} takes");
    }

    #[test]
    fn insert_is_idempotent_and_uses_git_lfs_layout() {
        let (dir, cache) = cache();
        let src = dir.path().join("src");
        fs::write(&src, b"hello").unwrap();
        let p = cache.insert_file(&src).unwrap();
        let hex = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(p.oid.to_string(), hex);
        assert_eq!(p.size, 5);
        let path = dir.path().join(format!("lfs/objects/2c/f2/{hex}"));
        assert_eq!(cache.path(&p.oid), path);
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        assert!(cache.contains(&p));
        // Again: the same pointer, nothing new.
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(cache.insert_file(&src).unwrap(), p);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        assert_eq!(fs::read_dir(dir.path().join("lfs/tmp")).unwrap().count(), 0);
        let mut s = String::new();
        cache
            .open_object(&p)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "hello");
        // A pointer with another size is not this object.
        let wrong = Pointer { size: 6, ..p };
        assert!(!cache.contains(&wrong));
        assert!(cache.open_object(&wrong).is_err());
    }

    #[test]
    fn a_truncated_object_is_replaced() {
        let (dir, cache) = cache();
        let src = dir.path().join("src");
        fs::write(&src, b"hello").unwrap();
        let p = Pointer::of_reader(&b"hello"[..]).unwrap();
        let path = cache.path(&p.oid);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"hel").unwrap();
        assert!(!cache.contains(&p));
        cache.insert_file(&src).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn leftovers_in_tmp_are_removed_at_open() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("lfs/tmp/sub")).unwrap();
        fs::write(dir.path().join("lfs/tmp/partial"), b"x").unwrap();
        ObjectCache::open(dir.path().join("lfs")).unwrap();
        assert_eq!(fs::read_dir(dir.path().join("lfs/tmp")).unwrap().count(), 0);
    }

    #[test]
    fn verified_inserts_refuse_wrong_content() {
        let (dir, cache) = cache();
        let p = Pointer::of_reader(&b"payload"[..]).unwrap();
        for bad in [&b"payloax"[..], b"payloa", b"payload!"] {
            let err = cache.insert_verified(&p, &mut &bad[..]).unwrap_err();
            assert_eq!(err.kind, PushFailureKind::Refused, "{err}");
            assert!(!cache.contains(&p));
            assert_eq!(fs::read_dir(dir.path().join("lfs/tmp")).unwrap().count(), 0);
        }
        cache.insert_verified(&p, &mut &b"payload"[..]).unwrap();
        assert_eq!(fs::read(cache.path(&p.oid)).unwrap(), b"payload");
    }

    #[test]
    fn a_large_file_hashes_like_sha256sum() {
        let (dir, cache) = cache();
        let src = dir.path().join("big");
        let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &content).unwrap();
        let p = cache.insert_file(&src).unwrap();
        assert_eq!(p.oid, LfsOid::of(&content));
        assert_eq!(p.size, content.len() as u64);
        if let Ok(out) = std::process::Command::new("sha256sum").arg(&src).output() {
            let hex = String::from_utf8_lossy(&out.stdout);
            assert!(hex.starts_with(&p.oid.to_string()), "{hex}");
        }
    }
}
