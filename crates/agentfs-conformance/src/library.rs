//! The shared calls on the real library: `Volume`, in this process.

use agentfs_core::tree_actor::{self, Error, Volume};
use agentfs_model::calls::{CallError, Caller, Took, TreeCalls, TreeId};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The model's first commit: files a, b, c and d, each six `-` lines.
pub const SEEDED: [&str; 4] = ["a", "b", "c", "d"];
const SEED_BYTES: &[u8] = b"-\n-\n-\n-\n-\n-\n";

/// Makes the model's first commit in a new volume. Each file is created under
/// another name and renamed, because a file just created is what an atomic save
/// renames (the model's a to d never are: `fresh`, git-model.md §7).
pub fn seed(volume: &mut Volume) -> Result<(), Error> {
    for path in SEEDED {
        let new = format!("{path}.new");
        let h = volume.write_open("main", &new, "agentfs-seed", true)?;
        volume.write(h, 0, SEED_BYTES, false)?;
        volume.close(h)?;
        volume.rename("main", &new, path, "agentfs-seed")?;
    }
    Ok(())
}

pub(crate) fn fail(e: Error) -> CallError {
    CallError {
        errno: Some(e.errno()),
        message: e.to_string(),
    }
}

/// `Volume` and its state directory.
pub struct Library {
    volume: Option<Volume>,
    dir: PathBuf,
    forks: Vec<String>,
    owners: BTreeMap<u64, Caller>,
}

impl Library {
    /// A new volume at `dir`, seeded.
    pub fn new(dir: &Path) -> Result<Self, CallError> {
        let mut volume = Volume::open(dir).map_err(fail)?;
        seed(&mut volume).map_err(fail)?;
        Ok(Self {
            volume: Some(volume),
            dir: dir.to_owned(),
            forks: Vec::new(),
            owners: BTreeMap::new(),
        })
    }

    /// The bare repository the invariants read.
    pub fn repo(&self) -> PathBuf {
        self.dir.join("history.git")
    }

    fn v(&self) -> Result<&Volume, CallError> {
        self.volume
            .as_ref()
            .ok_or_else(|| CallError::new("the volume is down"))
    }

    fn v_mut(&mut self) -> Result<&mut Volume, CallError> {
        self.volume
            .as_mut()
            .ok_or_else(|| CallError::new("the volume is down"))
    }

    fn tree(&self, t: TreeId) -> Result<&str, CallError> {
        match t {
            TreeId::Main => Ok("main"),
            TreeId::Fork(f) => self
                .forks
                .get(f.wrapping_sub(1))
                .map(String::as_str)
                .ok_or_else(|| CallError::new(format!("no fork {f}"))),
        }
    }

    /// The tree and writer label of a caller, owned: calls take `&mut self`.
    fn who(&self, c: &Caller) -> Result<(String, String), CallError> {
        Ok(match c {
            Caller::Main(n) => ("main".to_owned(), n.clone()),
            Caller::Fork(f) => (self.tree(TreeId::Fork(*f))?.to_owned(), format!("c{f}")),
        })
    }

    fn open(&mut self, c: &Caller, path: &str, create: bool) -> Result<u64, CallError> {
        let (tree, writer) = self.who(c)?;
        let h = self
            .v_mut()?
            .write_open(&tree, path, &writer, create)
            .map_err(fail)?;
        self.owners.insert(h, c.clone());
        Ok(h)
    }
}

impl TreeCalls for Library {
    type Open = u64;

    fn fork(&mut self) -> Result<usize, CallError> {
        let name = self.v_mut()?.fork().map_err(fail)?;
        self.forks.push(name);
        Ok(self.forks.len())
    }

    fn open_write(&mut self, c: &Caller, path: &str) -> Result<u64, CallError> {
        self.open(c, path, false)
    }

    fn open_trunc(&mut self, c: &Caller, path: &str) -> Result<u64, CallError> {
        let h = self.open(c, path, false)?;
        self.v_mut()?.truncate(h, 0).map_err(fail)?;
        Ok(h)
    }

    /// `O_CREAT | O_EXCL`, as the view makes it.
    fn create(&mut self, c: &Caller, path: &str) -> Result<u64, CallError> {
        let (tree, _) = self.who(c)?;
        if self.v()?.read(&tree, path).is_ok() {
            return Err(fail(Error::Exists));
        }
        self.open(c, path, true)
    }

    fn pwrite(&mut self, h: u64, offset: u64, bytes: &[u8]) -> Result<(), CallError> {
        self.v_mut()?.write(h, offset, bytes, false).map_err(fail)
    }

    fn append(&mut self, h: u64, bytes: &[u8]) -> Result<(), CallError> {
        self.v_mut()?.write(h, 0, bytes, true).map_err(fail)
    }

    fn truncate(&mut self, h: u64, size: u64) -> Result<(), CallError> {
        self.v_mut()?.truncate(h, size).map_err(fail)
    }

    fn close(&mut self, h: u64) -> Result<(), CallError> {
        self.owners.remove(&h);
        self.v_mut()?.close(h).map(|_| ()).map_err(fail)
    }

    fn unlink(&mut self, c: &Caller, path: &str) -> Result<(), CallError> {
        let (tree, writer) = self.who(c)?;
        self.v_mut()?.unlink(&tree, path, &writer).map_err(fail)
    }

    fn rename(&mut self, c: &Caller, from: &str, to: &str) -> Result<(), CallError> {
        let (tree, writer) = self.who(c)?;
        self.v_mut()?.rename(&tree, from, to, &writer).map_err(fail)
    }

    fn end_client(&mut self, c: &Caller) -> Result<(), CallError> {
        let mine: Vec<u64> = self
            .owners
            .iter()
            .filter(|(_, o)| *o == c)
            .map(|(h, _)| *h)
            .collect();
        for h in mine {
            TreeCalls::close(self, h)?;
        }
        Ok(())
    }

    fn take_next(&mut self, t: TreeId) -> Result<Took, CallError> {
        let tree = self.tree(t)?.to_owned();
        Ok(match self.v_mut()?.take_next(&tree).map_err(fail)? {
            tree_actor::Took::Taken => Took::Taken,
            tree_actor::Took::Waiting => Took::Waiting,
            tree_actor::Took::Empty => Took::Empty,
        })
    }

    fn finish_take(&mut self, _t: TreeId) -> Result<(), CallError> {
        Ok(())
    }

    /// Drop the volume and open it again: recovery closes every open once.
    fn crash(&mut self) -> Result<(), CallError> {
        self.volume = None;
        self.owners.clear();
        self.volume = Some(Volume::open(&self.dir).map_err(fail)?);
        Ok(())
    }

    fn read(&self, t: TreeId, path: &str) -> Result<Option<Vec<u8>>, CallError> {
        match self.v()?.read(self.tree(t)?, path) {
            Ok(b) => Ok(Some(b.to_vec())),
            Err(Error::NotFound) => Ok(None),
            Err(e) => Err(fail(e)),
        }
    }

    fn view(&self, h: u64) -> Result<Vec<u8>, CallError> {
        self.v()?.read_open(h).map(<[u8]>::to_vec).map_err(fail)
    }

    fn client_read(&self, c: &Caller, path: &str) -> Result<Option<Vec<u8>>, CallError> {
        let t = match c {
            Caller::Main(_) => TreeId::Main,
            Caller::Fork(f) => TreeId::Fork(*f),
        };
        self.read(t, path)
    }

    fn paths(&self, t: TreeId) -> Result<Vec<String>, CallError> {
        let mut paths: Vec<String> = self
            .v()?
            .files(self.tree(t)?)
            .map_err(fail)?
            .into_iter()
            .map(|(p, _, _)| p)
            .collect();
        paths.sort();
        Ok(paths)
    }
}
