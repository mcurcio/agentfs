//! Small filesystem helpers shared by the state files under `<state>/meta`
//! and the tree engine's state area: durable directories, atomic writes,
//! and a file's bytes reflinked or copied.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use serde::Serialize;

/// Writes `value` as pretty JSON to `path` atomically: a temporary file
/// (mode 0600), fsync, rename, fsync of the directory.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&json)?;
        file.sync_all()?;
    }
    agentfs_failpoints::hit("state.json.before_rename")?;
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()
}

/// Fsyncs the directory `dir`, making the names in it durable.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Makes `dir` and its missing parents (each mode 0700), and fsyncs the
/// parent of each one made, so their names survive a power cut.
pub fn create_dir_durably(dir: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut at = dir;
    while !at.exists() {
        missing.push(at);
        match at.parent() {
            Some(parent) => at = parent,
            None => break,
        }
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    for made in missing.iter().rev() {
        if let Some(parent) = made.parent() {
            sync_dir(parent)?;
        }
    }
    Ok(())
}

/// Gives the empty file `to` the bytes of `from`: a reflink (`FICLONE`)
/// where the filesystem can, else a copy. Whether it was a reflink; neither
/// file is fsynced.
pub fn clone_or_copy(from: &impl AsFd, to: &impl AsFd) -> io::Result<bool> {
    if rustix::fs::ioctl_ficlone(to, from).is_ok() {
        return Ok(true);
    }
    let mut from = File::from(from.as_fd().try_clone_to_owned()?);
    let mut to = File::from(to.as_fd().try_clone_to_owned()?);
    io::copy(&mut from, &mut to)?;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_json_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        write_json_atomic(&path, &vec![1, 2]).unwrap();
        write_json_atomic(&path, &vec![3]).unwrap();
        let back: Vec<u32> = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(back, vec![3]);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!path.with_extension("json.tmp").exists());
    }
}
