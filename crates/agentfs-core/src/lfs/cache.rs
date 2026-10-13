//! The local LFS object cache (design §2.2, §7): `<state>/lfs/objects/aa/bb/<oid>`,
//! the layout git-lfs uses in `.git/lfs/objects`, plus `<state>/lfs/tmp`
//! for objects being written.
//!
//! Objects are written to `tmp/`, fsynced, renamed into place and their
//! directories fsynced, so an object that exists is whole and durable.
//! Commits insert frozen copies from `pending/` (a reflink where the
//! filesystem allows, as for freezing); restores insert downloads, hashed
//! while they are written and refused unless they match. Nothing is ever
//! pruned (design §7).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::LfsError;
use super::pointer::{Hashing, LfsOid, Pointer};
use crate::events::PushFailureKind;

/// The object cache of one state area.
#[derive(Debug)]
pub struct ObjectCache {
    objects: PathBuf,
    tmp: PathBuf,
}

impl ObjectCache {
    /// Opens (creating if needed) the cache at `dir` (`<state>/lfs`),
    /// removing whatever a crash left in `tmp/`.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        let objects = dir.join("objects");
        let tmp = dir.join("tmp");
        fs::create_dir_all(&objects)?;
        fs::create_dir_all(&tmp)?;
        for entry in fs::read_dir(&tmp)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(Self { objects, tmp })
    }

    /// The cache at `dir` as another user of the same state area sees it
    /// (a pusher, a restore): created if missing, but `tmp/` is left alone,
    /// since the committer may be writing there.
    pub fn at(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        let objects = dir.join("objects");
        let tmp = dir.join("tmp");
        fs::create_dir_all(&objects)?;
        fs::create_dir_all(&tmp)?;
        Ok(Self { objects, tmp })
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
