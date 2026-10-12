//! Files re-read when they change (design §11.1): token credentials, SSH
//! keys and known_hosts files.
//!
//! A [`WatchedFile`] keeps the parsed content with the file's stat stamp
//! (device, inode, size, mtime, ctime) and parses the file again only when
//! the stamp changes, so whatever rotates the file only has to rewrite or
//! replace it. [`std::fs::metadata`] follows symlinks, so kubelet's atomic
//! `..data` symlink swap for Secret volumes and a rename over the file are
//! both seen. An in-place rewrite of the same size within the filesystem's
//! timestamp granularity would be missed.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What identifies one version of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime_ns: i128,
    ctime_ns: i128,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
            ctime_ns: i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
        }
    }
}

/// Why [`WatchedFile::load`] failed.
#[derive(Debug)]
pub(crate) enum WatchedError<E> {
    /// The file could not be stat'ed or read, or is larger than the cap.
    Io(io::Error),
    /// The parser refused the content.
    Parse(E),
}

/// A file parsed into `T`, cached until its stamp changes.
pub(crate) struct WatchedFile<T> {
    path: PathBuf,
    max_len: u64,
    cache: Mutex<Option<(Stamp, T)>>,
    reloads: AtomicUsize,
}

impl<T: Clone> WatchedFile<T> {
    /// The file at `path`, read only up to `max_len` bytes: a larger file is
    /// an IO error.
    pub(crate) fn new(path: impl Into<PathBuf>, max_len: u64) -> Self {
        Self {
            path: path.into(),
            max_len,
            cache: Mutex::new(None),
            reloads: AtomicUsize::new(0),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// How many times the file has been read and handed to a parser.
    pub(crate) fn reloads(&self) -> usize {
        self.reloads.load(Ordering::Relaxed)
    }

    /// The parsed content: from the cache while the stamp is unchanged,
    /// else read and parsed again with `parse`. A parse error is not
    /// cached, so the next call reads the file again.
    pub(crate) fn load<E>(
        &self,
        parse: impl FnOnce(&[u8]) -> Result<T, E>,
    ) -> Result<T, WatchedError<E>> {
        let stamp = Stamp::of(&std::fs::metadata(&self.path).map_err(WatchedError::Io)?);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached, parsed)) = cache.as_ref()
            && *cached == stamp
        {
            return Ok(parsed.clone());
        }
        // Read after the stat: a change in between is caught by the next
        // stat, which no longer matches the cached stamp.
        let mut content = Vec::new();
        File::open(&self.path)
            .and_then(|f| f.take(self.max_len + 1).read_to_end(&mut content))
            .map_err(WatchedError::Io)?;
        if content.len() as u64 > self.max_len {
            return Err(WatchedError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("larger than {} bytes", self.max_len),
            )));
        }
        self.reloads.fetch_add(1, Ordering::Relaxed);
        let parsed = parse(&content).map_err(WatchedError::Parse)?;
        *cache = Some((stamp, parsed.clone()));
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn text(bytes: &[u8]) -> Result<String, String> {
        String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())
    }

    #[test]
    fn caches_until_the_stamp_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "one").unwrap();
        let w = WatchedFile::<String>::new(&path, 16);
        assert_eq!(w.load(text).unwrap(), "one");
        assert_eq!(w.load(text).unwrap(), "one");
        assert_eq!(w.reloads(), 1);
        fs::write(&path, "three").unwrap();
        assert_eq!(w.load(text).unwrap(), "three");
        assert_eq!(w.reloads(), 2);
        assert_eq!(w.path(), path);
    }

    #[test]
    fn errors_are_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let w = WatchedFile::<String>::new(&path, 4);
        assert!(matches!(w.load(text), Err(WatchedError::Io(_))));
        fs::write(&path, "toolong").unwrap();
        assert!(matches!(w.load(text), Err(WatchedError::Io(_))));
        assert_eq!(w.reloads(), 0, "an oversized file is never parsed");
        fs::write(&path, b"\xff").unwrap();
        assert!(matches!(w.load(text), Err(WatchedError::Parse(_))));
        assert!(matches!(w.load(text), Err(WatchedError::Parse(_))));
        assert_eq!(w.reloads(), 2, "a parse error is read again");
        fs::write(&path, "ok").unwrap();
        assert_eq!(w.load(text).unwrap(), "ok");
    }
}
