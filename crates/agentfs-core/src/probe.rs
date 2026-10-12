//! Reads small files the workload controls from the backing tree (design §4.1,
//! §5): `.gitignore`, `.gitattributes`, `.git` files and git-directory
//! markers such as `HEAD`.
//!
//! These reads happen in the synchronous step of a record, so they must not
//! block or grow with what the workload writes. Only regular files are read,
//! never through a symlink: the file is opened with `O_NOFOLLOW` and
//! `O_NONBLOCK` (so a FIFO swapped in after the type check cannot hang the
//! caller) and its type is checked again on the open handle. At most `limit`
//! bytes are read; a larger file is treated as absent, as git ignores
//! oversized gitfiles and attribute files.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use rustix::fs::{FileType, Mode, OFlags};
use rustix::io::Errno;

/// Bound for files whose first line is all that matters (`.git` files,
/// `HEAD`, `description`).
pub const SMALL: u64 = 4 * 1024;

/// Bound for tree rule files (`.gitignore`, `.gitattributes`).
pub const RULE_FILE: u64 = 1024 * 1024;

/// The contents of `path` if it is a regular file of at most `limit` bytes.
///
/// `None` if it is missing, under a non-directory, a symlink, not a regular
/// file, or larger than `limit`. Other errors are returned.
pub fn read_regular(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
    // Checking first avoids even opening devices, sockets and FIFOs.
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(None),
        Err(e) if is_absent(&e) => return Ok(None),
        Err(e) => return Err(e),
    }
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = match rustix::fs::open(path, flags | OFlags::NOCTTY, Mode::empty()) {
        Ok(fd) => fd,
        // Replaced by a symlink since the check.
        Err(Errno::LOOP) => return Ok(None),
        Err(e) => {
            let e = io::Error::from(e);
            return if is_absent(&e) { Ok(None) } else { Err(e) };
        }
    };
    let stat = rustix::fs::fstat(&fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Ok(None);
    }
    let mut content = Vec::new();
    File::from(fd).take(limit + 1).read_to_end(&mut content)?;
    if content.len() as u64 > limit {
        return Ok(None);
    }
    Ok(Some(content))
}

/// True for errors meaning "nothing there": a missing path, or a path
/// through something that is not a directory.
pub fn is_absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn reads_regular_files_within_the_limit() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f");
        fs::write(&f, b"12345").unwrap();
        assert_eq!(read_regular(&f, 5).unwrap().as_deref(), Some(&b"12345"[..]));
        assert_eq!(read_regular(&f, 4).unwrap(), None, "too large");
        assert_eq!(read_regular(&t.path().join("missing"), 5).unwrap(), None);
        assert_eq!(read_regular(&f.join("under-a-file"), 5).unwrap(), None);
        assert_eq!(read_regular(t.path(), 5).unwrap(), None, "a directory");
    }

    #[test]
    fn never_follows_symlinks() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("target"), b"x").unwrap();
        symlink("target", t.path().join("link")).unwrap();
        assert_eq!(read_regular(&t.path().join("link"), 5).unwrap(), None);
    }

    #[test]
    fn a_fifo_does_not_block() {
        let t = tempfile::tempdir().unwrap();
        let fifo = t.path().join("fifo");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        assert_eq!(read_regular(&fifo, 5).unwrap(), None);
    }
}
