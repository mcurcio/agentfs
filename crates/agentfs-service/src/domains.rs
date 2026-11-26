//! One volume's domains restored on one store, and the one dispatcher that
//! hands each domain event to its consumers (docs/library/service.md): what
//! `Domains::stage` serves, and what `end_fork` restores. It wires domains
//! to adapters and decides nothing.
use std::cell::RefMut;
use std::path::Path;
use std::sync::Arc;

use agentfs_clients::ClientsService;
use agentfs_contracts::TreeId;
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
use agentfs_engine::operator_settings::OperatorSettings;
use agentfs_engine::session::Sessions;
use agentfs_engine::state_area::TreeStateArea;
use agentfs_engine::store::TreeStore;
use agentfs_engine::tree_actor::Recording;
use agentfs_trees::TreesService;
use agentfs_volume::VolumeService;

use crate::error::ServiceError;
use crate::events::DomainEvents;
use crate::launch::Launcher;
use crate::shared::Shared;

type Forks = TreesService<TreeStore, DomainEvents>;
type ClientsOf = ClientsService<TreeStore, Sessions, Shared<Forks>, DomainEvents>;
/// Clients as Volume asks it: empty until restored after `stage`.
pub(crate) type ClientsSlot = Shared<Option<ClientsOf>>;
type VolumeOf = VolumeService<TreeStateArea, ClientsSlot, DomainEvents>;

/// The volume the state area opened, served. An invariant: `open` keeps
/// what it recovered, so a state area that opened serves it.
pub(crate) fn served(area: &TreeStateArea) -> Arc<Recording> {
    area.recording()
        .expect("an opened state area serves its volume (TreeStateArea::open keeps it)")
}

/// Whether [`Domains::unstage`] began the drain.
#[must_use = "a begun drain closes every view the caller serves"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unstaged {
    /// The volume is draining: the caller closes every view it serves.
    Draining,
    /// Nothing began: the volume was not staged, or its unstage failed (a
    /// `tree UnstageFailed` line).
    Unchanged,
}

/// One volume's domains, restored on one store, and their event queue. Not
/// `Send`: the services and their queue are shared on one thread.
pub struct Domains {
    /// The volume, staged; none for `end_fork`, which judges no release.
    volume: Option<VolumeOf>,
    trees: Shared<Forks>,
    /// Filled by [`Domains::restore`]: never empty once a `Domains` exists.
    clients: ClientsSlot,
    sessions: Sessions,
    events: DomainEvents,
    cap: LaunchCap,
    /// The launcher's pod context: every event its calls cause carries it.
    at: PodContext,
    /// `VolumeDraining` consumed: [`Domains::unstage`] drains.
    draining: bool,
    /// `VolumeReleased` consumed.
    released: bool,
}

impl Domains {
    /// Volume staged as the launcher (it recovers the volume, and never
    /// creates one); `settings`' overlay limit set before any view opens;
    /// Trees and Clients restored on one store. Holds the state area's lock
    /// until dropped. `settings` is the one read of the operator settings,
    /// of this volume's state area: nothing ties the two.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Volume`]: the stage refused or failed (`Busy` while
    /// another holds the state area, `OtherFormat`, ...);
    /// [`ServiceError::Settings`]: an overlay limit refused, before the
    /// volume is staged;
    /// [`ServiceError::Forks`] or [`ServiceError::Clients`]: rows not read.
    pub fn stage(
        state: &Path,
        launcher: Launcher,
        settings: &OperatorSettings,
    ) -> Result<Self, ServiceError> {
        let (minter, cap, at, upkeep) = launcher.into_parts();
        let events = DomainEvents::default();
        let clients = ClientsSlot::new(None);
        let mut volume = VolumeService::new(
            TreeStateArea::at(state).with_upkeep(upkeep),
            clients.clone(),
            events.clone(),
        );
        let overlay_limit = settings.overlay_limit()?;
        volume.stage(&at, &cap)?;
        let recording = served(volume.area());
        recording.set_overlay_limit(overlay_limit);
        Self::restore(recording, (minter, cap, at), events, clients, Some(volume))
    }

    /// Trees and Clients restored over `recording`, as every start restores
    /// them, publishing to `events`; Clients fills `clients`, which `volume`
    /// asks. `cap` is the launcher every call is made as.
    pub(crate) fn restore(
        recording: Arc<Recording>,
        (minter, cap, at): (Minter, LaunchCap, PodContext),
        events: DomainEvents,
        clients: ClientsSlot,
        volume: Option<VolumeOf>,
    ) -> Result<Self, ServiceError> {
        let store = TreeStore::new(Arc::clone(&recording));
        let trees = Shared::new(
            TreesService::restore(store.clone(), events.clone()).map_err(ServiceError::Forks)?,
        );
        let sessions = Sessions::new(recording);
        let restored = ClientsService::restore(
            store,
            sessions.clone(),
            trees.clone(),
            events.clone(),
            minter,
        )
        .map_err(ServiceError::Clients)?;
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

    /// The sessions every bound client's view reaches its tree through.
    pub fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    /// Clients, restored.
    fn clients(&self) -> RefMut<'_, ClientsOf> {
        RefMut::map(self.clients.get_mut(), |clients| {
            clients
                .as_mut()
                .expect("Domains::restore fills Clients before it answers")
        })
    }

    /// The one dispatcher: each domain event, in order, to its consumers,
    /// after the call that caused it returns. `LastDetached` settles Trees
    /// (an unkeyed fork no client is attached to ends); `ForkDropped`
    /// releases its writer claim; `VolumeDraining` stops attaches and
    /// begins [`Domains::unstage`]'s drain, `VolumeStaged` allows them
    /// again; each end re-judges the release, and `VolumeReleased` is
    /// [`Domains::released`]. A consumer's failure is logged: the next boot
    /// re-judges from asked facts.
    fn dispatch(&mut self) {
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
    fn rejudge(&mut self) -> Result<(), String> {
        match self.volume.as_mut() {
            Some(volume) => volume.rejudge().map(drop).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    /// The Stop drain begins: Volume unstages (`Staged` → `Draining`). If
    /// that begins the drain: `views`, the caller's views' clients, are
    /// detached (their opens saved), this launcher's other clients ended,
    /// and other launchers' attached clients logged (`tree DrainBlocked`);
    /// a volume that released at once has none of these. [`Unstaged::Draining`] is
    /// answered and the caller closes every view it serves. A failure is
    /// logged (`tree UnstageFailed`) and the volume stays staged; the next
    /// stop asks again.
    pub fn unstage(&mut self, views: &[ClientId]) -> Unstaged {
        if let Some(volume) = self.volume.as_mut()
            && let Err(e) = volume.unstage(&self.cap)
        {
            eprintln!("tree UnstageFailed: {e}");
        }
        self.dispatch();
        if !std::mem::take(&mut self.draining) {
            return Unstaged::Unchanged;
        }
        self.detach_all(views);
        // Every client of this launcher ends, not only the views': the
        // release is judged from the Clients index alone.
        self.reconcile(&ClientSet::new());
        self.log_foreign_clients();
        Unstaged::Draining
    }

    /// The stop with no drain, which ends the domains: the volume stays
    /// staged and its clients attached, as a crash leaves them, and no
    /// drop of the domains runs (they are forgotten, so nothing waits on a
    /// session). Before it returns, the engine has ended the volume's own
    /// processes (`Recording::end_git_children`): once the caller's process
    /// exits, the volume's lease is free. The caller ends its process next.
    ///
    /// # Errors
    ///
    /// As `Recording::end_git_children`: the lease is then free only once
    /// those processes have exited. The domains are forgotten all the same.
    pub fn abandon(self) -> std::io::Result<()> {
        todo!("page exit-3-reap")
    }

    /// Whether Volume has released the volume (`VolumeReleased` consumed).
    pub fn released(&self) -> bool {
        self.released
    }

    /// Whether the tree actor has stopped after a recording failure.
    pub fn stopped(&self) -> bool {
        self.sessions.recording().actor().is_err()
    }

    /// Trees' settle, asking Clients who is attached.
    fn settle_trees(&mut self) -> Result<(), String> {
        let clients = self.clients();
        let settled = self.trees.get_mut().settle(&*clients);
        settled.map(drop).map_err(|e| e.to_string())
    }

    /// Clients' re-judgement: this launcher's clients not in `live` end, and
    /// dropped forks' claims are released. A failure raises the alarm; the
    /// next reconcile (the drain's, or the next boot's) asks again. Answers
    /// how many clients it ended.
    pub fn reconcile(&mut self, live: &ClientSet) -> u32 {
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
    fn log_foreign_clients(&self) {
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
    pub fn bind(
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
    pub fn settle(&mut self) {
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
    pub fn fork(
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
    pub fn end_keyed(&mut self, key: &ForkKey) -> Result<(), TreeError> {
        let ended = self.trees.get_mut().end_keyed(key, &self.cap, &self.at);
        self.dispatch();
        ended
    }

    /// The fork under `key` and its phase, whatever the phase; never made.
    pub fn keyed(&self, key: &ForkKey) -> Option<(TreeId, ForkPhase)> {
        let trees = self.trees.get_mut();
        trees
            .keyed(key)
            .map(|facts| (facts.fork.into(), facts.phase))
    }

    /// Detaches each client, its opens saved first; a failure is logged (the
    /// next boot's reconcile ends it).
    pub fn detach_all(&mut self, ids: &[ClientId]) {
        for client in ids {
            let detached = self.clients().detach(client, &self.cap, &self.at);
            if let Err(e) = detached {
                eprintln!("tree DetachFailed: client={client}: {e}");
            }
            self.dispatch();
        }
    }
}
