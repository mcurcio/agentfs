//! Which LFS objects a push must upload first (design §7): those the
//! commits the remote lacks reference ([`super::referenced::in_history`]).
//!
//! Every commit not yet on the remote (`head ^ last_pushed`, or the whole
//! branch when the remote has nothing or its last pushed commit is
//! unknown) is diffed against its first parent, without rename detection.
//! An added or modified blob is an object to upload when its path is in
//! that commit's managed `.gitattributes` block (which agentfs wrote) and
//! it is a canonical pointer.

use git2::{Oid, Repository};

use super::{LfsError, Pointer};
use crate::paths::RepoPath;

/// The objects the commits in `head ^ last_pushed` reference, each with
/// one path that references it, deduplicated and sorted by oid.
pub fn objects_to_upload(
    repo: &Repository,
    head: Oid,
    last_pushed: Option<Oid>,
) -> Result<Vec<(Pointer, RepoPath)>, LfsError> {
    super::referenced::in_history(repo, head, last_pushed).map_err(|e| {
        LfsError::local(format!(
            "finding the LFS objects of the unpushed commits: {}",
            e.message
                .trim_start_matches("finding the LFS objects of the commits: ")
        ))
    })
}
