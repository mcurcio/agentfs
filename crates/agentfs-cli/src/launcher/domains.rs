//! One volume's domains restored on one store, and the one dispatcher that
//! hands each domain event to its consumers (docs/trees.md, "Volume
//! lifecycle"): what `mount`, `host` and `fork end` share. It wires domains
//! to adapters and decides nothing.
use std::cell::RefMut;
use std::sync::Arc;

use agentfs_clients::ClientsService;
use agentfs_contracts::caps::{LaunchCap, Minter};
use agentfs_contracts::clients::{
    Access, AttachedLookup as _, Binding, ClientError, ClientEvent, ClientId, ClientSet, Clients,
    Consistency, Grant,
};
use agentfs_contracts::name::{ClientLabel, ForkKey};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::Event;
use agentfs_contracts::trees::{ForkPhase, ForkSettings, TreeError, Trees, TreesEvent};
use agentfs_contracts::volume::{Volume, VolumeEvent};
use agentfs_engine::session::Sessions;
use agentfs_engine::state_area::TreeStateArea;
use agentfs_engine::store::TreeStore;
use agentfs_engine::tree_actor::{Recording, TreeId};
use agentfs_trees::TreesService;
use agentfs_volume::VolumeService;
use anyhow::{Context, Result};

use crate::domain_events::DomainEvents;
use crate::shared::Shared;

pub(super) type Forks = TreesService<TreeStore, DomainEvents>;
pub(super) type ClientsOf = ClientsService<TreeStore, Sessions, Shared<Forks>, DomainEvents>;
/// Clients as Volume asks it: empty until restored after `stage`.
pub(super) type ClientsSlot = Shared<Option<ClientsOf>>;
pub(super) type VolumeOf = VolumeService<TreeStateArea, ClientsSlot, DomainEvents>;

/// One volume's domains, restored on one store, and their event queue.
pub(super) struct Domains {
    /// The mount's Volume, staged; none for `fork end`, which judges no
    /// release.
    pub(super) volume: Option<VolumeOf>,
    pub(super) trees: Shared<Forks>,
    /// Filled by [`Domains::restore`]: never empty once a `Domains` exists.
    pub(super) clients: ClientsSlot,
    pub(super) sessions: Sessions,
    pub(super) events: DomainEvents,
    pub(super) cap: LaunchCap,
    /// The launcher's pod context: every event its calls cause carries it.
    pub(super) at: PodContext,
    /// `VolumeDraining` consumed: the loop has its clients to detach and
    /// views to close.
    pub(super) draining: bool,
    /// `VolumeReleased` consumed: the loop ends.
    pub(super) released: bool,
}

impl Domains {
    /// Trees and Clients restored over `recording`, as every start restores
    /// them, publishing to `events`; Clients fills `clients`, which `volume`
    /// asks. `cap` is the launcher every call is made as.
    pub(super) fn restore(
        recording: Arc<Recording>,
        (minter, cap, at): (Minter, LaunchCap, PodContext),
        events: DomainEvents,
        clients: ClientsSlot,
        volume: Option<VolumeOf>,
    ) -> Result<Self> {
        let store = TreeStore::new(Arc::clone(&recording));
        let trees = Shared::new(
            TreesService::restore(store.clone(), events.clone())
                .context("reading the forks' rows")?,
        );
        let sessions = Sessions::new(recording);
        let restored = ClientsService::restore(
            store,
            sessions.clone(),
            trees.clone(),
            events.clone(),
            minter,
        )
        .context("reading the clients' rows")?;
        *clients.get_mut() = Some(restored);
        Ok(Self {
            volume,
            trees,
            clients,
            sessions,
            events,
            cap,
            at,
            draining: false,
            released: false,
        })
    }

    /// Clients, restored.
    pub(super) fn clients(&self) -> RefMut<'_, ClientsOf> {
        RefMut::map(self.clients.get_mut(), |clients| {
            clients
                .as_mut()
                .expect("Domains::restore fills Clients before it answers")
        })
    }

    /// The one dispatcher: each domain event, in order, to its consumers,
    /// after the call that caused it returns. `LastDetached` settles Trees
    /// (an unkeyed fork no client is attached to ends); `ForkDropped`
    /// releases its writer claim; `VolumeDraining` stops
    /// attaches and hands the loop its clients to detach, `VolumeStaged`
    /// allows them again; each end re-judges the release, and
    /// `VolumeReleased` ends the loop. A consumer's failure is logged: the
    /// next boot re-judges from asked facts.
    pub(super) fn dispatch(&mut self) {
        while let Some(event) = self.events.next() {
            let handled = match event {
                Event::Clients(ClientEvent::LastDetached { .. }) => {
                    self.settle_trees().and_then(|()| self.rejudge())
                }
                Event::Clients(ClientEvent::AttachmentEnded { .. }) => self.rejudge(),
                Event::Trees(TreesEvent::ForkDropped(fork)) => {
                    let released = self.clients().on_fork_dropped(fork);
                    released.map_err(|e| e.to_string())
                }
                Event::Volume(VolumeEvent::VolumeDraining(_)) => {
                    self.clients().on_volume_draining();
                    self.draining = true;
                    Ok(())
                }
                Event::Volume(VolumeEvent::VolumeStaged) => {
                    self.clients().on_volume_staged();
                    Ok(())
                }
                Event::Volume(VolumeEvent::VolumeReleased(_)) => {
                    self.released = true;
                    Ok(())
                }
                _ => Ok(()),
            };
            if let Err(e) = handled {
                eprintln!("tree EventFailed: {e}");
            }
        }
    }

    /// Volume re-judges the release; nothing without a Volume.
    pub(super) fn rejudge(&mut self) -> Result<(), String> {
        match self.volume.as_mut() {
            Some(volume) => volume.rejudge().map(drop).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    /// The Stop drain begins: `Staged` → `Draining`. A failure is logged and
    /// the volume stays staged; the next stop asks again.
    pub(super) fn unstage(&mut self) {
        if let Some(volume) = self.volume.as_mut()
            && let Err(e) = volume.unstage(&self.cap)
        {
            eprintln!("tree UnstageFailed: {e}");
        }
        self.dispatch();
    }

    /// Whether the tree actor has stopped after a recording failure.
    pub(super) fn stopped(&self) -> bool {
        self.sessions.recording().actor().is_err()
    }

    /// Trees' settle, asking Clients who is attached.
    pub(super) fn settle_trees(&mut self) -> Result<(), String> {
        let clients = self.clients();
        let settled = self.trees.get_mut().settle(&*clients);
        settled.map(drop).map_err(|e| e.to_string())
    }

    /// Clients' re-judgement: this launcher's clients not in `live` end, and
    /// dropped forks' claims are released. A failure raises the alarm; the
    /// next reconcile (the drain's, or the next boot's) asks again. Answers
    /// how many clients it ended.
    pub(super) fn reconcile(&mut self, live: &ClientSet) -> u32 {
        let reconciled = self.clients().reconcile(live, &self.cap, &self.at);
        let ended = match reconciled {
            Ok(ended) => u32::try_from(ended.len()).unwrap_or(u32::MAX),
            Err(e) => {
                eprintln!("tree ReconcileFailed: {e}");
                0
            }
        };
        self.dispatch();
        ended
    }

    /// Logs, at a drain's start, the attached clients of other launchers:
    /// the release waits for them, and only their own launcher ends them
    /// (docs/runbooks/csi-recovery.md, "A host never exits").
    pub(super) fn log_foreign_clients(&self) {
        let ours = self.cap.launcher();
        let foreign: Vec<String> = self
            .clients()
            .attached_any()
            .into_iter()
            .filter(|client| client.launcher != *ours)
            .map(|client| client.to_string())
            .collect();
        if !foreign.is_empty() {
            eprintln!(
                "tree DrainBlocked: the release waits for client(s) of another launcher to end: \
                 {}; start that launcher on this volume again and stop it, which ends them",
                foreign.join(", ")
            );
        }
    }

    /// One view's client on a new session, with `at` its pod context: the
    /// restored client of this launcher, tree, label and access rebound
    /// (its view host died with the process), else a new one attached
    /// (live, consistent per file).
    pub(super) fn bind(
        &mut self,
        tree: TreeId,
        label: ClientLabel,
        access: Access,
        at: PodContext,
    ) -> Result<Binding, ClientError> {
        let session = self.sessions.open();
        let resumable = self.clients().resumable(&self.cap, tree, &label, access);
        let answered = match resumable {
            Some(client) => self.clients().rebind(&client, at, session, &self.cap),
            None => {
                let grant = match access {
                    Access::ReadWrite => Grant::live_read_write(tree, Consistency::PerFile),
                    Access::ReadOnly => Grant::live_read_only(tree, Consistency::PerFile),
                };
                self.clients().attach(grant, label, at, session, &self.cap)
            }
        };
        self.dispatch();
        answered
    }

    /// Ends every unkeyed fork no client is attached to and drops every
    /// ended fork nothing holds, then re-judges the release: at boot and
    /// after each landing.
    pub(super) fn settle(&mut self) {
        if let Err(e) = self.settle_trees() {
            eprintln!("tree SettleFailed: {e}");
        }
        self.dispatch();
        if let Err(e) = self.rejudge() {
            eprintln!("tree EventFailed: {e}");
        }
        self.dispatch();
    }

    /// A fork of `upstream`: the open one under its key, or a new one. Its
    /// branch and its row are one step, durable when this returns.
    pub(super) fn fork(
        &mut self,
        upstream: TreeId,
        key: Option<ForkKey>,
        settings: ForkSettings,
    ) -> Result<TreeId, TreeError> {
        let fork = self
            .trees
            .get_mut()
            .create(upstream, key, settings, &self.cap, &self.at);
        self.dispatch();
        Ok(fork?.into())
    }

    /// Ends the keyed fork under `key`: it drops once nothing holds it.
    pub(super) fn end_keyed(&mut self, key: &ForkKey) -> Result<(), TreeError> {
        let ended = self.trees.get_mut().end_keyed(key, &self.cap, &self.at);
        self.dispatch();
        ended
    }

    /// The fork under `key` and its phase, whatever the phase; never made.
    pub(super) fn keyed(&self, key: &ForkKey) -> Option<(TreeId, ForkPhase)> {
        let trees = self.trees.get_mut();
        trees
            .keyed(key)
            .map(|facts| (facts.fork.into(), facts.phase))
    }

    /// Detaches each client, its opens saved first; a failure is logged (the
    /// next boot's reconcile ends it).
    pub(super) fn detach_all(&mut self, ids: &[ClientId]) {
        for client in ids {
            let detached = self.clients().detach(client, &self.cap, &self.at);
            if let Err(e) = detached {
                eprintln!("tree DetachFailed: client={client}: {e}");
            }
            self.dispatch();
        }
    }
}
