//! The shared calls on the real library: `TreeActor`, in this process.

use crate::compact_turns::Turns;
use crate::engine_names::EngineNames;
use agentfs_contracts::clients::FileOwner;
use agentfs_contracts::name::{VolumeName, WriterLabel};
use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::volume::{VolumeId, VolumeIdentity};
use agentfs_core::tree_actor::{
    self, Attrs, Error, Kind as EngineKind, Perm, RenameMode, TreeActor, TreeId as EngineTree,
    Upkeep,
};
use agentfs_model::calls::{CallError, Caller, Kind, Meta, Took, TreeCalls, TreeId};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a replay's create gives a file: the model's created file's metadata
/// ([`Meta::FILE`]), mode 0644, owned by 0:0.
const ATTRS: Attrs = attrs(&Meta::FILE);

/// The mode and owner of the model's metadata `m`, as the library takes them.
pub(crate) const fn attrs(m: &Meta) -> Attrs {
    Attrs {
        perm: Perm::from_mode(m.mode),
        owner: FileOwner {
            uid: m.uid,
            gid: m.gid,
        },
    }
}

/// The library's metadata of an entry in the model's terms.
pub(crate) fn model_meta(m: &tree_actor::Meta) -> Meta {
    Meta {
        kind: match m.kind {
            EngineKind::File => Kind::File,
            EngineKind::Symlink => Kind::Symlink,
            EngineKind::Dir => Kind::Dir,
        },
        mode: m.mode.bits(),
        uid: m.uid,
        gid: m.gid,
    }
}

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
        let h = volume.write_open(root, &new, &by, Some(ATTRS))?;
        volume.write(h, 0, SEED_BYTES, false)?;
        volume.close(h)?;
        volume.rename(root, &new, path, RenameMode::Replace, &by)?;
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

/// A new volume at `dir`, served with no pod context and `upkeep`.
pub fn create(dir: &Path, upkeep: Upkeep) -> Result<TreeActor, Error> {
    TreeActor::create_with(dir, &identity(), None, &PodContext::default(), upkeep)
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

/// `TreeActor` and its state directory. The volume compacts at every built
/// event, and its compactor is parked at each gap in turn while a replayed
/// call runs ([`Turns`]).
pub struct Library {
    volume: Option<TreeActor>,
    /// The open volume's compaction gate, ended before the volume is dropped.
    turns: Arc<Turns>,
    dir: PathBuf,
    names: EngineNames,
    owners: BTreeMap<u64, Caller>,
}

impl Drop for Library {
    fn drop(&mut self) {
        // Before the volume's shutdown joins its compactor.
        self.turns.end();
    }
}

impl Library {
    /// A new volume at `dir`, seeded, its trees named by `names`.
    pub fn new(dir: &Path, names: EngineNames) -> Result<Self, CallError> {
        let turns = Turns::new();
        let mut volume = create(dir, Upkeep::gated(turns.clone())).map_err(fail)?;
        seed(&mut volume, names.name(TreeId::ROOT)?).map_err(fail)?;
        volume.settle().map_err(fail)?;
        Ok(Self {
            volume: Some(volume),
            turns,
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
        // A replayed call ended: a compactor parked for it goes on.
        self.turns.call_ended();
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
            .write_open(tree, path, &writer, create.then_some(ATTRS))
            .map_err(fail);
        let h = self.settled(h)?;
        self.owners.insert(h, c.clone());
        Ok(h)
    }
}

impl TreeCalls for Library {
    type Open = u64;

    /// A fork with no row: this replay is the bare tree actor, below the
    /// store adapter (the mounted replay forks through it).
    #[allow(
        clippy::disallowed_methods,
        reason = "the library replay forks the bare tree actor"
    )]
    fn fork(&mut self) -> Result<TreeId, CallError> {
        let name = self
            .v_mut()?
            .fork_with(EngineTree::Root, |_, _| Ok(Vec::new()))
            .map_err(fail);
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
        let renamed = self
            .v_mut()?
            .rename(tree, from, to, RenameMode::Replace, &writer)
            .map_err(fail);
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
        self.turns.end();
        self.volume = None;
        self.owners.clear();
        self.turns = Turns::new();
        let upkeep = Upkeep::gated(self.turns.clone());
        let volume =
            TreeActor::open_with(&self.dir, &PodContext::default(), upkeep).map_err(fail)?;
        // Open never waits for history; the invariants read it after each call.
        volume.settle().map_err(fail)?;
        self.volume = Some(volume);
        Ok(())
    }

    fn mkdir(&mut self, c: &Caller, path: &str, meta: &Meta) -> Result<(), CallError> {
        let (tree, writer) = self.who(c)?;
        let made = self
            .v_mut()?
            .mkdir(tree, path, attrs(meta), &writer)
            .map(drop)
            .map_err(fail);
        self.settled(made)
    }

    fn rmdir(&mut self, c: &Caller, path: &str) -> Result<(), CallError> {
        let (tree, writer) = self.who(c)?;
        let removed = self.v_mut()?.rmdir(tree, path, &writer).map_err(fail);
        self.settled(removed)
    }

    fn set_meta(&mut self, c: &Caller, path: &str, meta: &Meta) -> Result<(), CallError> {
        let (tree, writer) = self.who(c)?;
        let set = self
            .v_mut()?
            .set_meta(tree, path, attrs(meta), &writer)
            .map_err(fail);
        self.settled(set)
    }

    fn symlink(
        &mut self,
        c: &Caller,
        path: &str,
        target: &str,
        meta: &Meta,
    ) -> Result<(), CallError> {
        let (tree, writer) = self.who(c)?;
        let linked = self
            .v_mut()?
            .symlink(tree, path, target, attrs(meta).owner, &writer)
            .map(drop)
            .map_err(fail);
        self.settled(linked)
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

    /// Every entry under the root directory, walked through its listings.
    fn entries(&self, t: TreeId) -> Result<Vec<(String, Meta)>, CallError> {
        let (v, tree) = (self.v()?, self.names.name(t)?);
        let (mut out, mut dirs) = (Vec::new(), vec![String::new()]);
        while let Some(dir) = dirs.pop() {
            for child in v.children(tree, &dir, 0, usize::MAX).map_err(fail)? {
                let path = if dir.is_empty() {
                    child.name
                } else {
                    format!("{dir}/{}", child.name)
                };
                let meta = model_meta(&v.node(tree, &path).map_err(fail)?.meta);
                if meta.kind == Kind::Dir {
                    dirs.push(path.clone());
                }
                out.push((path, meta));
            }
        }
        Ok(out)
    }
}

impl crate::Objects for Library {
    fn objects(&self, t: TreeId) -> Result<Vec<(String, u64)>, CallError> {
        Ok(self
            .v()?
            .files(self.names.name(t)?)
            .map_err(fail)?
            .into_iter()
            .map(|(path, object, _)| (path, object))
            .collect())
    }
}
