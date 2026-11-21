//! Sessions: the history service's side of each attached client
//! (docs/library/core.md, "Sessions"; docs/trees.md, "Clients"). Clients
//! binds a client's authority to a session, a `ClientCap` for a read-write
//! client or its tree for a read-only one; a view reaches its tree only
//! through its session, so its calls name no tree and no writer (C5), a
//! read-only session cannot write (C3) and a session reaches only its own
//! client's opens.
//!
//! Lock order: the tree actor's lock, then the bindings'.

use std::collections::BTreeMap;
use std::ops::DerefMut;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use agentfs_contracts::clients::{FileOwner, SessionId};
use agentfs_contracts::pod::WriterAt;
use agentfs_contracts::ports::{SessionAuthority, SessionBinder};

use crate::tree_actor::{
    Attrs, DirEntry, Error, FileMap, Node, Recording, RenameMode, Saved, SetTime, Then, TreeActor,
    TreeId, UntrackedArea,
};

type Result<T> = std::result::Result<T, Error>;

/// Every session of one served volume, and what each may do. Cheap to
/// clone: every clone shares the bindings.
#[derive(Clone)]
pub struct Sessions {
    recording: Arc<Recording>,
    bound: Arc<Mutex<BTreeMap<SessionId, SessionAuthority>>>,
    next: Arc<AtomicU64>,
}

impl Sessions {
    pub fn new(recording: Arc<Recording>) -> Self {
        Self {
            recording,
            bound: Arc::default(),
            next: Arc::new(AtomicU64::new(1)),
        }
    }

    /// The volume served: what the view host's takes and metrics read.
    pub fn recording(&self) -> &Arc<Recording> {
        &self.recording
    }

    /// A new session's number, unique per process run. It may do nothing
    /// until Clients binds it.
    pub fn open(&self) -> SessionId {
        SessionId(self.next.fetch_add(1, Ordering::Relaxed))
    }

    /// The session, while it is bound.
    pub fn session(&self, id: SessionId) -> Option<TreeSession> {
        let tree = match self.bindings().get(&id)? {
            SessionAuthority::Write(cap) => cap.tree(),
            SessionAuthority::Read(tree) => *tree,
        };
        Some(TreeSession {
            sessions: self.clone(),
            id,
            tree,
        })
    }

    fn bindings(&self) -> MutexGuard<'_, BTreeMap<SessionId, SessionAuthority>> {
        self.bound.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SessionBinder for Sessions {
    fn bind(&mut self, session: SessionId, authority: SessionAuthority) {
        self.bindings().insert(session, authority);
    }

    /// Under the actor's lock the binding goes and every open of its writer
    /// closes as a save; then, unlocked, every record is made durable. A
    /// failure is logged: an unsaved close stays pending, saved at its tree's
    /// next step or at restart.
    fn unbind(&mut self, session: SessionId) {
        let actor = self.recording.actor();
        let authority = self.bindings().remove(&session);
        let Some(SessionAuthority::Write(cap)) = authority else {
            return;
        };
        let mut actor = match actor {
            Ok(actor) => actor,
            Err(e) => {
                eprintln!("tree SaveFailed: client {}'s opens: {e}", cap.client());
                return;
            }
        };
        #[expect(clippy::disallowed_methods, reason = "the session's own unbind")]
        let closed = actor.close_writer(cap.tree(), &cap.writes_as().writer);
        drop(actor);
        if let Err(e) = closed.map(drop).and_then(|()| self.recording.durable_all()) {
            eprintln!("tree SaveFailed: client {}'s opens: {e}", cap.client());
        }
    }
}

/// One bound session: a view's way to its tree.
#[derive(Clone)]
pub struct TreeSession {
    sessions: Sessions,
    id: SessionId,
    tree: TreeId,
}

impl TreeSession {
    pub fn tree(&self) -> TreeId {
        self.tree
    }

    /// The actor, locked, with this session's binding read under the lock:
    /// after `unbind`, its writes answer [`Error::ReadOnly`].
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped.
    pub fn lock(&self) -> Result<SessionGuard<'_>> {
        let actor = self.sessions.recording.actor()?;
        let writes_as = self.writes_as();
        Ok(SessionGuard {
            actor: Box::new(actor),
            tree: self.tree,
            writes_as,
        })
    }

    /// The session's writer, read from its binding: none once unbound or
    /// for a read-only session.
    fn writes_as(&self) -> Option<WriterAt> {
        match self.sessions.bindings().get(&self.id) {
            Some(SessionAuthority::Write(cap)) => Some(cap.writes_as().clone()),
            _ => None,
        }
    }

    /// RELEASE of this session's open `handle` ([`Recording::close_with`]):
    /// checked as the session's own under the lock, decided with the lock
    /// released, so the volume's other views go on meanwhile. Saved before
    /// it returns.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an open that is not this session's; as
    /// [`TreeActor::close`].
    pub fn close(&self, handle: u64) -> Result<Saved> {
        self.sessions.recording.close_with(handle, |actor| {
            own(actor, self.tree, self.writes_as().as_ref(), handle)
        })
    }

    /// The operator's rules ([`Recording::rules`]).
    pub fn rules(&self) -> &Arc<agentfs_rules::RuleSet> {
        self.sessions.recording.rules()
    }

    /// This session's tree's untracked area ([`Recording::untracked_area`]):
    /// a view reaches no other tree's.
    ///
    /// # Errors
    ///
    /// As [`Recording::untracked_area`].
    pub fn untracked_area(&self) -> Result<UntrackedArea> {
        self.sessions.recording.untracked_area(self.tree)
    }

    /// This tree's lag gate ([`Recording::admit_then`]).
    pub fn admit_then(&self, then: Then) {
        self.sessions.recording.admit_then(self.tree, then);
    }

    /// An open's records made durable ([`Recording::durable_then`]).
    pub fn durable_then(&self, handle: u64, then: Then) {
        self.sessions.recording.durable_then(handle, then);
    }

    /// Every record made durable ([`Recording::durable_all_then`]).
    pub fn durable_all_then(&self, then: Then) {
        self.sessions.recording.durable_all_then(then);
    }
}

/// `handle` is an open of `tree` by the writer `writes_as`.
fn own(actor: &TreeActor, tree: TreeId, writes_as: Option<&WriterAt>, handle: u64) -> Result<()> {
    let (of, writer) = actor.open_by(handle)?;
    match writes_as {
        Some(by) if of == tree && *writer == by.writer => Ok(()),
        _ => Err(Error::NotFound),
    }
}

/// The actor locked for one session: [`TreeActor`]'s calls of the same name,
/// with the session's tree and writer. The lock is [`Recording::actor`]'s
/// guard, measured as every hold is; it is held boxed because the guard's
/// type is not named outside the tree actor.
pub struct SessionGuard<'a> {
    actor: Box<dyn DerefMut<Target = TreeActor> + 'a>,
    tree: TreeId,
    writes_as: Option<WriterAt>,
}

impl SessionGuard<'_> {
    /// The session's writer; none for a read-only or unbound session.
    fn writer(&self) -> Result<&WriterAt> {
        self.writes_as.as_ref().ok_or(Error::ReadOnly)
    }

    /// `handle` is an open of this session's tree and writer.
    fn own(&self, handle: u64) -> Result<()> {
        own(&self.actor, self.tree, self.writes_as.as_ref(), handle)
    }

    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        self.actor.read(self.tree, path)
    }

    pub fn file_map(&self, path: &str) -> Result<FileMap> {
        self.actor.file_map(self.tree, path)
    }

    pub fn file_len(&self, path: &str) -> Result<u64> {
        self.actor.file_len(self.tree, path)
    }

    pub fn node(&self, path: &str) -> Result<Node> {
        self.actor.node(self.tree, path)
    }

    pub fn node_at(&self, number: u64) -> Result<(&str, Node)> {
        self.actor.node_at(self.tree, number)
    }

    pub fn open_node(&self, object: u64) -> Result<Node> {
        self.actor.open_node(self.tree, object)
    }

    pub fn children(&self, dir: &str, skip: usize, count: usize) -> Result<Vec<DirEntry>> {
        self.actor.children(self.tree, dir, skip, count)
    }

    pub fn readlink(&self, path: &str) -> Result<Vec<u8>> {
        self.actor.readlink(self.tree, path)
    }

    pub fn mkdir(&mut self, path: &str, attrs: Attrs) -> Result<u64> {
        let by = self.writes_as.as_ref().ok_or(Error::ReadOnly)?;
        self.actor.mkdir(self.tree, path, attrs, by)
    }

    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        let by = self.writes_as.as_ref().ok_or(Error::ReadOnly)?;
        self.actor.rmdir(self.tree, path, by)
    }

    pub fn symlink(&mut self, path: &str, target: &str, owner: FileOwner) -> Result<u64> {
        let by = self.writes_as.as_ref().ok_or(Error::ReadOnly)?;
        self.actor.symlink(self.tree, path, target, owner, by)
    }

    pub fn set_meta(&mut self, path: &str, attrs: Attrs) -> Result<()> {
        let by = self.writes_as.as_ref().ok_or(Error::ReadOnly)?;
        self.actor.set_meta(self.tree, path, attrs, by)
    }

    pub fn set_times(
        &mut self,
        path: &str,
        atime: Option<SetTime>,
        mtime: Option<SetTime>,
    ) -> Result<()> {
        let by = self.writes_as.as_ref().ok_or(Error::ReadOnly)?;
        self.actor.set_times(self.tree, path, atime, mtime, by)
    }

    pub fn object_at(&self, path: &str) -> Result<u64> {
        self.actor.object_at(self.tree, path)
    }

    pub fn path_of(&self, object: u64) -> Result<&str> {
        self.actor.path_of(self.tree, object)
    }

    pub fn is_anonymous(&self, object: u64) -> Result<bool> {
        self.actor.is_anonymous(self.tree, object)
    }

    pub fn inode_size(&self, object: u64) -> Option<u64> {
        self.actor.inode_size(self.tree, object)
    }

    pub fn page_fill(&mut self, object: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        self.actor.page_fill(self.tree, object, offset, size)
    }

    pub fn write_open(&mut self, path: &str, create: Option<Attrs>) -> Result<u64> {
        let by = self.writes_as.as_ref().ok_or(Error::ReadOnly)?;
        self.actor.write_open(self.tree, path, by, create)
    }

    pub fn write_open_truncating(&mut self, path: &str, create: Option<Attrs>) -> Result<u64> {
        let by = self.writes_as.as_ref().ok_or(Error::ReadOnly)?;
        self.actor
            .write_open_truncating(self.tree, path, by, create)
    }

    pub fn anonymous_open(&mut self, attrs: Attrs) -> Result<u64> {
        let by = self.writes_as.as_ref().ok_or(Error::ReadOnly)?;
        self.actor.anonymous_open(self.tree, by, attrs)
    }

    pub fn first_link(&mut self, object: u64, path: &str) -> Result<()> {
        let by = self.writes_as.as_ref().ok_or(Error::ReadOnly)?;
        self.actor.first_link(self.tree, object, path, by)
    }

    pub fn rename(&mut self, from: &str, to: &str, mode: RenameMode) -> Result<()> {
        let by = self.writes_as.as_ref().ok_or(Error::ReadOnly)?;
        self.actor.rename(self.tree, from, to, mode, by)
    }

    /// A whole-file replace of `path` with the `len` bytes of `src`
    /// ([`TreeActor::replace_file`]).
    pub fn replace_file(
        &mut self,
        path: &str,
        attrs: Attrs,
        src: &mut dyn std::io::Read,
        len: u64,
    ) -> Result<Saved> {
        let by = self.writes_as.as_ref().ok_or(Error::ReadOnly)?;
        self.actor
            .replace_file(self.tree, path, attrs, src, len, by)
    }

    pub fn unlink(&mut self, path: &str) -> Result<()> {
        let by = self.writes_as.as_ref().ok_or(Error::ReadOnly)?;
        self.actor.unlink(self.tree, path, by)
    }

    pub fn open_object(&self, handle: u64) -> Result<u64> {
        self.own(handle)?;
        self.actor.open_object(handle)
    }

    pub fn read_open(&self, handle: u64) -> Result<&FileMap> {
        self.own(handle)?;
        self.actor.read_open(handle)
    }

    pub fn write(&mut self, handle: u64, offset: u64, bytes: &[u8], append: bool) -> Result<()> {
        self.own(handle)?;
        self.actor.write(handle, offset, bytes, append)
    }

    pub fn truncate(&mut self, handle: u64, size: u64) -> Result<()> {
        self.own(handle)?;
        self.actor.truncate(handle, size)
    }

    pub fn page_writeback(&mut self, handle: u64, offset: u64, bytes: &[u8]) -> Result<()> {
        self.own(handle)?;
        self.actor.page_writeback(handle, offset, bytes)
    }

    pub fn close(&mut self, handle: u64) -> Result<Saved> {
        self.own(handle)?;
        self.actor.close(handle)
    }

    /// Whether the session may write: a writer is bound.
    pub fn writable(&self) -> bool {
        self.writer().is_ok()
    }
}
