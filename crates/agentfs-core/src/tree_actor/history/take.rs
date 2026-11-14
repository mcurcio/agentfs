//! Queue takes decided on built history: a landing judged into its upstream,
//! and a fork synced from its upstream (target git-model §6). Pure over a
//! [`BuiltSnapshot`]'s Git reads and the tree as built; the actor records the
//! decision as a take step.

use std::collections::{BTreeMap, BTreeSet};

use git2::Oid;

use std::io;

use super::Objects;
use super::built::BuiltSnapshot;
use super::format::{self, read_snapshot};
use super::message;
use super::refs;
use crate::tree_actor::journal::{TAKE_CHUNK, TakeHead};
use crate::tree_actor::stages::{Timer, Which};
use crate::tree_actor::tree::{EventNumber, Lines, MergeFileIdentity};
use crate::tree_actor::{Error, Result, TreeId, queue, referee};
use crate::tree_git::{STREAMED_BLOB, TreeGit};
use crate::tree_reconcile::{self, FileImage, Image, Piece};

/// The actor's [`referee::Landed`]: whether a save's commit is in `upstream`'s history. That
/// never changes, and every line one save wrote has that save's commit: ask Git once per commit,
/// not once per line (an 8 MiB file is 8,000 lines).
struct LandedInto<'a> {
    repo: &'a TreeGit,
    owners: &'a BTreeMap<EventNumber, Oid>,
    upstream: Oid,
    reached: BTreeMap<Oid, bool>,
}

impl referee::Landed for LandedInto<'_> {
    fn landed(&mut self, save: EventNumber) -> Result<bool> {
        let owner = *self.owners.get(&save).ok_or(Error::Format)?;
        if let Some(known) = self.reached.get(&owner) {
            return Ok(*known);
        }
        let answer = self.repo.is_ancestor(owner, self.upstream)?;
        self.reached.insert(owner, answer);
        Ok(answer)
    }
}

/// One upstream commit a sync can take, the fork's snapshot after it, and the
/// older commits whose files hold lines it keeps (finding 44's last lines).
pub(in crate::tree_actor) struct SyncStep {
    pub(in crate::tree_actor) commit: Oid,
    pub(in crate::tree_actor) snapshot: referee::Snapshot,
    pub(in crate::tree_actor) older: Vec<Oid>,
}

/// A take as decided: how the head moves, the snapshot it leaves, its
/// collisions, whether its queue entry goes, and the commits other than the
/// tree's head whose files hold its lines.
pub(in crate::tree_actor) struct Decided {
    pub head: TakeHead,
    pub snapshot: Option<referee::Snapshot>,
    pub collisions: Vec<referee::Collision>,
    pub entry_done: bool,
    pub sources: Vec<Oid>,
}

/// Where a take's files' bytes are: the blobs of the files whose lines it
/// keeps, and each file's pieces of them, in its record's `files` order. A
/// version's bytes are the same in every file that holds it, so a take's file
/// is written from them without a line's bytes in any map.
pub(in crate::tree_actor) struct TakeBytes {
    images: Vec<Box<dyn Image + Send + Sync>>,
    files: Vec<Vec<Piece>>,
}

impl std::fmt::Debug for TakeBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TakeBytes")
            .field("images", &self.images.len())
            .field("files", &self.files)
            .finish()
    }
}

impl TakeBytes {
    /// Each file's length, in order.
    pub(in crate::tree_actor) fn lens(&self) -> Vec<u64> {
        self.files
            .iter()
            .map(|pieces| pieces.iter().map(|p| p.len).sum())
            .collect()
    }

    /// The files' bytes in order, in chunks of at most [`TAKE_CHUNK`]: one
    /// chunk in memory at a time.
    pub(in crate::tree_actor) fn chunks(&self) -> impl Iterator<Item = io::Result<Vec<u8>>> + '_ {
        let mut pieces = self.files.iter().flatten().copied();
        let mut current: Option<Piece> = None;
        std::iter::from_fn(move || {
            let mut buf = Vec::new();
            while (buf.len() as u64) < TAKE_CHUNK {
                let piece = match current.take() {
                    Some(piece) if piece.len > 0 => piece,
                    _ => match pieces.next() {
                        Some(piece) => piece,
                        None => break,
                    },
                };
                let n = piece.len.min(TAKE_CHUNK - buf.len() as u64);
                let start = buf.len();
                buf.resize(start + n as usize, 0);
                let mut done = 0;
                while done < buf.len() - start {
                    match self.images[piece.source]
                        .read_at(&mut buf[start + done..], piece.offset + done as u64)
                    {
                        Ok(0) => return Some(Err(io::ErrorKind::UnexpectedEof.into())),
                        Ok(got) => done += got,
                        Err(e) => return Some(Err(e)),
                    }
                }
                current = Some(Piece {
                    offset: piece.offset + n,
                    len: piece.len - n,
                    ..piece
                });
            }
            (!buf.is_empty()).then_some(Ok(buf))
        })
    }
}

/// A blob as an image: in memory up to [`STREAMED_BLOB`], else streamed by
/// `git cat-file` into an unlinked file of the state area.
fn blob_image(repo: &TreeGit, oid: Oid) -> Result<Box<dyn Image + Send + Sync>> {
    if repo.blob_size(oid)? <= STREAMED_BLOB {
        return Ok(Box::new(repo.read_blob(oid)?));
    }
    let dir = repo.path().parent().ok_or(Error::Format)?;
    let file = unlinked_file(dir)?;
    repo.blob_to(oid, &file)?;
    Ok(Box::new(FileImage::of(file)?))
}

/// A new file in `dir` with no name: its space returns when it is dropped.
fn unlinked_file(dir: &std::path::Path) -> io::Result<std::fs::File> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    loop {
        let n = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!(".take-{}-{n}", std::process::id()));
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                std::fs::remove_file(&path)?;
                return Ok(file);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// The bytes of each file `files` leaves, from the files of the same identity
/// in the tree's head and in `sources`.
pub(in crate::tree_actor) fn take_bytes(
    snapshot: &BuiltSnapshot,
    tree: TreeId,
    files: &[(MergeFileIdentity, Option<referee::File>)],
    sources: &[Oid],
) -> Result<TakeBytes> {
    let repo = snapshot.repo();
    let commits: Vec<Oid> = snapshot
        .head(tree)
        .into_iter()
        .chain(sources.iter().copied())
        .collect();
    let mut images: Vec<Box<dyn Image + Send + Sync>> = Vec::new();
    let mut by_blob: BTreeMap<Oid, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for (identity, file) in files {
        let Some(file) = file else {
            continue;
        };
        let mut held = Vec::new();
        for commit in &commits {
            if let Some((lines, blob)) = format::file_of(repo, *commit, *identity)? {
                let at = match by_blob.get(&blob) {
                    Some(at) => *at,
                    None => {
                        images.push(blob_image(repo, blob)?);
                        by_blob.insert(blob, images.len() - 1);
                        images.len() - 1
                    }
                };
                held.push((lines, at));
            }
        }
        let sources: Vec<(&Lines, &dyn Image)> = held
            .iter()
            .map(|(lines, at)| (lines, &*images[*at] as &dyn Image))
            .collect();
        let pieces = tree_reconcile::pieces(&file.lines, &sources)?;
        out.push(
            pieces
                .into_iter()
                .map(|p| Piece {
                    source: held[p.source].1,
                    ..p
                })
                .collect(),
        );
    }
    Ok(TakeBytes { images, files: out })
}

/// Whether two snapshots show the same: the same files, each with the same line
/// versions in the same order, and the same name versions. The versions each line
/// version was written over may differ.
fn shows_the_same(a: &referee::Snapshot, b: &referee::Snapshot) -> bool {
    a.files.len() == b.files.len()
        && a.names.len() == b.names.len()
        && a.files.iter().all(|(id, file)| {
            b.files
                .get(id)
                .is_some_and(|other| other.lines.runs() == file.lines.runs())
        })
        && a.names.iter().all(|(id, history)| {
            b.names
                .get(id)
                .is_some_and(|other| other.head == history.head)
        })
}

fn prior_decisions(repo: &TreeGit, mut head: Oid) -> Result<referee::PriorDecisions> {
    let _timer = Timer::start(Which::PriorDecisions);
    let mut result = BTreeMap::new();
    loop {
        let commit = repo.read_commit(head)?;
        for collision in message::collisions(&commit.message)? {
            result.entry(collision.lost).or_insert(collision.winner);
        }
        match commit.parents.first() {
            Some(parent) => head = *parent,
            None => break,
        }
    }
    Ok(result)
}

/// The plan of one root queue entry: the fork it lands, and the keys it touches.
pub(in crate::tree_actor) fn plan_landing(
    snapshot: &BuiltSnapshot,
    incoming: Oid,
) -> Result<queue::Plan> {
    let repo = snapshot.repo();
    let commit = repo.read_commit(incoming)?;
    let base = *commit.parents.first().ok_or(Error::Format)?;
    let source = message::tree_of(&commit.message)?.ok_or(Error::Format)?;
    let objects = snapshot.objects(source);
    let files = referee::touches(
        &read_snapshot(repo, base, objects)?,
        &read_snapshot(repo, incoming, objects)?,
    );
    Ok(queue::Plan { source, files })
}

/// A landing of `incoming` into `tree`, whose built snapshot is `mine`.
pub(in crate::tree_actor) fn decide_landing(
    snapshot: &BuiltSnapshot,
    tree: TreeId,
    mine: &referee::Snapshot,
    incoming: Oid,
) -> Result<Decided> {
    let repo = snapshot.repo();
    let current = snapshot.head(tree).ok_or(Error::NotFound)?;
    let incoming_commit = repo.read_commit(incoming)?;
    let base_commit = *incoming_commit.parents.first().ok_or(Error::Format)?;
    // A file new to `tree` comes on the object its writer's tree has it on.
    let source = message::tree_of(&incoming_commit.message)?.ok_or(Error::Format)?;
    let objects = snapshot.objects(source);
    let (base, theirs) = (
        read_snapshot(repo, base_commit, objects)?,
        read_snapshot(repo, incoming, objects)?,
    );
    let prior = prior_decisions(repo, current)?;
    let decision = {
        let _timer = Timer::start(Which::Judge);
        referee::land(mine, &base, &theirs, &prior)?
    };
    let head = if base_commit == current {
        // Made on the parent's current version: every line replaces the
        // parent's, so nothing collides, and the save itself is the head.
        if !decision.collisions.is_empty()
            || !referee::touches(&decision.snapshot, &theirs).is_empty()
        {
            return Err(Error::Format);
        }
        TakeHead::FastForward {
            to: incoming.to_string(),
        }
    } else {
        TakeHead::Landing {
            incoming: incoming.to_string(),
        }
    };
    Ok(Decided {
        head,
        snapshot: Some(decision.snapshot),
        collisions: decision.collisions,
        entry_done: true,
        sources: vec![incoming],
    })
}

/// [`queue::cut`] over the upstream commits the fork lacks, oldest first: each is synced
/// and diffed against the last taken (or the fork's own snapshot) with `referee::touches`.
pub(in crate::tree_actor) fn plan_sync(
    snapshot: &BuiltSnapshot,
    tree: TreeId,
    upstream: TreeId,
    fork_snapshot: &referee::Snapshot,
    open: &BTreeSet<queue::Key>,
) -> Result<queue::Cut<SyncStep>> {
    let repo = snapshot.repo();
    let fork_head = snapshot.head(tree).ok_or(Error::NotFound)?;
    let mut todo = Vec::new();
    let mut cursor = snapshot.head(upstream).ok_or(Error::NotFound)?;
    while !repo.is_ancestor(cursor, fork_head)? {
        todo.push(cursor);
        let commit = repo.read_commit(cursor)?;
        let Some(parent) = commit.parents.first() else {
            break;
        };
        cursor = *parent;
    }
    todo.reverse();
    let objects = snapshot.objects(upstream);
    queue::cut(todo, open, |last: Option<&SyncStep>, oid| {
        let parent = read_snapshot(repo, oid, objects)?;
        let undecided = referee::undecided(
            fork_snapshot,
            &mut LandedInto {
                repo,
                owners: snapshot.owners(),
                upstream: oid,
                reached: BTreeMap::new(),
            },
        )?;
        let (last_lines, older) = last_lines(repo, &parent, oid, &undecided)?;
        let next = {
            let _timer = Timer::start(Which::SyncMerge);
            referee::sync(&parent, fork_snapshot, &undecided, &last_lines)?
        };
        let current = last.map_or(fork_snapshot, |step| &step.snapshot);
        let keys = referee::touches(current, &next);
        Ok((
            SyncStep {
                commit: oid,
                snapshot: next,
                older,
            },
            keys,
        ))
    })
}

/// The lines each file held in the parent's last version of it, for the files
/// the parent at `at` has ended and a fork keeps under an undecided name, and
/// the commits they were read from. The parent's snapshot has no file to read
/// them from (finding 44).
fn last_lines(
    repo: &TreeGit,
    parent: &referee::Snapshot,
    at: Oid,
    undecided: &referee::Undecided,
) -> Result<(BTreeMap<MergeFileIdentity, Lines>, Vec<Oid>)> {
    let mut wanted = referee::ended_files(parent, undecided)?;
    let mut lines = BTreeMap::new();
    let mut older = Vec::new();
    let mut cursor = at;
    while !wanted.is_empty() {
        let Some(previous) = repo.read_commit(cursor)?.parents.first().copied() else {
            break;
        };
        let snapshot = read_snapshot(repo, previous, &Objects::new())?;
        let before = wanted.len();
        wanted.retain(|id| match snapshot.files.get(id) {
            Some(file) => {
                lines.insert(*id, file.lines.clone());
                false
            }
            None => true,
        });
        if wanted.len() < before {
            older.push(previous);
        }
        cursor = previous;
    }
    Ok((lines, older))
}

/// Whether a landing of `tree` waits in `upstream`'s queue, as built.
pub(in crate::tree_actor) fn pending_from(
    snapshot: &BuiltSnapshot,
    tree: TreeId,
    upstream: TreeId,
) -> Result<bool> {
    for (_, oid) in snapshot.pointers().under(&refs::queue(upstream)) {
        let commit = snapshot.repo().read_commit(oid)?;
        if message::tree_of(&commit.message)? == Some(tree) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A sync of `tree` to the cut's last change.
pub(in crate::tree_actor) fn decide_sync(
    snapshot: &BuiltSnapshot,
    tree: TreeId,
    upstream: TreeId,
    cut: queue::Cut<SyncStep>,
) -> Result<Decided> {
    let entry_done = !cut.remaining();
    let Some(SyncStep {
        commit: target,
        snapshot: mut synced,
        older,
    }) = cut.into_taken()
    else {
        return Ok(Decided {
            head: TakeHead::Stay,
            snapshot: None,
            collisions: Vec::new(),
            entry_done,
            sources: Vec::new(),
        });
    };
    let parent = read_snapshot(snapshot.repo(), target, snapshot.objects(upstream))?;
    // R6.1: a sync that can fast-forward does. It can when nothing of the fork waits
    // for the upstream and the sync shows what the upstream's commit shows (the
    // model's `World::sync`). The fork then takes that commit's line maps and names,
    // so a version the upstream decided against stays in history only (finding 51).
    let head = if shows_the_same(&parent, &synced) && !pending_from(snapshot, tree, upstream)? {
        for (id, file) in &mut synced.files {
            if let Some(theirs) = parent.files.get(id) {
                file.lines = theirs.lines.clone();
            }
        }
        synced.names = parent.names.clone();
        TakeHead::FastForward {
            to: target.to_string(),
        }
    } else {
        TakeHead::Sync {
            target: target.to_string(),
        }
    };
    let mut sources = vec![target];
    sources.extend(older);
    Ok(Decided {
        head,
        snapshot: Some(synced),
        collisions: Vec::new(),
        entry_done,
        sources,
    })
}
