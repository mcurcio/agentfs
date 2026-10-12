//! The patch queue's state (design §12): the status ref iac reads, and
//! the local cache in `meta/patch.json`.
//!
//! `refs/agentfs/<volume>/status` holds a commit without parents whose
//! tree is one file, `status.json` ([`StatusDoc`], schema version 1). It
//! is rewritten each time the queue runs and force-pushed by the queue
//! push: it is a snapshot, not history.
//!
//! [`PatchState`] caches what the last run found, for `agentfs status`,
//! the telemetry gauges and the next run's history scan. It is never
//! authoritative: a missing or unreadable file reads as empty, and the
//! `Agentfs-Inbox` trailers on the branch decide what is applied.

use std::fs;
use std::io;
use std::path::Path;

use git2::{Oid, Repository, Signature};
use serde::{Deserialize, Serialize};

use super::{Blocked, Ignored, PatchError, status_ref};

/// The status document's schema version.
pub const STATUS_VERSION: u32 = 1;

/// The file in the status commit's tree.
pub const STATUS_FILE: &str = "status.json";

/// `meta/patch.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PatchState {
    pub version: u32,
    /// The highest inbox number applied on the branch.
    pub last_applied: u64,
    /// The patch and the branch commit of `last_applied`.
    pub last_applied_patch: Option<String>,
    pub last_applied_commit: Option<String>,
    /// The branch head the trailer scan last reached; the next scan stops
    /// there if it is still on the branch.
    pub scanned_head: Option<String>,
    /// Patches queued and not applied.
    pub queue_depth: u64,
    /// Where the queue stopped, if it did.
    pub blocked: Option<Blocked>,
    /// The status commit last written.
    pub status_oid: Option<String>,
    /// When the queue last ran (milliseconds since the Unix epoch).
    pub updated_ms: u64,
    /// The patch being written into the tree: set just before the write,
    /// cleared when the run saves its state. Present at the start of a run
    /// only after a crash between the write and the record (design §12).
    pub in_flight: Option<InFlight>,
}

/// A patch whose write into the tree may have been interrupted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InFlight {
    pub inbox: u64,
    pub patch: String,
    /// The branch head it was merged onto.
    pub head: String,
    /// The paths the write changes.
    pub paths: Vec<String>,
}

impl PatchState {
    /// The state at `path`; empty if it is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Writes atomically.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        crate::fsutil::write_json_atomic(path, self)
    }

    /// One line for `agentfs status`.
    pub fn summary(&self) -> String {
        if self.updated_ms == 0 {
            return "never run".to_owned();
        }
        let mut line = match self.last_applied {
            0 => format!("none applied, {} queued", self.queue_depth),
            n => format!("last applied {n}, {} queued", self.queue_depth),
        };
        match &self.blocked {
            Some(b) => line.push_str(&format!(", blocked at {} ({})", b.inbox, b.reason)),
            None => line.push_str(", ok"),
        }
        line
    }
}

/// The last applied patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedEntry {
    pub inbox: u64,
    pub patch: String,
    /// The branch commit that applied it.
    pub commit: String,
}

/// A patch still queued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub inbox: u64,
    pub patch: String,
}

/// How the fetch before the run went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchStatus {
    pub remote: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// `status.json` (schema in docs/runbooks/patch-queue.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusDoc {
    pub version: u32,
    pub volume: String,
    /// The branch's full ref name.
    pub branch: String,
    /// The branch head after the run.
    pub head: Option<String>,
    /// When the run ended, `YYYYMMDDTHHMMSSZ`.
    pub updated: String,
    pub last_applied: Option<AppliedEntry>,
    /// Applied in this run, in order.
    pub applied_now: Vec<u64>,
    /// Still queued, in order, the blocked patch first.
    pub queue: Vec<QueueEntry>,
    pub blocked: Option<Blocked>,
    pub ignored: Vec<Ignored>,
    /// `None` when nothing was fetched (no patch remote, `--no-fetch`).
    pub fetch: Option<FetchStatus>,
}

impl StatusDoc {
    /// The status commit's subject.
    pub fn subject(&self) -> String {
        let applied = self.last_applied.as_ref().map_or(0, |a| a.inbox);
        let mut s = format!(
            "agentfs patch status: through {applied} applied, {} queued",
            self.queue.len()
        );
        if let Some(b) = &self.blocked {
            s.push_str(&format!(", blocked at {}: {}", b.inbox, b.reason));
        }
        s
    }
}

/// Writes `doc` as a commit without parents holding `status.json`, signed
/// by `signature`, and points the local status ref at it (forced).
pub fn write_status_ref(
    repo: &Repository,
    volume: &str,
    doc: &StatusDoc,
    signature: &Signature<'_>,
) -> Result<Oid, PatchError> {
    let mut json = serde_json::to_vec_pretty(doc).map_err(io::Error::other)?;
    json.push(b'\n');
    let blob = repo.blob(&json)?;
    let mut builder = repo.treebuilder(None)?;
    builder.insert(STATUS_FILE, blob, 0o100644)?;
    let tree = repo.find_tree(builder.write()?)?;
    let commit = repo.commit(
        None,
        signature,
        signature,
        &format!("{}\n", doc.subject()),
        &tree,
        &[],
    )?;
    repo.reference(
        &status_ref(volume),
        commit,
        true,
        "agentfs: patch queue status",
    )?;
    Ok(commit)
}

/// Reads `status.json` from the status commit at `oid`.
pub fn read_status(repo: &Repository, oid: Oid) -> Result<StatusDoc, PatchError> {
    let commit = repo.find_commit(oid)?;
    let entry = commit.tree()?.get_path(Path::new(STATUS_FILE))?;
    let blob = repo.find_blob(entry.id())?;
    serde_json::from_slice(blob.content()).map_err(|e| PatchError::Io(io::Error::other(e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::BlockReason;

    fn doc() -> StatusDoc {
        StatusDoc {
            version: STATUS_VERSION,
            volume: "vol".into(),
            branch: "refs/heads/agentfs/vol".into(),
            head: Some("1".repeat(40)),
            updated: "20261012T120000Z".into(),
            last_applied: Some(AppliedEntry {
                inbox: 1,
                patch: "2".repeat(40),
                commit: "3".repeat(40),
            }),
            applied_now: vec![1],
            queue: vec![
                QueueEntry {
                    inbox: 2,
                    patch: "4".repeat(40),
                },
                QueueEntry {
                    inbox: 3,
                    patch: "5".repeat(40),
                },
            ],
            blocked: Some(Blocked {
                inbox: 2,
                patch: Some("4".repeat(40)),
                reason: BlockReason::Conflict,
                message: "conflicts in b.txt".into(),
                paths: vec!["b.txt".into()],
            }),
            ignored: vec![],
            fetch: Some(FetchStatus {
                remote: "origin".into(),
                ok: true,
                error: None,
            }),
        }
    }

    #[test]
    fn patch_state_round_trips_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("patch.json");
        assert_eq!(PatchState::load(&path), PatchState::default());
        assert_eq!(PatchState::default().summary(), "never run");
        let state = PatchState {
            version: 1,
            last_applied: 1,
            last_applied_patch: None,
            last_applied_commit: None,
            scanned_head: Some("a".repeat(40)),
            queue_depth: 2,
            blocked: doc().blocked,
            status_oid: None,
            updated_ms: 5,
            in_flight: None,
        };
        state.save(&path).unwrap();
        assert_eq!(PatchState::load(&path), state);
        assert_eq!(
            state.summary(),
            "last applied 1, 2 queued, blocked at 2 (conflict)"
        );
        fs::write(&path, "not json").unwrap();
        assert_eq!(PatchState::load(&path), PatchState::default());
    }

    #[test]
    fn status_json_schema() {
        let json: serde_json::Value = serde_json::to_value(doc()).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "applied_now",
                "blocked",
                "branch",
                "fetch",
                "head",
                "ignored",
                "last_applied",
                "queue",
                "updated",
                "version",
                "volume"
            ]
        );
        assert_eq!(json["blocked"]["reason"], "conflict");
        assert_eq!(json["blocked"]["paths"][0], "b.txt");
        assert_eq!(json["queue"][1]["inbox"], 3);
        assert_eq!(json["version"], 1);
    }

    #[test]
    fn the_status_ref_is_a_parentless_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path()).unwrap();
        let sig = Signature::now("agentfs", "agentfs@example").unwrap();
        let first = write_status_ref(&repo, "vol", &doc(), &sig).unwrap();
        let mut second_doc = doc();
        second_doc.blocked = None;
        second_doc.queue.clear();
        let second = write_status_ref(&repo, "vol", &second_doc, &sig).unwrap();
        assert_ne!(first, second);
        let commit = repo.find_commit(second).unwrap();
        assert_eq!(commit.parent_count(), 0);
        assert_eq!(
            commit.summary().unwrap(),
            Some("agentfs patch status: through 1 applied, 0 queued")
        );
        assert_eq!(
            repo.refname_to_id("refs/agentfs/vol/status").unwrap(),
            second
        );
        assert_eq!(read_status(&repo, second).unwrap(), second_doc);
        assert_eq!(
            repo.find_commit(first).unwrap().summary().unwrap(),
            Some("agentfs patch status: through 1 applied, 2 queued, blocked at 2: conflict")
        );
    }
}
