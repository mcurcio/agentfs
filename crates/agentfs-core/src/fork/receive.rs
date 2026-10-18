//! A local-only source receiving its fork's change (F8 S3; R4: a
//! no-remote source's fork publishes `agentfs/<fork>` into its own
//! history, never applied).
//!
//! When the source has no remote section and no section is flagged
//! `agentfsForkRemote` (`config::ForkRemote::SourceHistory`), the fork's
//! finisher publishes into the source's repository instead of a remote:
//! [`receive_fork_change`] copies the fork's objects and writes
//! `refs/heads/agentfs/<fork>` (by [`crate::push::claim`]: created, or
//! fast-forwarded from a value on the fork's history), then the change
//! and finish refs, create-only. It never touches the source's own
//! branch, its patch queue or its tree: whatever reads the source's
//! history does the gating (principle 9).
//!
//! The fork's output is untrusted (design §11.5, "potentially hostile"),
//! so each way it enters the source has one judged path: refs only
//! through [`ForkRefs`], which refuses the source's own branch and
//! namespace; objects only less a base [`OnBranch`] proved on the
//! source's branch; LFS objects only through the cache's verified insert,
//! which keeps the bytes it hashed.
//!
//! Once the source declares a remote, its pushes carry those refs
//! ([`received_refs`]). The fork and the source are on one node (forks are placed on their source's node). The CLI and the
//! node plugin run it through the source's control socket when its
//! service is up (design §2.3), else directly under the source's state
//! lock.
//!
//! The fork finds its source by [`SOURCE_STATE_KEY`], written into the
//! fork's own config at create when the source is local-only
//! (`config::Settings::source_state`). [`Receiver`] is how a finisher
//! reaches it: [`UnderLock`] in this process, or the CLI's, which asks the
//! source's running service first.

use std::io::{self, Write as _};
use std::time::Duration;

use git2::{Oid, Repository};

use crate::config::Settings;
use crate::events::PushFailureKind;
use crate::fork::finish::FinishRecord;
use crate::history::{HistoryError, finish_from_message};
use crate::lfs::ObjectCache;
use crate::lock::{LockError, StateLock};
use crate::push::{Claim, claim};
use crate::volume::StateDir;

use super::identity::{ForkIdentity, PushTarget, change_ref, finish_ref};

/// `agentfs.fork.sourceState`: the source's state area, in a fork of a
/// local-only source (its own config only, like the fork identity).
pub const SOURCE_STATE_KEY: &str = "agentfs.fork.sourceState";

/// The fork remote a finish names when it publishes into the source's
/// history: not a valid remote name (`config::validate_name`), so no
/// remote section can be mistaken for it.
pub const SOURCE_HISTORY: &str = "(source history)";

#[derive(Debug, thiserror::Error)]
pub enum ReceiveError {
    /// The source's state area is gone (removed by hand): the fork can be
    /// abandoned (`agentfs fork abandon`).
    #[error("the source volume's state area is gone")]
    SourceRemoved,
    /// The fork has no finish commit yet.
    #[error("the fork is not finished: nothing to receive")]
    NotPublished,
    /// The source holds `refname` at a commit that is not on the fork's
    /// history ([`crate::push::Claim::Diverged`]): nothing was written.
    #[error("{refname} in the source's history is another writer's; nothing was written")]
    Diverged { refname: String },
    /// `refname`, a ref the fork would write, is the source's own: its
    /// branch, or a ref under `refs/agentfs/<source>/` ([`ForkRefs`]).
    /// Nothing was written.
    #[error("{refname} is the source's own ref, never a fork's; nothing was written")]
    SourcesRef { refname: String },
    /// The fork's base is not on the source's branch ([`OnBranch`]): the
    /// source never had it, or its history was rewritten since. Nothing
    /// was written.
    #[error("the fork's base {base} is not on the source's branch; nothing was written")]
    BaseNotOnBranch { base: Oid },
    /// An LFS object in the fork's cache is not what its name says (its
    /// SHA-256 or size differ): nothing of it was kept, and no ref was
    /// written.
    #[error("{message}")]
    LfsMismatch { message: String },
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl ReceiveError {
    /// The refusal kind and message a source's service answers with over
    /// its control socket (`receive_fork`); `None` for an error that is
    /// not a refusal. [`ReceiveError::from_refusal`] reads it back.
    pub fn refusal(&self) -> Option<(&'static str, String)> {
        Some(match self {
            Self::SourceRemoved => ("source_removed", self.to_string()),
            Self::NotPublished => ("not_published", self.to_string()),
            Self::Diverged { refname } => ("diverged", refname.clone()),
            Self::SourcesRef { refname } => ("sources_ref", refname.clone()),
            Self::BaseNotOnBranch { base } => ("base_not_on_branch", base.to_string()),
            Self::LfsMismatch { message } => ("lfs_mismatch", message.clone()),
            Self::History(_) | Self::Io(_) => return None,
        })
    }

    /// The refusal [`ReceiveError::refusal`] wrote; `None` for a kind it
    /// never writes, or a message that cannot be its.
    pub fn from_refusal(kind: &str, message: String) -> Option<Self> {
        Some(match kind {
            "source_removed" => Self::SourceRemoved,
            "not_published" => Self::NotPublished,
            "diverged" => Self::Diverged { refname: message },
            "sources_ref" => Self::SourcesRef { refname: message },
            "base_not_on_branch" => Self::BaseNotOnBranch {
                base: super::identity::full_commit(&message)?,
            },
            "lfs_mismatch" => Self::LfsMismatch { message },
            _ => return None,
        })
    }
}

impl From<git2::Error> for ReceiveError {
    fn from(e: git2::Error) -> Self {
        Self::History(e.into())
    }
}

/// Writes `fork`'s branch, change and finish refs from the fork's state
/// area `from` into the source's state area `source` (see the module
/// docs). Returns the change commit, or the finish commit when the fork
/// changed nothing. Idempotent: a retry after a crash finds the refs at
/// the same ids and succeeds. The caller holds the source's state lock,
/// or is the source's own service.
///
/// A state area at `source` that is not `fork`'s source (another volume,
/// or a fork) is [`ReceiveError::SourceRemoved`]: the source that was
/// there is gone.
pub fn receive_fork_change(
    source: &StateDir,
    fork: &ForkIdentity,
    from: &StateDir,
) -> Result<Oid, ReceiveError> {
    crate::history::init_libgit2();
    let source_git = source.git();
    if !source_git.join("config").is_file() {
        return Err(ReceiveError::SourceRemoved);
    }
    let settings = Settings::load(&source_git).map_err(HistoryError::from)?;
    if settings.volume != fork.source || settings.is_fork() {
        return Err(ReceiveError::SourceRemoved);
    }
    let forked = Repository::open_bare(from.git())?;
    let history_ref = fork.history_ref();
    let head = match forked.find_reference(&history_ref) {
        Ok(r) => r.peel_to_commit()?,
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            return Err(ReceiveError::NotPublished);
        }
        Err(e) => return Err(e.into()),
    };
    let record = finish_from_message(head.message_raw().unwrap_or_default(), head.id())
        .filter(|r| r.fork == fork.fork)
        .ok_or(ReceiveError::NotPublished)?;
    let refs = ForkRefs::judge(&settings, fork, &record)?;
    let repo = Repository::open_bare(&source_git)?;
    let base = OnBranch::judge(&repo, &settings.branch_ref, fork.base)?;

    // Every ref is judged before anything is written.
    let advertised = existing(&repo, &refs.history)?;
    let on_history = |oid: Oid| {
        forked.odb().is_ok_and(|odb| odb.exists(oid))
            && forked
                .graph_descendant_of(record.finish, oid)
                .unwrap_or(false)
    };
    let branch = claim(advertised, record.finish, on_history);
    if branch == Claim::Diverged {
        return Err(ReceiveError::Diverged {
            refname: refs.history,
        });
    }
    for (name, oid) in &refs.create_only {
        if existing(&repo, name)?.is_some_and(|found| found != *oid) {
            return Err(ReceiveError::Diverged {
                refname: name.clone(),
            });
        }
    }

    copy_objects(&forked, &repo, &record, base, advertised)?;
    copy_lfs(from, source)?;
    let message = format!("agentfs: fork {} received", fork.fork);
    match (branch, advertised) {
        (Claim::Create, _) => {
            repo.reference(&refs.history, record.finish, false, &message)?;
        }
        // Leased on the value judged, as a push is.
        (Claim::FastForward, Some(current)) => {
            repo.reference_matching(&refs.history, record.finish, true, current, &message)?;
        }
        _ => {}
    }
    for (name, oid) in &refs.create_only {
        if existing(&repo, name)?.is_none() {
            repo.reference(name, *oid, false, &message)?;
        }
    }
    for name in refs.names() {
        super::create::sync_parents(&source_git, name)?;
    }
    Ok(record.change.unwrap_or(record.finish))
}

/// The refs a receive writes into the source: the fork's history ref,
/// then its change (when there is one) and finish refs, create-only.
/// Made only by [`ForkRefs::judge`], so no ref is written that was not
/// judged the fork's.
#[derive(Debug)]
struct ForkRefs {
    history: String,
    create_only: Vec<(String, Oid)>,
}

impl ForkRefs {
    /// `fork`'s refs for `record`, refused ([`ReceiveError::SourcesRef`])
    /// when any is the source's own: its branch (`agentfs.branch` may
    /// name any), or under its namespace `refs/agentfs/<source>/`, which
    /// holds its patch queue, divergence saves and accepted tips. The
    /// fork's name is a valid volume name (`finish_from_message` checks
    /// the record's, which must equal it), so no `/` in it reaches into
    /// another volume's namespace.
    fn judge(
        source: &Settings,
        fork: &ForkIdentity,
        record: &FinishRecord,
    ) -> Result<Self, ReceiveError> {
        let mut create_only = vec![(finish_ref(&fork.fork), record.finish)];
        if let Some(change) = record.change {
            create_only.insert(0, (change_ref(&fork.fork), change));
        }
        let refs = Self {
            history: fork.history_ref(),
            create_only,
        };
        let namespace = format!("refs/agentfs/{}/", source.volume);
        if let Some(own) = refs
            .names()
            .find(|name| **name == source.branch_ref || name.starts_with(&namespace))
        {
            return Err(ReceiveError::SourcesRef {
                refname: own.clone(),
            });
        }
        Ok(refs)
    }

    /// Every ref name, the history ref first.
    fn names(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.history).chain(self.create_only.iter().map(|(n, _)| n))
    }
}

/// A commit on the source's branch: the fork's base, judged by
/// [`OnBranch::judge`]. [`copy_objects`] takes only this, so the fork's
/// objects are copied less what a commit the source holds reaches, never
/// less a base the fork merely names.
#[derive(Debug, Clone, Copy)]
struct OnBranch(Oid);

impl OnBranch {
    /// `base` when the source's branch `branch_ref` is at it or descends
    /// from it; else [`ReceiveError::BaseNotOnBranch`]: a base the source
    /// never had, or one its rewritten history (design §15) no longer
    /// holds, whose fork would bring that history back.
    fn judge(repo: &Repository, branch_ref: &str, base: Oid) -> Result<Self, ReceiveError> {
        let on = match existing(repo, branch_ref)? {
            Some(head) if head == base => true,
            Some(head) => repo.odb()?.exists(base) && repo.graph_descendant_of(head, base)?,
            None => false,
        };
        if on {
            Ok(Self(base))
        } else {
            Err(ReceiveError::BaseNotOnBranch { base })
        }
    }
}

/// The refs [`receive_fork_change`] wrote into `repo`, the history of the
/// volume `volume`: for each fork received (a `refs/agentfs/<fork>/finish`
/// of another volume), its branch or hidden history ref, its change and
/// its finish, sorted by name. They are this history too: the source's
/// branch push carries them to every remote, create-only, and DeleteVolume
/// waits until each remote holds them (`push::RemotePusher`), so a remote
/// added after the source was local-only gets them and deleting the source
/// never loses them.
pub fn received_refs(repo: &Repository, volume: &str) -> Result<Vec<(String, Oid)>, git2::Error> {
    let mut forks = Vec::new();
    for reference in repo.references_glob("refs/agentfs/*/finish")? {
        let reference = reference?;
        let fork = reference
            .name()
            .ok()
            .and_then(|n| n.strip_prefix("refs/agentfs/"))
            .and_then(|n| n.strip_suffix("/finish"));
        if let Some(fork) = fork
            && fork != volume
            && !fork.contains('/')
        {
            forks.push(fork.to_owned());
        }
    }
    let mut out = Vec::new();
    for fork in &forks {
        for name in [
            PushTarget::Branch.history_ref(fork),
            PushTarget::Hidden.history_ref(fork),
            change_ref(fork),
            finish_ref(fork),
        ] {
            if let Some(oid) = existing(repo, &name)? {
                out.push((name, oid));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The ref `name` in `repo`, `None` when there is none.
fn existing(repo: &Repository, name: &str) -> Result<Option<Oid>, git2::Error> {
    match repo.refname_to_id(name) {
        Ok(oid) => Ok(Some(oid)),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The fork's history and change, packed into the source's object store,
/// less what the source has: base, and the branch value it holds.
fn copy_objects(
    forked: &Repository,
    repo: &Repository,
    record: &FinishRecord,
    base: OnBranch,
    held: Option<Oid>,
) -> Result<(), ReceiveError> {
    let odb = repo.odb()?;
    let mut walk = forked.revwalk()?;
    walk.push(record.finish)?;
    if let Some(change) = record.change {
        walk.push(change)?;
    }
    for have in [Some(base.0), held].into_iter().flatten() {
        if odb.exists(have) {
            walk.hide(have)?;
        }
    }
    let mut pack = forked.packbuilder()?;
    pack.insert_walk(&mut walk)?;
    let mut writer = odb.packwriter()?;
    let mut write_error = None;
    let packed = pack.foreach(|chunk| match writer.write_all(chunk) {
        Ok(()) => true,
        Err(e) => {
            write_error = Some(e);
            false
        }
    });
    if let Some(e) = write_error {
        return Err(e.into());
    }
    packed?;
    writer.commit()?;
    for oid in [Some(record.finish), record.change].into_iter().flatten() {
        if !odb.exists(oid) {
            return Err(io::Error::other(format!(
                "commit {oid} is not in the source after copying the fork's objects"
            ))
            .into());
        }
    }
    Ok(())
}

/// Every LFS object in the fork's cache that the source's lacks: the
/// fork's history, now the source's too, refers to them, and the fork's
/// cache goes with the fork. Each is read once, through the cache's
/// verified insert, which hashes the bytes as it writes them and keeps
/// them only under the name they hash to: what is kept is what was
/// checked ([`ReceiveError::LfsMismatch`] otherwise).
fn copy_lfs(from: &StateDir, source: &StateDir) -> Result<(), ReceiveError> {
    let forked = ObjectCache::at(from.lfs())?;
    let cache = ObjectCache::at(source.lfs())?;
    for (pointer, _) in forked.list()? {
        if cache.contains(&pointer) {
            continue;
        }
        let mut object = forked.open_object(&pointer)?;
        cache
            .insert_verified(&pointer, &mut object)
            .map_err(|e| match e.kind {
                PushFailureKind::Refused => ReceiveError::LfsMismatch { message: e.message },
                _ => ReceiveError::Io(io::Error::other(e.message)),
            })?;
    }
    Ok(())
}

/// Why a [`Receiver`] did not receive the change.
#[derive(Debug, thiserror::Error)]
pub enum ReceiverError {
    /// The source's state lock is held and no service of it answered:
    /// `agentfs fork finish` exits 75, as other lock commands do.
    #[error(transparent)]
    Busy(LockError),
    #[error(transparent)]
    Receive(#[from] ReceiveError),
    /// The source's service, or its lock, failed otherwise: retried.
    #[error("{0}")]
    Unavailable(String),
}

/// How a finisher reaches its local-only source.
pub trait Receiver: Send {
    /// [`receive_fork_change`] of `fork` from `from` into `source`, with
    /// whatever makes it safe there.
    fn receive(
        &self,
        source: &StateDir,
        fork: &ForkIdentity,
        from: &StateDir,
    ) -> Result<(), ReceiverError>;
}

/// [`receive_fork_change`] in this process, under the source's state
/// lock, taken without waiting past the lock's minimum.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnderLock;

impl Receiver for UnderLock {
    fn receive(
        &self,
        source: &StateDir,
        fork: &ForkIdentity,
        from: &StateDir,
    ) -> Result<(), ReceiverError> {
        // Locking creates `meta/`: never in a state area that is gone.
        if !source.git().join("config").is_file() {
            return Err(ReceiveError::SourceRemoved.into());
        }
        let _lock =
            StateLock::acquire(source, "fork receive", Duration::ZERO).map_err(|e| match e {
                busy @ LockError::Busy { .. } => ReceiverError::Busy(busy),
                other => ReceiverError::Unavailable(other.to_string()),
            })?;
        receive_fork_change(source, fork, from)?;
        Ok(())
    }
}
