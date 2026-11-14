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

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agentfs_contracts::audit::{Audited, Principal, Stamp, UnixNanos};
use agentfs_contracts::name::{ForkKey, LauncherId};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{ForkStore, QueueFacts, StoreError};
use agentfs_contracts::tree::{ForkId, TreeId};
use agentfs_contracts::trees::{ForkFacts, ForkPhase, ForkSettings};
use agentfs_contracts::volume::IoKind;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::tree_actor::{self, Chain, Recording, RowDomain, RowEvent, RowWrite, TreeActor};

/// The kind of the event that creates a fork's row.
const FORK_CREATED: &str = "ForkCreated";

/// The domains' stores over one served volume: Trees' rows and the queue
/// facts now; Clients' in slice S4, Volume's in S6.
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
        expect(dead_code, reason = "slices S4 and S6 write rows alone through it")
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

/// `value` as a chain's next row: `created` kept from `previous`, the chain's
/// current row, or now and `by` on its first write; `changed` now and `by`.
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
    let changed = Stamp {
        at: now,
        by: by.clone(),
    };
    let created = match previous {
        Some(row) => serde_json::from_str::<Created>(row)?.created,
        None => changed.clone(),
    };
    Ok(Audited {
        value,
        created,
        changed,
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

/// The row write of `audited` on `chain`, with its event (kind, detail).
fn row_write<T: Serialize>(
    chain: Chain,
    audited: &Audited<T>,
    (kind, detail): (&'static str, String),
    by: &Principal,
    at: &PodContext,
) -> Result<RowWrite, tree_actor::Error> {
    Ok(RowWrite {
        chain,
        row: serde_json::to_string(audited)?,
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
