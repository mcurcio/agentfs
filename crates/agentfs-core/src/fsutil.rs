//! Small filesystem helpers shared by the state files under `<state>/meta`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt;
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
    crate::failpoints::hit("state.json.before_rename")?;
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()
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
