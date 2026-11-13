//! Tree history: snapshots read from and written to commits, a tree's object table, direct saves,
//! queue entries and checkouts (target git-model §4, §5).

pub(super) mod message;
pub(super) mod refs;
mod take;
pub(super) mod upstreams;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use git2::Oid;
use serde::{Deserialize, Serialize};

use super::pointers::Pointers;
use super::stages::{Timer, Which};
use super::tree::{EventNumber, Lines, MergeFileIdentity, Tree};
use super::{Draft, Error, Result, Saved, TreeId, Volume, referee};
use crate::tree_git::{RefUpdate, TreeEntry, TreeGit};
use message::{Action, Message};
pub(super) use take::SyncStep;

#[derive(Serialize, Deserialize)]
pub(in crate::tree_actor) struct ObjectTable {
    pub(in crate::tree_actor) files: BTreeMap<MergeFileIdentity, u64>,
}

// What is lost of any pointer's commit, tree and blobs refuses to serve. The
// second pass, on this failure path only, names the pointer.
pub(in crate::tree_actor) fn check_objects(repo: &TreeGit, pointers: &Pointers) -> Result<()> {
    let missing = repo.missing_objects(&pointers.tips())?;
    let Some(first) = missing.first() else {
        return Ok(());
    };
    for (name, tip) in pointers.under(refs::ALL) {
        if let Some(oid) = repo.missing_objects(&[tip])?.first() {
            return Err(Error::MissingObject { name, oid: *oid });
        }
    }
    Err(Error::MissingObject {
        name: refs::ALL.to_owned(),
        oid: *first,
    })
}

// The exclusive actor lock is held first; no other writer can own these.
pub(in crate::tree_actor) fn remove_stale_locks(gitdir: &Path) -> io::Result<()> {
    fn refs(dir: &Path) -> io::Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                refs(&entry.path())?;
            } else if entry.file_type()?.is_file()
                && entry.path().extension().is_some_and(|s| s == "lock")
            {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
    refs(&gitdir.join("refs"))?;
    for name in ["HEAD.lock", "config.lock", "packed-refs.lock", "index.lock"] {
        let path = gitdir.join(name);
        if path.is_file() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

impl Volume {
    pub(in crate::tree_actor) fn snapshot_at(&self, head: Oid) -> Result<referee::Snapshot> {
        let _timer = Timer::start(Which::SnapshotRead);
        let commit = self.repo.read_commit(head)?;
        if !message::is_envelope(&commit.message) {
            // The Root commit alone has no envelope, and an empty tree.
            if self.repo.list_tree(commit.tree)?.is_empty() {
                return Ok(referee::Snapshot {
                    files: BTreeMap::new(),
                    names: BTreeMap::new(),
                });
            }
            return Err(Error::Format);
        }
        let entries = self.repo.list_tree(commit.tree)?;
        let metadata = entries
            .iter()
            .find(|e| e.name == b"metadata.json")
            .ok_or(Error::Format)?;
        let namespace = entries
            .iter()
            .find(|e| e.name == b"namespace.json")
            .ok_or(Error::Format)?;
        let by_path: BTreeMap<String, (u64, MergeFileIdentity, Lines)> =
            serde_json::from_slice(&self.repo.read_blob(metadata.oid)?)?;
        let names = serde_json::from_slice(&self.repo.read_blob(namespace.oid)?)?;
        let files = by_path
            .into_values()
            .map(|(object, id, lines)| (id, referee::File { object, lines }))
            .collect();
        Ok(referee::Snapshot { files, names })
    }

    pub(in crate::tree_actor) fn snapshot_tree(&self, snapshot: &referee::Snapshot) -> Result<Oid> {
        let _timer = Timer::start(Which::Objects);
        let mut files = Vec::new();
        let mut metadata = BTreeMap::new();
        for (id, file) in &snapshot.files {
            let history = snapshot.names.get(id).ok_or(Error::Format)?;
            let path = history.path()?.ok_or(Error::Format)?;
            let bytes = file.lines.bytes();
            files.push(TreeEntry {
                name: path.as_bytes().to_vec(),
                mode: 0o100644,
                oid: self.repo.write_blob(&bytes)?,
            });
            metadata.insert(path, (file.object, id, &file.lines));
        }
        let files = self.repo.write_tree(&files)?;
        let metadata = self.repo.write_blob(&serde_json::to_vec(&metadata)?)?;
        let namespace = self
            .repo
            .write_blob(&serde_json::to_vec(&snapshot.names)?)?;
        self.envelope(files, metadata, namespace)
    }

    pub(in crate::tree_actor) fn envelope(
        &self,
        files: Oid,
        metadata: Oid,
        namespace: Oid,
    ) -> Result<Oid> {
        Ok(self.repo.write_tree(&[
            TreeEntry {
                name: b"files".to_vec(),
                mode: 0o040000,
                oid: files,
            },
            TreeEntry {
                name: b"metadata.json".to_vec(),
                mode: 0o100644,
                oid: metadata,
            },
            TreeEntry {
                name: b"namespace.json".to_vec(),
                mode: 0o100644,
                oid: namespace,
            },
        ])?)
    }

    pub(in crate::tree_actor) fn object_table_update(
        &self,
        tree: TreeId,
        snapshot: &referee::Snapshot,
        updates: &mut Vec<RefUpdate>,
    ) -> Result<()> {
        let name = refs::object_table(tree);
        let files: BTreeMap<MergeFileIdentity, u64> = snapshot
            .files
            .iter()
            .map(|(id, file)| (*id, file.object))
            .collect();
        let oid = self
            .repo
            .write_blob(&serde_json::to_vec(&ObjectTable { files })?)?;
        updates.push(RefUpdate {
            expected_old: self.pointers.get(&name),
            refname: name,
            new: Some(oid),
        });
        Ok(())
    }

    pub(in crate::tree_actor) fn restore_object_table(
        &self,
        tree: TreeId,
        snapshot: &mut referee::Snapshot,
    ) -> Result<()> {
        if let Some(oid) = self.pointers.get(&refs::object_table(tree)) {
            let table: ObjectTable = serde_json::from_slice(&self.repo.read_blob(oid)?)?;
            for (id, file) in &mut snapshot.files {
                file.object = *table.files.get(id).ok_or(Error::Format)?;
            }
        }
        Ok(())
    }

    /// The commit of every direct save on record, by its event number: one Git
    /// child over everything the pointers reach. An event has one commit, so
    /// no walk from a fork's head is needed to find it. A copy: `owners` is
    /// folded as saves land and is empty-then-rebuilt here at every open.
    pub(in crate::tree_actor) fn rebuild_owners(&mut self) -> Result<()> {
        let _timer = Timer::start(Which::EventOwners);
        self.owners = self
            .repo
            .commit_messages(&refs::owner_tips(&self.pointers))?
            .into_iter()
            .filter_map(|(oid, message)| Some((message::direct_event(&message)?, oid)))
            .collect();
        Ok(())
    }

    /// Writes `t`'s commit and the pointer changes that publish it.
    pub(in crate::tree_actor) fn build_save(
        &self,
        tree: TreeId,
        t: Tree,
        writer: &str,
        action: Action,
        event: EventNumber,
        open: Option<u64>,
    ) -> Result<Draft> {
        let _timer = Timer::start(Which::Objects);
        let mut files = Vec::new();
        for (path, object) in &t.names {
            let blob = self.repo.write_blob(&t.bytes[object])?;
            files.push(TreeEntry {
                name: path.as_bytes().to_vec(),
                mode: 0o100644,
                oid: blob,
            });
        }
        let files = self.repo.write_tree(&files)?;
        let mut metadata = BTreeMap::new();
        for (path, object) in &t.names {
            metadata.insert(path, (object, t.identities[object], &*t.lines[object]));
        }
        let blob = self.repo.write_blob(&serde_json::to_vec(&metadata)?)?;
        let namespace = self.repo.write_blob(&serde_json::to_vec(&t.namespace)?)?;
        let content = self.envelope(files, blob, namespace)?;
        let message = Message::Direct {
            action,
            writer,
            tree,
            event,
            open,
        }
        .render()?;
        let current = self.head(tree)?;
        let head = self.repo.write_commit(content, &[current], &message)?;
        let mut changes = vec![RefUpdate {
            refname: refs::branch(tree),
            new: Some(head),
            expected_old: Some(current),
        }];
        self.object_table_update(tree, &Self::snapshot_of(&t), &mut changes)?;
        let enqueued = self.enqueue_moves(tree, head, event, &mut changes)?;
        let mut draft = Draft::new(writer, tree, t);
        draft.changes = changes;
        draft.enqueued = enqueued;
        draft.saved = Saved::Commit(head);
        Ok(draft)
    }

    // One entry per destination, numbered by the step that causes it: order
    // across restart, independent of commit dates. A tree's head moves to its
    // upstream as a landing and to each tree it is the upstream of as a sync.
    pub(in crate::tree_actor) fn enqueue_moves(
        &self,
        tree: TreeId,
        head: Oid,
        event: EventNumber,
        changes: &mut Vec<RefUpdate>,
    ) -> Result<u64> {
        let mut destinations: Vec<(TreeId, Oid)> = Vec::new();
        if let Some(upstream) = self.upstream_of(tree) {
            destinations.push((upstream, head));
        }
        let downstream: Vec<TreeId> = self.upstreams.forks_of(tree).collect();
        if !downstream.is_empty() {
            let sync = self.repo.write_blob(b"sync")?;
            destinations.extend(downstream.into_iter().map(|fork| (fork, sync)));
        }
        for (destination, oid) in &destinations {
            changes.push(RefUpdate {
                refname: refs::queue_entry(*destination, event),
                new: Some(*oid),
                expected_old: None,
            });
        }
        Ok(destinations.len() as u64)
    }

    pub(in crate::tree_actor) fn checkout(&self, tree: TreeId) -> Result<()> {
        let dir = self.root.join("trees").join(tree.to_string());
        fs::create_dir_all(&dir)?;
        // Rebuild from the branch, never from intermediate in-memory state.
        let commit = self.repo.read_commit(self.head(tree)?)?;
        // The trailer tells the Root commit even if a workload used our names.
        let files = if message::is_envelope(&commit.message) {
            self.repo
                .list_tree(commit.tree)?
                .into_iter()
                .find(|e| e.name == b"files")
                .ok_or(Error::Format)?
                .oid
        } else {
            commit.tree
        };
        self.repo.checkout_tree(files, &dir)?;
        Ok(())
    }
}
