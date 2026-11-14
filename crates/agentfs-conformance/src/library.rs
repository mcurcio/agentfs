//! The shared calls on the real library: `TreeActor`, in this process.

use crate::engine_names::EngineNames;
use agentfs_contracts::name::{VolumeName, WriterLabel};
use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::volume::{VolumeId, VolumeIdentity};
use agentfs_core::tree_actor::{self, Error, TreeActor, TreeId as EngineTree};
use agentfs_model::calls::{CallError, Caller, Took, TreeCalls, TreeId};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The model's first commit: files a, b, c and d, each six `-` lines.
pub const SEEDED: [&str; 4] = ["a", "b", "c", "d"];
const SEED_BYTES: &[u8] = b"-\n-\n-\n-\n-\n-\n";

/// Makes the model's first commit in a new volume, in its root `root`. Each file
/// is created under another name and renamed, because a file just created is what
/// an atomic save renames (the model's a to d never are: `fresh`, git-model.md §7).
pub fn seed(volume: &mut TreeActor, root: EngineTree) -> Result<(), Error> {
    let by = writer("agentfs-seed")?;
    for path in SEEDED {
        let new = format!("{path}.new");
        let h = volume.write_open(root, &new, &by, true)?;
        volume.write(h, 0, SEED_BYTES, false)?;
        volume.close(h)?;
        volume.rename(root, &new, path, &by)?;
    }
    Ok(())
}

/// A replay's volume: `conformance`, with a fixed id. Its commits carry no pod
/// context: the model has none.
pub fn identity() -> VolumeIdentity {
    VolumeIdentity {
        name: VolumeName::parse("conformance").expect("a safe name"),
        id: VolumeId(uuid::Uuid::from_u128(1)),
    }
}

/// A new volume at `dir`, served with no pod context.
pub fn create(dir: &Path) -> Result<TreeActor, Error> {
    TreeActor::create(dir, &identity(), &PodContext::default())
}

/// `label` as a writer with no pod context: the model's labels (`c3`) as they are.
pub(crate) fn writer(label: &str) -> Result<WriterAt, Error> {
    Ok(WriterAt {
        writer: WriterLabel::parse(label).map_err(|_| Error::Name)?,
        at: PodContext::default(),
    })
}

pub(crate) fn fail(e: Error) -> CallError {
    CallError {
        errno: Some(e.errno()),
        message: e.to_string(),
    }
}

/// `TreeActor` and its state directory.
pub struct Library {
    volume: Option<TreeActor>,
    dir: PathBuf,
    names: EngineNames,
    owners: BTreeMap<u64, Caller>,
}

impl Library {
    /// A new volume at `dir`, seeded, its trees named by `names`.
    pub fn new(dir: &Path, names: EngineNames) -> Result<Self, CallError> {
        let mut volume = create(dir).map_err(fail)?;
        seed(&mut volume, names.name(TreeId::ROOT)?).map_err(fail)?;
        volume.settle().map_err(fail)?;
        Ok(Self {
            volume: Some(volume),
            dir: dir.to_owned(),
            names,
            owners: BTreeMap::new(),
        })
    }

    /// The bare repository the invariants read.
    pub fn repo(&self) -> PathBuf {
        self.dir.join("history.git")
    }

    fn v(&self) -> Result<&TreeActor, CallError> {
        self.volume
            .as_ref()
            .ok_or_else(|| CallError::new("the volume is down"))
    }

    fn v_mut(&mut self) -> Result<&mut TreeActor, CallError> {
        self.volume
            .as_mut()
            .ok_or_else(|| CallError::new("the volume is down"))
    }

    /// `result`, once history is built through every record so far: the
    /// invariants read git after each call, and the builder makes history
    /// behind the call path.
    fn settled<T>(&self, result: Result<T, CallError>) -> Result<T, CallError> {
        let value = result?;
        self.v()?.settle().map_err(fail)?;
        Ok(value)
    }

    /// The tree and writer label of a caller, owned: calls take `&mut self`.
    fn who(&self, c: &Caller) -> Result<(EngineTree, WriterAt), CallError> {
        let (tree, label) = match c {
            Caller::Root(n) => (self.names.name(TreeId::ROOT)?, n.clone()),
            Caller::Fork(f) => (self.names.name(*f)?, format!("c{f}")),
        };
        Ok((tree, writer(&label).map_err(fail)?))
    }

    fn open(&mut self, c: &Caller, path: &str, create: bool) -> Result<u64, CallError> {
        let (tree, writer) = self.who(c)?;
        let h = self
            .v_mut()?
            .write_open(tree, path, &writer, create)
            .map_err(fail);
        let h = self.settled(h)?;
        self.owners.insert(h, c.clone());
        Ok(h)
    }
}

impl TreeCalls for Library {
    type Open = u64;

    fn fork(&mut self) -> Result<TreeId, CallError> {
        let name = self.v_mut()?.fork().map_err(fail);
        let name = self.settled(name)?;
        self.names.forked(name)
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
        if self.v()?.read(tree, path).is_ok() {
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
        let closed = self.v_mut()?.close(h).map(|_| ()).map_err(fail);
        self.settled(closed)
    }

    fn unlink(&mut self, c: &Caller, path: &str) -> Result<(), CallError> {
        let (tree, writer) = self.who(c)?;
        let unlinked = self.v_mut()?.unlink(tree, path, &writer).map_err(fail);
        self.settled(unlinked)
    }

    fn rename(&mut self, c: &Caller, from: &str, to: &str) -> Result<(), CallError> {
        let (tree, writer) = self.who(c)?;
        let renamed = self.v_mut()?.rename(tree, from, to, &writer).map_err(fail);
        self.settled(renamed)
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
        let tree = self.names.name(t)?;
        let took = self.v_mut()?.take_next(tree).map_err(fail);
        Ok(match self.settled(took)? {
            tree_actor::Took::Taken => Took::Taken,
            tree_actor::Took::Waiting => Took::Waiting,
            tree_actor::Took::Empty => Took::Empty,
        })
    }

    /// Drop the volume and open it again: recovery closes every open once.
    fn crash(&mut self) -> Result<(), CallError> {
        self.volume = None;
        self.owners.clear();
        self.volume = Some(TreeActor::open(&self.dir, &PodContext::default()).map_err(fail)?);
        Ok(())
    }

    fn read(&self, t: TreeId, path: &str) -> Result<Option<Vec<u8>>, CallError> {
        match self.v()?.read(self.names.name(t)?, path) {
            Ok(b) => Ok(Some(b.to_vec())),
            Err(Error::NotFound) => Ok(None),
            Err(e) => Err(fail(e)),
        }
    }

    fn paths(&self, t: TreeId) -> Result<Vec<String>, CallError> {
        let mut paths: Vec<String> = self
            .v()?
            .files(self.names.name(t)?)
            .map_err(fail)?
            .into_iter()
            .map(|(p, _, _)| p)
            .collect();
        paths.sort();
        Ok(paths)
    }
}
