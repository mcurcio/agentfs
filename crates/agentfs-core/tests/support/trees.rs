//! The tree actor tests' volumes, one line each: a new volume, a reopen, and a
//! writer with an empty pod context.

#![allow(dead_code, reason = "each test file uses what it needs")]

use std::path::Path;

use agentfs_contracts::name::{VolumeName, WriterLabel};
use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::volume::{VolumeId, VolumeIdentity};
use agentfs_core::file_lock::{FileLock, Mode};
use agentfs_core::tree_actor::TreeActor;

/// The identity [`new_actor`] gives: `test`, with a fixed id.
pub fn identity() -> VolumeIdentity {
    VolumeIdentity {
        name: VolumeName::parse("test").expect("a safe name"),
        id: VolumeId(uuid::Uuid::from_u128(1)),
    }
}

/// A new volume [`identity`], served with an empty pod context.
pub fn new_actor(dir: &Path) -> TreeActor {
    TreeActor::create(dir, &identity(), &PodContext::default()).expect("creates")
}

/// Opens the volume again.
pub fn reopen(dir: &Path) -> TreeActor {
    TreeActor::open(dir, &PodContext::default()).expect("reopens")
}

/// Returns once no process holds the state area's lease. A killed process's
/// tree Git children (the builder's `git fast-import`) share its lease and die
/// of their parent-death signal after it (`tree_git`'s `attach_lease`), so the
/// lease can outlive the process a test waited for. Taking the lease, blocking,
/// is the event that every holder is gone; it is released at once.
pub fn wait_for_lease(dir: &Path) {
    let file = FileLock::open(&dir.join(agentfs_core::tree_actor::LOCK)).expect("opens the lease");
    drop(FileLock::wait(file, Mode::Exclusive).expect("takes the lease"));
}

/// [`reopen`] after the process that served the volume was killed.
pub fn reopen_after_kill(dir: &Path) -> TreeActor {
    wait_for_lease(dir);
    reopen(dir)
}

/// `label` with an empty pod context.
pub fn writer(label: &str) -> WriterAt {
    WriterAt {
        writer: WriterLabel::parse(label).expect("a writer"),
        at: PodContext::default(),
    }
}

/// Where a file named `name` sits under `files/`, `lines/` and a checkout:
/// `<aa>/<bb>/<name>`, `aa` and `bb` the first four hex digits of the name's
/// blob id, as `git hash-object --stdin` gives it (docs/trees.md).
pub fn fanned(name: &str) -> String {
    let hex = git2::Oid::hash_object(git2::ObjectType::Blob, name.as_bytes())
        .expect("hashes")
        .to_string();
    format!("{}/{}/{name}", &hex[..2], &hex[2..4])
}

/// Tree `tree`'s checkout of `name` in the state area `dir`.
pub fn checkout(dir: &Path, tree: &str, name: &str) -> std::path::PathBuf {
    dir.join("trees").join(tree).join(fanned(name))
}
