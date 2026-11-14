//! Capabilities and their minter (docs/target-architecture/system.md, "Authority:
//! who mints what"). Fields are private: a capability exists only because the
//! composition root, or the one domain it lends a [`Minter`] to, made it.

use crate::clients::ClientId;
use crate::name::LauncherId;
use crate::pod::WriterAt;
use crate::tree::TreeId;

/// The one source of capabilities. Built only in a composition root (the
/// `agentfs` binary's `main`); `clippy.toml` bans `Minter::new` elsewhere from
/// slice S1, so a stray construction fails the build.
#[derive(Debug)]
pub struct Minter(());

impl Minter {
    /// Only a composition root calls this.
    #[allow(
        clippy::new_without_default,
        reason = "a Default would be a second, unbanned way to mint"
    )]
    pub fn new() -> Self {
        Self(())
    }
}

/// What a launcher may do (clients.md, C2; volume.md, VO6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaunchScope {
    /// The volume's own launcher: ensure, stage, unstage, attach anywhere.
    Root,
    /// Only forks it created; never the root, the volume or another's fork.
    ForksItCreates,
}

/// A launcher's capability. Until Rules (M3) every launcher's view profiles are
/// all of them, so it holds none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LaunchCap {
    launcher: LauncherId,
    may: LaunchScope,
}

impl LaunchCap {
    pub fn mint(_: &Minter, launcher: LauncherId, may: LaunchScope) -> Self {
        Self { launcher, may }
    }

    pub fn launcher(&self) -> &LauncherId {
        &self.launcher
    }

    pub fn may(&self) -> LaunchScope {
        self.may
    }
}

/// A read-write client's authority to write its own tree, as its own writer
/// (clients.md, C1 and C5). Minted only by Clients' `attach`, with the
/// [`Minter`] the composition root lends it, and handed straight to the
/// session it binds; a read-only client has none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientCap {
    client: ClientId,
    tree: TreeId,
    writes_as: WriterAt,
}

impl ClientCap {
    pub fn mint(_: &Minter, client: ClientId, tree: TreeId, writes_as: WriterAt) -> Self {
        Self {
            client,
            tree,
            writes_as,
        }
    }

    pub fn client(&self) -> &ClientId {
        &self.client
    }

    pub fn tree(&self) -> TreeId {
        self.tree
    }

    /// The `Agentfs-Writer` text and pod context every commit of this client carries.
    pub fn writes_as(&self) -> &WriterAt {
        &self.writes_as
    }
}
