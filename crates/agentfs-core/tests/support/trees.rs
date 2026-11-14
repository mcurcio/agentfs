//! The tree actor tests' volumes, one line each: a new volume, a reopen, and a
//! writer with an empty pod context.

#![allow(dead_code, reason = "each test file uses what it needs")]

use std::path::Path;

use agentfs_contracts::name::{VolumeName, WriterLabel};
use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::volume::{VolumeId, VolumeIdentity};
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

/// Opens the volume again. After a kill, an orphaned Git child of the killed
/// process (the builder's `git fast-import`, which holds the state area's lease
/// until its input closes and it exits) can hold the lease a moment longer: that
/// open answers `Busy`, and this tries again, for up to ten seconds.
pub fn reopen(dir: &Path) -> TreeActor {
    let mut tries = 0;
    loop {
        match TreeActor::open(dir, &PodContext::default()) {
            Err(agentfs_core::tree_actor::Error::Busy) if tries < 200 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            result => return result.expect("reopens"),
        }
    }
}

/// `label` with an empty pod context.
pub fn writer(label: &str) -> WriterAt {
    WriterAt {
        writer: WriterLabel::parse(label).expect("a writer"),
        at: PodContext::default(),
    }
}
