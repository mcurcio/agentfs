//! The domains' rows over the volume's one tree actor (docs/trees.md, "Rows
//! and event commits"; docs/library/core.md): the store ports of
//! `agentfs_contracts::ports`. Each write is one journal step, the volume's
//! one commit point, durable before it returns (one group-commit fsync, off
//! the actor's lock); its event commit on the row's chain trails, built with
//! the step's batch. Rows are read from the actor's saved rows, never Git.
//!
//! The audit stamps' one home: a row's first write sets `created` and
//! `changed` to now and its principal; every later write keeps `created`
//! from the chain's current row and sets `changed`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agentfs_contracts::audit::{Audited, Principal, Stamp, UnixNanos};
use agentfs_contracts::clients::{AttachmentEndReason, ClientId, Ordinal};
use agentfs_contracts::name::{ForkKey, LauncherId};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{
    ClientRow, ClientStore, ForkStore, NewClient, QueueFacts, StoreError, TreeClients,
};
use agentfs_contracts::tree::{ForkId, TreeId};
use agentfs_contracts::trees::{ForkFacts, ForkPhase, ForkSettings};
use agentfs_contracts::volume::IoKind;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::tree_actor::{self, Chain, Recording, RowDomain, RowEvent, RowWrite, TreeActor};

/// The kind of the event that creates a fork's row.
const FORK_CREATED: &str = "ForkCreated";
/// The kinds of the events on a tree's Clients chain.
const ATTACHED: &str = "Attached";
const REBOUND: &str = "Rebound";
const ATTACHMENT_ENDED: &str = "AttachmentEnded";
const LAST_DETACHED: &str = "LastDetached";

/// The domains' stores over one served volume: Trees' and Clients' rows and
/// the queue facts now; Volume's in S6.
#[derive(Clone)]
pub struct TreeStore {
    recording: Arc<Recording>,
}

impl TreeStore {
    pub fn new(recording: Arc<Recording>) -> Self {
        Self { recording }
    }

    /// One write: under the actor's lock, `step` reads what it needs and runs
    /// its one step; then, unlocked, every record appended is made durable.
    fn write<R>(
        &self,
        step: impl FnOnce(&mut TreeActor, UnixNanos) -> Result<R, tree_actor::Error>,
    ) -> Result<R, StoreError> {
        let written = {
            let mut actor = self.recording.actor().map_err(|e| store_error(&e))?;
            step(&mut actor, now()).map_err(|e| store_error(&e))?
        };
        self.recording.durable_all().map_err(|e| store_error(&e))?;
        Ok(written)
    }

    /// One step of one row alone on `chain`, stamped over its current row.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "slice S6 writes the volume's row alone through it"
        )
    )]
    fn write_row<T: Serialize + DeserializeOwned>(
        &self,
        chain: Chain,
        value: T,
        event: (&'static str, String),
        by: &Principal,
        at: &PodContext,
    ) -> Result<Audited<T>, StoreError> {
        self.write(|actor, now| {
            let previous = actor.row(chain);
            let audited = stamped(previous.as_deref(), value, by, now)?;
            let write = row_write(chain, &audited, event, by, at)?;
            #[expect(clippy::disallowed_methods, reason = "the store's own row step")]
            actor.record_rows(|_| Ok(vec![write]))?;
            Ok(audited)
        })
    }

    /// Every row of `domain`, parsed.
    fn load<T: DeserializeOwned>(&self, domain: RowDomain) -> Result<Vec<Audited<T>>, StoreError> {
        let rows = self
            .recording
            .actor()
            .map_err(|e| store_error(&e))?
            .rows(domain);
        rows.iter()
            .map(|(chain, row)| {
                serde_json::from_str(row)
                    .map_err(|e| StoreError::Damaged(format!("{chain:?}'s row: {e}")))
            })
            .collect()
    }

    /// A question of the actor; a stopped actor answers `none`.
    fn ask<R>(&self, none: R, question: impl FnOnce(&TreeActor) -> R) -> R {
        match self.recording.actor() {
            Ok(actor) => question(&actor),
            Err(_) => none,
        }
    }
}

fn now() -> UnixNanos {
    UnixNanos(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)),
    )
}

/// The audit stamps' one home: `value` with `created` kept from its previous
/// write, or now and `by` on its first; `changed` now and `by`.
fn stamp<T>(created: Option<&Stamp>, value: T, by: &Principal, now: UnixNanos) -> Audited<T> {
    let changed = Stamp {
        at: now,
        by: by.clone(),
    };
    Audited {
        value,
        created: created.cloned().unwrap_or_else(|| changed.clone()),
        changed,
    }
}

/// `value` as a chain's next row, stamped over `previous`, the chain's
/// current row (none on its first write).
fn stamped<T>(
    previous: Option<&str>,
    value: T,
    by: &Principal,
    now: UnixNanos,
) -> Result<Audited<T>, tree_actor::Error> {
    #[derive(serde::Deserialize)]
    struct Created {
        created: Stamp,
    }
    let created = previous
        .map(serde_json::from_str::<Created>)
        .transpose()?
        .map(|row| row.created);
    Ok(stamp(created.as_ref(), value, by, now))
}

/// A tree's Clients row as the journal leaves it; an empty one before its
/// first attach.
fn clients_row(actor: &TreeActor, tree: TreeId) -> Result<TreeClients, tree_actor::Error> {
    Ok(match actor.row(Chain::Clients(tree)) {
        Some(row) => serde_json::from_str(&row)?,
        None => TreeClients {
            tree,
            attached: Vec::new(),
            writer_claim: None,
        },
    })
}

/// A principal as an event commit's `Agentfs-By` names it.
fn principal(by: &Principal) -> String {
    match by {
        Principal::Client(client) => format!("client {client}"),
        Principal::Launcher(launcher) => format!("launcher {launcher}"),
        Principal::Agentfs => "agentfs".to_owned(),
    }
}

/// The row write of `row` on `chain`, with its event (kind, detail).
fn row_write<T: Serialize>(
    chain: Chain,
    row: &T,
    (kind, detail): (&'static str, String),
    by: &Principal,
    at: &PodContext,
) -> Result<RowWrite, tree_actor::Error> {
    Ok(RowWrite {
        chain,
        row: serde_json::to_string(row)?,
        event: RowEvent {
            kind,
            detail,
            by: principal(by),
            at: at.clone(),
        },
    })
}

fn io_kind(errno: i32) -> IoKind {
    match errno {
        libc::ENOSPC => IoKind::NoSpace,
        errno => IoKind::Other(errno),
    }
}

/// The ports' word for a tree actor's failure.
fn store_error(error: &tree_actor::Error) -> StoreError {
    use tree_actor::Error as E;
    match error {
        E::Stopped => StoreError::Stopped,
        // Unknown whether the step survived: the reopen decides.
        E::Durable(_) => StoreError::Stopped,
        E::Format | E::CorruptJournal(_) | E::MissingObject { .. } | E::Json(_) => {
            StoreError::Damaged(error.to_string())
        }
        E::NotFound | E::Busy | E::PointerMoved { .. } => StoreError::Refused(error.to_string()),
        // Nothing recorded.
        other => StoreError::NotDurable(io_kind(other.errno())),
    }
}

impl ForkStore for TreeStore {
    fn load(&self) -> Result<Vec<Audited<ForkFacts>>, StoreError> {
        TreeStore::load(self, RowDomain::Trees)
    }

    fn create(
        &mut self,
        upstream: TreeId,
        key: Option<ForkKey>,
        settings: ForkSettings,
        created_by: &LauncherId,
        at: &PodContext,
    ) -> Result<Audited<ForkFacts>, StoreError> {
        let by = Principal::Launcher(created_by.clone());
        self.write(|actor, now| {
            let mut created = None;
            #[expect(clippy::disallowed_methods, reason = "the store's own fork step")]
            actor.fork_with(upstream, |fork, _| {
                let id = fork.fork().ok_or(tree_actor::Error::Format)?;
                let facts = ForkFacts {
                    fork: id,
                    upstream,
                    created_by: created_by.clone(),
                    key,
                    settings,
                    phase: ForkPhase::Open,
                };
                // A fork's chain is new: fork numbers are never reused.
                let audited = stamped(None, facts, &by, now)?;
                let write = row_write(
                    Chain::Trees(id),
                    &audited,
                    (FORK_CREATED, format!("{fork} of {upstream}")),
                    &by,
                    at,
                )?;
                created = Some(audited);
                Ok(vec![write])
            })?;
            created.ok_or(tree_actor::Error::Format)
        })
    }

    fn end(&mut self, fork: ForkId, by: &Principal, at: &PodContext) -> Result<(), StoreError> {
        let _ = (fork, by, at);
        todo!("S5")
    }

    fn drop_fork(
        &mut self,
        fork: ForkId,
        by: &Principal,
        at: &PodContext,
    ) -> Result<(), StoreError> {
        let _ = (fork, by, at);
        todo!("S5")
    }
}

/// Each tree's row on `refs/agentfs/clients/<tree>`: its attached clients and
/// a fork's writer claim, each entry stamped. One step per call.
impl ClientStore for TreeStore {
    fn load(&self) -> Result<Vec<TreeClients>, StoreError> {
        let rows = self
            .recording
            .actor()
            .map_err(|e| store_error(&e))?
            .rows(RowDomain::Clients);
        rows.iter()
            .map(|(chain, row)| {
                serde_json::from_str(row)
                    .map_err(|e| StoreError::Damaged(format!("{chain:?}'s row: {e}")))
            })
            .collect()
    }

    fn attach(
        &mut self,
        client: &NewClient,
        claim: bool,
        by: &Principal,
    ) -> Result<ClientId, StoreError> {
        let tree = client.grant.tree();
        self.write(|actor, now| {
            let mut row = clients_row(actor, tree)?;
            let mut attached = None;
            #[expect(clippy::disallowed_methods, reason = "the store's own row step")]
            actor.record_rows(|step| {
                // Its ordinal is its attach's step: unique, never reused.
                let id = ClientId {
                    launcher: client.launcher.clone(),
                    tree,
                    label: client.label.clone(),
                    n: Ordinal(step.get()),
                };
                let entry = ClientRow {
                    id: id.clone(),
                    grant: client.grant.clone(),
                    at: client.at.clone(),
                };
                row.attached.push(stamp(None, entry, by, now));
                if claim {
                    row.writer_claim = Some(stamp(None, id.clone(), by, now));
                }
                let write = row_write(
                    Chain::Clients(tree),
                    &row,
                    (ATTACHED, id.to_string()),
                    by,
                    &client.at,
                )?;
                attached = Some(id);
                Ok(vec![write])
            })?;
            attached.ok_or(tree_actor::Error::Format)
        })
    }

    fn rebind(
        &mut self,
        client: &ClientId,
        at: &PodContext,
        by: &Principal,
    ) -> Result<(), StoreError> {
        self.write(|actor, now| {
            let mut row = clients_row(actor, client.tree)?;
            let entry = row
                .attached
                .iter_mut()
                .find(|entry| entry.value.id == *client)
                .ok_or(tree_actor::Error::NotFound)?;
            let value = ClientRow {
                at: at.clone(),
                ..entry.value.clone()
            };
            *entry = stamp(Some(&entry.created), value, by, now);
            let write = row_write(
                Chain::Clients(client.tree),
                &row,
                (REBOUND, client.to_string()),
                by,
                at,
            )?;
            #[expect(clippy::disallowed_methods, reason = "the store's own row step")]
            actor.record_rows(|_| Ok(vec![write]))?;
            Ok(())
        })
    }

    fn end(
        &mut self,
        ended: &[(ClientId, AttachmentEndReason)],
        emptied: &[TreeId],
        by: &Principal,
        at: &PodContext,
    ) -> Result<(), StoreError> {
        if ended.is_empty() && emptied.is_empty() {
            return Ok(());
        }
        self.write(|actor, _| {
            let mut rows = BTreeMap::new();
            for tree in ended
                .iter()
                .map(|(c, _)| c.tree)
                .chain(emptied.iter().copied())
            {
                if let std::collections::btree_map::Entry::Vacant(row) = rows.entry(tree) {
                    row.insert(clients_row(actor, tree)?);
                }
            }
            let mut writes = Vec::with_capacity(ended.len() + emptied.len());
            for (client, why) in ended {
                let row = rows
                    .get_mut(&client.tree)
                    .ok_or(tree_actor::Error::Format)?;
                let before = row.attached.len();
                row.attached.retain(|entry| entry.value.id != *client);
                if row.attached.len() == before {
                    return Err(tree_actor::Error::NotFound);
                }
                writes.push(row_write(
                    Chain::Clients(client.tree),
                    &*row,
                    (ATTACHMENT_ENDED, format!("{client} {why:?}")),
                    by,
                    at,
                )?);
            }
            for tree in emptied {
                let row = rows.get(tree).ok_or(tree_actor::Error::Format)?;
                writes.push(row_write(
                    Chain::Clients(*tree),
                    row,
                    (LAST_DETACHED, tree.to_string()),
                    by,
                    at,
                )?);
            }
            #[expect(clippy::disallowed_methods, reason = "the store's own row step")]
            actor.record_rows(|_| Ok(writes))?;
            Ok(())
        })
    }

    fn release_claim(&mut self, fork: ForkId, by: &Principal) -> Result<(), StoreError> {
        let _ = (fork, by);
        todo!("S5")
    }
}

/// The actor's queue facts, from the journal's pointers: never a wait for
/// history. A stopped actor answers empty and `false`; what follows fails
/// `Stopped`.
impl QueueFacts for TreeStore {
    fn queued(&self) -> Vec<TreeId> {
        self.ask(Vec::new(), TreeActor::queued)
    }

    fn pending_from(&self, fork: ForkId) -> bool {
        self.ask(false, |actor| actor.pending_from(fork.into()))
    }

    fn has_child(&self, fork: ForkId) -> bool {
        self.ask(false, |actor| actor.has_child(fork.into()))
    }
}

#[cfg(test)]
mod tests {
    use agentfs_contracts::name::VolumeName;
    use agentfs_contracts::volume::{VolumeId, VolumeIdentity};

    use super::*;

    fn identity() -> VolumeIdentity {
        VolumeIdentity {
            name: VolumeName::parse("test").unwrap(),
            id: VolumeId(uuid::Uuid::from_u128(1)),
        }
    }

    fn launcher(name: &str) -> Principal {
        Principal::Launcher(LauncherId::parse(name).unwrap())
    }

    /// `created` is set once, by the first write, and kept by every later
    /// write, through a compaction and a reopen.
    #[test]
    fn created_survives_a_change_and_a_compaction() {
        let tmp = tempfile::tempdir().unwrap();
        let recording = Recording::create(tmp.path(), &identity(), &PodContext::default()).unwrap();
        let store = TreeStore::new(Arc::clone(&recording));
        let at = PodContext::default();
        let first = store
            .write_row(
                Chain::Volume,
                "staged".to_owned(),
                ("VolumeStaged", String::new()),
                &launcher("first"),
                &at,
            )
            .unwrap();
        let second = store
            .write_row(
                Chain::Volume,
                "draining".to_owned(),
                ("VolumeDraining", String::new()),
                &launcher("second"),
                &at,
            )
            .unwrap();
        assert_eq!(second.created, first.created);
        assert_eq!(second.changed.by, launcher("second"));
        recording.settle().unwrap();
        drop(store);
        drop(recording);
        // Every step built: the reopen compacts to one checkpoint.
        let recording = Recording::open(tmp.path(), &PodContext::default()).unwrap();
        let segments = std::fs::read_dir(tmp.path().join("journal"))
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|e| e == "seg")
            })
            .count();
        assert_eq!(segments, 1, "compacted to its checkpoint's segment");
        let rows: Vec<Audited<String>> = TreeStore::new(recording).load(RowDomain::Volume).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, "draining");
        assert_eq!(rows[0].created, first.created);
        assert_eq!(rows[0].changed, second.changed);
        assert_eq!(rows[0].created.by, launcher("first"));
    }

    #[test]
    fn each_failure_has_its_ports_word() {
        use tree_actor::Error as E;
        assert_eq!(store_error(&E::Stopped), StoreError::Stopped);
        assert!(matches!(store_error(&E::Format), StoreError::Damaged(_)));
        assert!(matches!(
            store_error(&E::CorruptJournal(3)),
            StoreError::Damaged(_)
        ));
        assert!(matches!(store_error(&E::NotFound), StoreError::Refused(_)));
        assert!(matches!(store_error(&E::Busy), StoreError::Refused(_)));
        assert_eq!(
            store_error(&E::Io(std::io::Error::from_raw_os_error(libc::ENOSPC))),
            StoreError::NotDurable(IoKind::NoSpace)
        );
        assert_eq!(
            store_error(&E::Io(std::io::Error::from_raw_os_error(libc::EIO))),
            StoreError::NotDurable(IoKind::Other(libc::EIO))
        );
    }

    #[test]
    fn a_principal_is_its_by_trailers_text() {
        assert_eq!(principal(&launcher("cli")), "launcher cli");
        assert_eq!(principal(&Principal::Agentfs), "agentfs");
    }
}
