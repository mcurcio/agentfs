//! Where a writer ran (docs/target-architecture/future/one-engine.md, "The
//! trailer"): the pod context, and the writer with it as a commit names them.

use serde::{Deserialize, Serialize};

use crate::name::{Label, WriterLabel};

/// Where a writer ran: the recording engine's pod fields, each optional. A field
/// the launcher lacks is absent, never guessed. Each present field is one
/// trailer: `Agentfs-Namespace`, `Agentfs-Pod`, `Agentfs-Pod-Uid`, `Agentfs-Node`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PodContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_uid: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<Label>,
}

/// A writer and where it ran: what the tree actor journals with each write-open
/// and step, and writes as a commit's `Agentfs-Writer` and pod trailers. A
/// landing merge copies its landed commit's (git-model §3); a sync and the Root
/// commit carry `agentfs` with the history service's own pod context.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WriterAt {
    pub writer: WriterLabel,
    #[serde(default)]
    pub at: PodContext,
}
