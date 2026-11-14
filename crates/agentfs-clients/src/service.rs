//! The Clients domain over its ports (slice S4): attach, rebind, detach,
//! reconcile and the per-tree index, rebuilt from the rows at boot; no attach
//! while the volume drains (slice S6) (docs/trees.md, "Clients").

use std::collections::{BTreeMap, BTreeSet};

use agentfs_contracts::audit::Principal;
use agentfs_contracts::caps::{ClientCap, LaunchCap, LaunchScope, Minter};
use agentfs_contracts::clients::{
    Access, AttachedLookup, AttachmentEndReason, Binding, ClientError, ClientEvent, ClientId,
    ClientSet, Clients, Grant, SessionId,
};
use agentfs_contracts::name::ClientLabel;
use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::ports::{
    ClientRow, ClientStore, Event, EventSink, NewClient, SessionAuthority, SessionBinder,
    StoreError,
};
use agentfs_contracts::tree::{ForkId, TreeId};
use agentfs_contracts::trees::{ForkLookup, ForkPhase};

/// One attached client, as this run knows it.
struct Attachment {
    row: ClientRow,
    /// Its session while bound.
    session: Option<SessionId>,
    /// Attached or rebound this run: a restored client is not, until rebound.
    bound: bool,
}

/// One volume's Clients domain. It mints `ClientCap`s with its own [`Minter`],
/// lent by the composition root, and binds each to its session.
pub struct ClientsService<S, B, F, E> {
    store: S,
    sessions: B,
    forks: F,
    events: E,
    minter: Minter,
    /// Each tree's attached clients: the index `attached` and `LastDetached`
    /// read. A tree with none has no entry.
    index: BTreeMap<TreeId, BTreeMap<ClientId, Attachment>>,
    /// Each fork's one writer, for life (C4).
    claims: BTreeMap<ForkId, ClientId>,
    /// Clients ended this run: a second `detach` of one answers `Ok`.
    ended: BTreeSet<ClientId>,
    /// From `VolumeDraining` until `VolumeStaged`: `attach` answers `Draining`.
    draining: bool,
}

impl<S, B, F, E> ClientsService<S, B, F, E>
where
    S: ClientStore,
    B: SessionBinder,
    F: ForkLookup,
    E: EventSink,
{
    /// Loads every row and rebuilds the per-tree index. Every restored client
    /// waits for its launcher's `rebind` or `reconcile`.
    ///
    /// # Errors
    ///
    /// The store's load.
    pub fn restore(
        store: S,
        sessions: B,
        forks: F,
        events: E,
        minter: Minter,
    ) -> Result<Self, StoreError> {
        let mut index: BTreeMap<TreeId, BTreeMap<ClientId, Attachment>> = BTreeMap::new();
        let mut claims = BTreeMap::new();
        for row in store.load()? {
            for client in row.attached {
                let row = client.value;
                index.entry(row.id.tree).or_default().insert(
                    row.id.clone(),
                    Attachment {
                        row,
                        session: None,
                        bound: false,
                    },
                );
            }
            if let (Some(fork), Some(claim)) = (row.tree.fork(), row.writer_claim) {
                claims.insert(fork, claim.value);
            }
        }
        Ok(Self {
            store,
            sessions,
            forks,
            events,
            minter,
            index,
            claims,
            ended: BTreeSet::new(),
            draining: false,
        })
    }

    /// The restored client a launcher's view resumes: of this launcher, tree,
    /// label and access, not yet bound this run; the latest attached when
    /// several are.
    pub fn resumable(
        &self,
        by: &LaunchCap,
        tree: TreeId,
        label: &ClientLabel,
        access: Access,
    ) -> Option<ClientId> {
        self.index
            .get(&tree)?
            .values()
            .filter(|a| {
                !a.bound
                    && a.row.id.launcher == *by.launcher()
                    && a.row.id.label == *label
                    && a.row.grant.access() == access
            })
            .map(|a| a.row.id.clone())
            .max_by_key(|id| id.n)
    }

    /// Consumes `ForkDropped`: the fork's writer claim is released (C4).
    ///
    /// # Errors
    ///
    /// The store's write.
    pub fn on_fork_dropped(&mut self, fork: ForkId) -> Result<(), StoreError> {
        self.store.release_claim(fork)?;
        self.claims.remove(&fork);
        Ok(())
    }

    /// Releases the claim of every fork Trees reports dropped: what a lost
    /// `ForkDropped` (a crash or a failed release after the drop) left.
    fn release_dropped_claims(&mut self) -> Result<(), StoreError> {
        let dropped: Vec<ForkId> = self
            .claims
            .keys()
            .copied()
            .filter(|fork| {
                self.forks
                    .facts(*fork)
                    .is_some_and(|facts| facts.phase == ForkPhase::Dropped)
            })
            .collect();
        for fork in dropped {
            self.on_fork_dropped(fork)?;
        }
        Ok(())
    }

    /// Consumes `VolumeDraining`: `attach` answers `Draining` from now.
    pub fn on_volume_draining(&mut self) {
        self.draining = true;
    }

    /// Consumes `VolumeStaged`: `attach` is accepted again.
    pub fn on_volume_staged(&mut self) {
        self.draining = false;
    }

    fn attachment(&self, client: &ClientId) -> Option<&Attachment> {
        self.index.get(&client.tree)?.get(client)
    }

    /// C2, then the tree's state: a `ForksItCreates` launcher reaches only
    /// the forks it created; a fork must be open.
    fn check_tree(&self, tree: TreeId, by: &LaunchCap) -> Result<(), ClientError> {
        let fork_launcher = by.may() == LaunchScope::ForksItCreates;
        let Some(fork) = tree.fork() else {
            return if fork_launcher {
                Err(ClientError::NotPermitted)
            } else {
                Ok(())
            };
        };
        let facts = self.forks.facts(fork).ok_or(ClientError::TreeUnknown)?;
        if fork_launcher && facts.created_by != *by.launcher() {
            return Err(ClientError::NotPermitted);
        }
        if facts.phase != ForkPhase::Open {
            return Err(ClientError::TreeUnknown);
        }
        Ok(())
    }

    /// What the client's session may do: write as it, for a read-write grant
    /// (its `ClientCap`, minted here, C1), or read its tree.
    fn authority(&self, row: &ClientRow) -> SessionAuthority {
        let tree = row.grant.tree();
        match row.grant.access() {
            Access::ReadWrite => SessionAuthority::Write(ClientCap::mint(
                &self.minter,
                row.id.clone(),
                tree,
                WriterAt {
                    writer: row.id.writer(),
                    at: row.at.clone(),
                },
            )),
            Access::ReadOnly => SessionAuthority::Read(tree),
        }
    }

    /// Binds a client's authority to `session` and answers its binding.
    fn bind(&mut self, client: &ClientId, session: SessionId) -> Option<Binding> {
        let authority = self.authority(&self.attachment(client)?.row);
        self.sessions.bind(session, authority);
        let attachment = self.index.get_mut(&client.tree)?.get_mut(client)?;
        attachment.session = Some(session);
        attachment.bound = true;
        Some(Binding {
            client: client.clone(),
            session,
            grant: attachment.row.grant.clone(),
        })
    }

    /// Ends `ended`: each session unbound first (its opens saved), then one
    /// store step with each end and each tree left empty; on success the
    /// index forgets them and the events follow, `LastDetached` once per
    /// tree (C6). A failed step leaves the index as it was.
    fn end(
        &mut self,
        ended: &[(ClientId, AttachmentEndReason)],
        by: &LaunchCap,
        at: &PodContext,
    ) -> Result<(), StoreError> {
        if ended.is_empty() {
            return Ok(());
        }
        for (client, _) in ended {
            let session = self
                .index
                .get_mut(&client.tree)
                .and_then(|clients| clients.get_mut(client))
                .and_then(|a| a.session.take());
            if let Some(session) = session {
                self.sessions.unbind(session);
            }
        }
        let gone: BTreeSet<&ClientId> = ended.iter().map(|(client, _)| client).collect();
        let emptied: Vec<TreeId> = self
            .index
            .iter()
            .filter(|(_, clients)| clients.keys().all(|c| gone.contains(c)))
            .filter(|(_, clients)| clients.keys().any(|c| gone.contains(c)))
            .map(|(tree, _)| *tree)
            .collect();
        self.store.end(
            ended,
            &emptied,
            &Principal::Launcher(by.launcher().clone()),
            at,
        )?;
        for (client, why) in ended {
            if let Some(clients) = self.index.get_mut(&client.tree) {
                clients.remove(client);
                if clients.is_empty() {
                    self.index.remove(&client.tree);
                }
            }
            self.ended.insert(client.clone());
            self.events
                .publish(Event::Clients(ClientEvent::AttachmentEnded {
                    client: client.clone(),
                    why: *why,
                }));
        }
        for tree in emptied {
            self.events
                .publish(Event::Clients(ClientEvent::LastDetached { tree }));
        }
        Ok(())
    }
}

impl<S, B, F, E> AttachedLookup for ClientsService<S, B, F, E>
where
    S: ClientStore,
    B: SessionBinder,
    F: ForkLookup,
    E: EventSink,
{
    fn attached_any(&self) -> ClientSet {
        self.index
            .values()
            .flat_map(|c| c.keys().cloned())
            .collect()
    }
}

impl<S, B, F, E> Clients for ClientsService<S, B, F, E>
where
    S: ClientStore,
    B: SessionBinder,
    F: ForkLookup,
    E: EventSink,
{
    fn attach(
        &mut self,
        grant: Grant,
        label: ClientLabel,
        at: PodContext,
        session: SessionId,
        by: &LaunchCap,
    ) -> Result<Binding, ClientError> {
        if self.draining {
            return Err(ClientError::Draining);
        }
        let tree = grant.tree();
        self.check_tree(tree, by)?;
        let claim = grant.access() == Access::ReadWrite && tree.fork().is_some();
        if claim && tree.fork().is_some_and(|f| self.claims.contains_key(&f)) {
            return Err(ClientError::ForkTaken);
        }
        let new = NewClient {
            launcher: by.launcher().clone(),
            label,
            grant,
            at,
        };
        let id = self
            .store
            .attach(&new, claim, &Principal::Launcher(new.launcher.clone()))?;
        if let (true, Some(fork)) = (claim, tree.fork()) {
            self.claims.insert(fork, id.clone());
        }
        self.index.entry(tree).or_default().insert(
            id.clone(),
            Attachment {
                row: ClientRow {
                    id: id.clone(),
                    grant: new.grant,
                    at: new.at,
                },
                session: None,
                bound: false,
            },
        );
        let binding = self.bind(&id, session).ok_or(ClientError::UnknownClient)?;
        self.events
            .publish(Event::Clients(ClientEvent::Attached(id)));
        Ok(binding)
    }

    fn rebind(
        &mut self,
        client: &ClientId,
        at: PodContext,
        session: SessionId,
        by: &LaunchCap,
    ) -> Result<Binding, ClientError> {
        let attachment = self.attachment(client).ok_or(ClientError::UnknownClient)?;
        if client.launcher != *by.launcher() {
            return Err(ClientError::NotItsLauncher);
        }
        if attachment.bound {
            return Err(ClientError::NotPermitted);
        }
        self.store
            .rebind(client, &at, &Principal::Launcher(by.launcher().clone()))?;
        if let Some(a) = self
            .index
            .get_mut(&client.tree)
            .and_then(|c| c.get_mut(client))
        {
            a.row.at = at;
        }
        self.bind(client, session).ok_or(ClientError::UnknownClient)
    }

    fn detach(
        &mut self,
        client: &ClientId,
        by: &LaunchCap,
        at: &PodContext,
    ) -> Result<(), ClientError> {
        if self.attachment(client).is_none() {
            return if self.ended.contains(client) {
                Ok(())
            } else {
                Err(ClientError::UnknownClient)
            };
        }
        if client.launcher != *by.launcher() {
            return Err(ClientError::NotItsLauncher);
        }
        self.end(&[(client.clone(), AttachmentEndReason::Detached)], by, at)?;
        Ok(())
    }

    fn reconcile(
        &mut self,
        live: &ClientSet,
        by: &LaunchCap,
        at: &PodContext,
    ) -> Result<Vec<ClientId>, ClientError> {
        let ended: Vec<(ClientId, AttachmentEndReason)> = self
            .attached_any()
            .into_iter()
            .filter(|c| c.launcher == *by.launcher() && !live.contains(c))
            .map(|c| (c, AttachmentEndReason::Reconciled))
            .collect();
        self.end(&ended, by, at)?;
        self.release_dropped_claims()?;
        Ok(ended.into_iter().map(|(c, _)| c).collect())
    }

    fn attached(&self, tree: TreeId) -> ClientSet {
        self.index
            .get(&tree)
            .map(|c| c.keys().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use agentfs_contracts::audit::{Audited, Stamp, UnixNanos};
    use agentfs_contracts::clients::{Consistency, FileOwner, Ordinal};
    use agentfs_contracts::name::LauncherId;
    use agentfs_contracts::ports::TreeClients;
    use agentfs_contracts::trees::{ForkFacts, ForkSettings, Landing};

    use super::*;

    /// Rows in memory, numbered as the actor numbers them: by step. Each
    /// end's `at` is kept; `fail_ends` fails every end, changing nothing.
    #[derive(Default)]
    struct Store {
        rows: BTreeMap<TreeId, TreeClients>,
        step: u64,
        ends_at: Vec<PodContext>,
        fail_ends: bool,
    }

    fn audited<T>(value: T) -> Audited<T> {
        let stamp = Stamp {
            at: UnixNanos(0),
            by: Principal::Agentfs,
        };
        Audited {
            value,
            created: stamp.clone(),
            changed: stamp,
        }
    }

    impl ClientStore for Store {
        fn load(&self) -> Result<Vec<TreeClients>, StoreError> {
            Ok(self.rows.values().cloned().collect())
        }

        fn attach(
            &mut self,
            client: &NewClient,
            claim: bool,
            _: &Principal,
        ) -> Result<ClientId, StoreError> {
            self.step += 1;
            let tree = client.grant.tree();
            let id = ClientId {
                launcher: client.launcher.clone(),
                tree,
                label: client.label.clone(),
                n: Ordinal(self.step),
            };
            let row = self.rows.entry(tree).or_insert_with(|| TreeClients {
                tree,
                attached: Vec::new(),
                writer_claim: None,
            });
            row.attached.push(audited(ClientRow {
                id: id.clone(),
                grant: client.grant.clone(),
                at: client.at.clone(),
            }));
            if claim {
                row.writer_claim = Some(audited(id.clone()));
            }
            Ok(id)
        }

        fn rebind(
            &mut self,
            _: &ClientId,
            _: &PodContext,
            _: &Principal,
        ) -> Result<(), StoreError> {
            self.step += 1;
            Ok(())
        }

        fn end(
            &mut self,
            ended: &[(ClientId, AttachmentEndReason)],
            _: &[TreeId],
            _: &Principal,
            at: &PodContext,
        ) -> Result<(), StoreError> {
            if self.fail_ends {
                return Err(StoreError::NotDurable(
                    agentfs_contracts::volume::IoKind::NoSpace,
                ));
            }
            self.step += 1;
            self.ends_at.push(at.clone());
            for (client, _) in ended {
                if let Some(row) = self.rows.get_mut(&client.tree) {
                    row.attached.retain(|a| a.value.id != *client);
                }
            }
            Ok(())
        }

        fn release_claim(&mut self, fork: ForkId) -> Result<(), StoreError> {
            if let Some(row) = self.rows.get_mut(&fork.into())
                && row.writer_claim.take().is_some()
            {
                self.step += 1;
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct Sessions(BTreeMap<SessionId, SessionAuthority>);

    impl SessionBinder for Sessions {
        fn bind(&mut self, session: SessionId, authority: SessionAuthority) {
            self.0.insert(session, authority);
        }
        fn unbind(&mut self, session: SessionId) {
            self.0.remove(&session);
        }
    }

    /// Fork 1 by `cli`, fork 2 by `other`, both open; fork 3 ended; fork 4
    /// dropped.
    struct Forks;

    impl ForkLookup for Forks {
        fn facts(&self, fork: ForkId) -> Option<ForkFacts> {
            let (by, phase) = match fork.0 {
                1 => ("cli", ForkPhase::Open),
                2 => ("other", ForkPhase::Open),
                3 => ("cli", ForkPhase::Ended),
                4 => ("cli", ForkPhase::Dropped),
                _ => return None,
            };
            Some(ForkFacts {
                fork,
                upstream: TreeId::Root,
                created_by: LauncherId::parse(by).unwrap(),
                key: None,
                settings: ForkSettings {
                    landing: Landing::Automatic,
                },
                phase,
            })
        }
    }

    #[derive(Default)]
    struct Events(Vec<Event>);

    impl EventSink for Events {
        fn publish(&mut self, event: Event) {
            self.0.push(event);
        }
    }

    type Service = ClientsService<Store, Sessions, Forks, Events>;

    #[allow(
        clippy::disallowed_methods,
        reason = "a test is its own composition root"
    )]
    fn minter() -> Minter {
        Minter::new()
    }

    fn cap(launcher: &str, may: LaunchScope) -> LaunchCap {
        LaunchCap::mint(&minter(), LauncherId::parse(launcher).unwrap(), may)
    }

    fn service() -> Service {
        ClientsService::restore(
            Store::default(),
            Sessions::default(),
            Forks,
            Events::default(),
            minter(),
        )
        .unwrap()
    }

    const OWNER: FileOwner = FileOwner { uid: 1, gid: 1 };

    fn write(tree: TreeId) -> Grant {
        Grant::live_read_write(tree, Consistency::PerFile, OWNER)
    }

    fn read(tree: TreeId) -> Grant {
        Grant::live_read_only(tree, Consistency::PerFile, OWNER)
    }

    fn label(text: &str) -> ClientLabel {
        ClientLabel::parse(text).unwrap()
    }

    fn attach(
        clients: &mut Service,
        grant: Grant,
        by: &LaunchCap,
    ) -> Result<ClientId, ClientError> {
        let session = SessionId(clients.sessions.0.len() as u64 + 100);
        clients
            .attach(grant, label("c"), PodContext::default(), session, by)
            .map(|b| b.client)
    }

    /// C2: a fork launcher reaches only the forks it created, never the root
    /// or another's; a refused attach writes no row and binds nothing.
    #[test]
    fn a_fork_launcher_reaches_only_its_forks() {
        let mut clients = service();
        let mine = cap("cli", LaunchScope::ForksItCreates);
        for (grant, refused) in [
            (write(TreeId::Root), ClientError::NotPermitted),
            (read(TreeId::Root), ClientError::NotPermitted),
            (write(TreeId::Fork(2)), ClientError::NotPermitted),
            (read(TreeId::Fork(2)), ClientError::NotPermitted),
            (write(TreeId::Fork(3)), ClientError::TreeUnknown),
            (write(TreeId::Fork(9)), ClientError::TreeUnknown),
        ] {
            assert_eq!(
                attach(&mut clients, grant.clone(), &mine),
                Err(refused),
                "{grant:?}"
            );
        }
        assert_eq!(clients.store.step, 0, "no row written");
        assert!(clients.sessions.0.is_empty());
        let id = attach(&mut clients, write(TreeId::Fork(1)), &mine).unwrap();
        assert_eq!(id.tree, TreeId::Fork(1));
        assert_eq!(
            clients.attached(TreeId::Fork(1)),
            ClientSet::from([id.clone()])
        );
        match clients.sessions.0.values().next().unwrap() {
            SessionAuthority::Write(cap) => {
                assert_eq!(cap.client(), &id);
                assert_eq!(cap.writes_as().writer.as_str(), id.to_string());
            }
            other => panic!("{other:?}"),
        }
        // The volume's own launcher reaches every open tree.
        let root = cap("cli", LaunchScope::Root);
        attach(&mut clients, write(TreeId::Root), &root).unwrap();
        attach(&mut clients, read(TreeId::Fork(2)), &root).unwrap();
        assert!(matches!(
            clients.sessions.0.values().last().unwrap(),
            SessionAuthority::Read(TreeId::Fork(2))
        ));
    }

    /// C4 and C6 on the domain alone: a fork's second writer is refused; a
    /// tree's last end publishes `LastDetached` once; a restored client is
    /// rebound once.
    #[test]
    fn claims_ends_and_rebinds() {
        let mut clients = service();
        let root = cap("cli", LaunchScope::Root);
        let writer = attach(&mut clients, write(TreeId::Fork(1)), &root).unwrap();
        let reader = attach(&mut clients, read(TreeId::Fork(1)), &root).unwrap();
        assert_eq!(
            attach(&mut clients, write(TreeId::Fork(1)), &root),
            Err(ClientError::ForkTaken)
        );
        assert_eq!(
            clients.detach(
                &writer,
                &cap("other", LaunchScope::Root),
                &PodContext::default()
            ),
            Err(ClientError::NotItsLauncher)
        );
        clients.events.0.clear();
        clients
            .detach(&writer, &root, &PodContext::default())
            .unwrap();
        clients
            .detach(&reader, &root, &PodContext::default())
            .unwrap();
        clients
            .detach(&reader, &root, &PodContext::default())
            .unwrap();
        assert_eq!(
            clients.events.0,
            [
                Event::Clients(ClientEvent::AttachmentEnded {
                    client: writer.clone(),
                    why: AttachmentEndReason::Detached
                }),
                Event::Clients(ClientEvent::AttachmentEnded {
                    client: reader.clone(),
                    why: AttachmentEndReason::Detached
                }),
                Event::Clients(ClientEvent::LastDetached {
                    tree: TreeId::Fork(1)
                }),
            ]
        );
        assert!(clients.sessions.0.is_empty(), "each session unbound");
        assert_eq!(
            attach(&mut clients, write(TreeId::Fork(1)), &root),
            Err(ClientError::ForkTaken),
            "the claim outlives its client"
        );
        // A restart: the root's client is restored, then rebound once.
        let kept = attach(&mut clients, write(TreeId::Root), &root).unwrap();
        let mut clients = ClientsService::restore(
            clients.store,
            Sessions::default(),
            Forks,
            Events::default(),
            minter(),
        )
        .unwrap();
        assert_eq!(
            clients.resumable(&root, TreeId::Root, &label("c"), Access::ReadWrite),
            Some(kept.clone())
        );
        assert_eq!(
            clients.resumable(&root, TreeId::Root, &label("c"), Access::ReadOnly),
            None
        );
        let pod = PodContext {
            pod: Some(agentfs_contracts::name::Label::parse("p-2").unwrap()),
            ..PodContext::default()
        };
        let binding = clients
            .rebind(&kept, pod.clone(), SessionId(7), &root)
            .unwrap();
        assert_eq!(binding.client, kept);
        match &clients.sessions.0[&SessionId(7)] {
            SessionAuthority::Write(cap) => assert_eq!(cap.writes_as().at, pod),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            clients.rebind(&kept, pod, SessionId(8), &root),
            Err(ClientError::NotPermitted)
        );
        assert_eq!(
            attach(&mut clients, write(TreeId::Fork(1)), &root),
            Err(ClientError::ForkTaken),
            "and its restart"
        );
        assert_eq!(
            clients.reconcile(
                &ClientSet::from([kept.clone()]),
                &root,
                &PodContext::default()
            ),
            Ok(vec![])
        );
        assert_eq!(clients.attached_any(), ClientSet::from([kept]));
    }

    /// C4 ends at the drop: a dropped fork's claim leaves the row and the
    /// index, and a restore finds none.
    #[test]
    fn a_dropped_forks_claim_is_released() {
        let mut clients = service();
        let root = cap("cli", LaunchScope::Root);
        let writer = attach(&mut clients, write(TreeId::Fork(1)), &root).unwrap();
        clients
            .detach(&writer, &root, &PodContext::default())
            .unwrap();
        assert_eq!(
            attach(&mut clients, write(TreeId::Fork(1)), &root),
            Err(ClientError::ForkTaken)
        );
        clients.on_fork_dropped(ForkId(1)).unwrap();
        assert!(clients.claims.is_empty());
        assert!(clients.store.rows[&TreeId::Fork(1)].writer_claim.is_none());
        let restored = ClientsService::restore(
            clients.store,
            Sessions::default(),
            Forks,
            Events::default(),
            minter(),
        )
        .unwrap();
        assert!(restored.claims.is_empty());
    }

    /// The boot's reconcile releases the claim of a fork Trees reports
    /// dropped: a crash after the drop step and before `ClaimReleased`
    /// leaves the claim on the row, and the next boot releases it; an open
    /// fork's claim stays.
    #[test]
    fn reconcile_releases_a_dropped_forks_claim() {
        let mut clients = service();
        let root = cap("cli", LaunchScope::Root);
        let writer = attach(&mut clients, write(TreeId::Fork(1)), &root).unwrap();
        clients
            .detach(&writer, &root, &PodContext::default())
            .unwrap();
        // Fork 4 dropped with its claim left on the row.
        let claim = ClientId {
            tree: TreeId::Fork(4),
            ..writer
        };
        clients.store.rows.insert(
            TreeId::Fork(4),
            TreeClients {
                tree: TreeId::Fork(4),
                attached: Vec::new(),
                writer_claim: Some(audited(claim)),
            },
        );
        let mut clients = ClientsService::restore(
            clients.store,
            Sessions::default(),
            Forks,
            Events::default(),
            minter(),
        )
        .unwrap();
        assert_eq!(clients.claims.len(), 2);
        assert_eq!(
            clients.reconcile(&ClientSet::new(), &root, &PodContext::default()),
            Ok(vec![])
        );
        assert_eq!(
            clients.claims.keys().copied().collect::<Vec<_>>(),
            [ForkId(1)]
        );
        assert!(clients.store.rows[&TreeId::Fork(4)].writer_claim.is_none());
        assert!(clients.store.rows[&TreeId::Fork(1)].writer_claim.is_some());
    }

    /// An end carries the launcher's pod context; a reconcile whose end
    /// cannot be recorded answers the store's failure (the caller raises the
    /// alarm) and ends none.
    #[test]
    fn ends_carry_the_launchers_pod_and_a_failed_reconcile_says_so() {
        let mut clients = service();
        let root = cap("cli", LaunchScope::Root);
        let pod = PodContext {
            pod: Some(agentfs_contracts::name::Label::parse("p-3").unwrap()),
            ..PodContext::default()
        };
        let one = attach(&mut clients, write(TreeId::Root), &root).unwrap();
        let two = attach(&mut clients, read(TreeId::Root), &root).unwrap();
        clients.detach(&one, &root, &pod).unwrap();
        assert_eq!(clients.store.ends_at, std::slice::from_ref(&pod));
        clients.store.fail_ends = true;
        assert_eq!(
            clients.reconcile(&ClientSet::new(), &root, &pod),
            Err(ClientError::NotDurable(
                agentfs_contracts::volume::IoKind::NoSpace
            ))
        );
        assert_eq!(clients.attached_any(), ClientSet::from([two.clone()]));
        clients.store.fail_ends = false;
        assert_eq!(
            clients.reconcile(&ClientSet::new(), &root, &pod),
            Ok(vec![two])
        );
        assert_eq!(clients.store.ends_at, [pod.clone(), pod]);
    }

    /// While the volume drains no client attaches, nothing written; detach
    /// still ends one; staged again, attach is accepted.
    #[test]
    fn no_attach_while_the_volume_drains() {
        let mut clients = service();
        let root = cap("cli", LaunchScope::Root);
        let kept = attach(&mut clients, write(TreeId::Root), &root).unwrap();
        clients.on_volume_draining();
        let step = clients.store.step;
        assert_eq!(
            attach(&mut clients, write(TreeId::Root), &root),
            Err(ClientError::Draining)
        );
        assert_eq!(clients.store.step, step, "no row written");
        clients
            .detach(&kept, &root, &PodContext::default())
            .unwrap();
        assert!(clients.attached_any().is_empty());
        clients.on_volume_staged();
        attach(&mut clients, write(TreeId::Root), &root).unwrap();
    }
}
