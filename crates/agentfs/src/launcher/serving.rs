//! What `mount` and `host` share once their flags are read (docs/trees.md,
//! "Volume lifecycle"): the volume staged through Volume, its domains
//! restored as one launcher, the view host started, each view's client
//! detached as its view ends, and the drain at a stop (every client of the
//! launcher ended, every view closed) until Volume releases. One channel
//! carries every trigger of the loop; a [`Driver`] adds the command's own.
//! It wires domains to adapters and decides nothing.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, mpsc};

use agentfs_contracts::caps::{LaunchCap, Minter};
use agentfs_contracts::clients::{Binding, ClientId, ClientSet};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::views::{MountOptions, MountPoint, ViewError, ViewId, Views};
use agentfs_contracts::volume::{Volume, VolumeError};
use agentfs_core::state_area::TreeStateArea;
use agentfs_fuse::tree_view::{HostEvent, ViewHost};
use agentfs_volume::VolumeService;
use anyhow::{Context as _, Result, bail};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use super::domains::{ClientsSlot, Domains};
use super::{busy, other_format};
use crate::domain_events::DomainEvents;

/// Every trigger of the loop, on its one channel: the view host's events, a
/// stop (SIGTERM or SIGINT, or the command's own), and the command's own
/// triggers.
pub(super) enum Trigger<T> {
    Host(HostEvent),
    Stop,
    Own(T),
}

/// A volume staged and its domains restored, before its view host starts.
pub(super) struct Staged<T> {
    pub(super) domains: Domains,
    pub(super) sender: mpsc::Sender<Trigger<T>>,
    triggers: mpsc::Receiver<Trigger<T>>,
}

/// Stages the volume as `cap` (it recovers it, and never creates one),
/// installs the stop on SIGTERM and SIGINT, and restores Trees and Clients
/// on one store. `Err` holds the exit status of a refusal: busy (75) or
/// another format (1).
///
/// # Errors
///
/// A failed stage, signal install or restore.
pub(super) fn stage<T: Send + 'static>(
    state: &Path,
    at: &PodContext,
    cap: LaunchCap,
    minter: Minter,
) -> Result<std::result::Result<Staged<T>, ExitCode>> {
    let events = DomainEvents::default();
    let clients = ClientsSlot::new(None);
    let mut volume = VolumeService::new(TreeStateArea::at(state), clients.clone(), events.clone());
    match volume.stage(at, &cap) {
        Ok(_) => {}
        Err(VolumeError::Busy) => return Ok(Err(busy())),
        Err(VolumeError::OtherFormat) => return Ok(Err(other_format(state))),
        Err(e) => bail!("opening tree state: {e}"),
    }
    let (sender, triggers) = mpsc::channel();
    stop_on_signals(sender.clone())?;
    let recording = volume
        .area()
        .recording()
        .context("opening tree state: the staged volume is not served")?;
    let domains = Domains::restore(
        recording,
        (minter, cap, at.clone()),
        events,
        clients,
        Some(volume),
    )?;
    Ok(Ok(Staged {
        domains,
        sender,
        triggers,
    }))
}

/// Sends a [`Trigger::Stop`] at each SIGTERM or SIGINT, from a thread of its
/// own: installed once the volume is staged, so a signal while it boots
/// waits in the channel for the loop.
fn stop_on_signals<T: Send + 'static>(sender: mpsc::Sender<Trigger<T>>) -> Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGINT]).context("installing signal handlers")?;
    std::thread::Builder::new()
        .name("tree-signals".into())
        .spawn(move || {
            for _ in signals.forever() {
                if sender.send(Trigger::Stop).is_err() {
                    break;
                }
            }
        })
        .context("starting the signal thread")?;
    Ok(())
}

/// What a command adds to the shared loop.
pub(super) trait Driver<T> {
    /// A stop has come (SIGTERM, SIGINT, or a [`Trigger::Stop`] of the
    /// command's): before the volume unstages.
    fn stopping(&mut self, serving: &mut Serving<T>);
    /// A view's mount is gone and its client detached.
    fn view_ended(&mut self, serving: &mut Serving<T>, view: ViewId);
    /// One of the command's own triggers.
    fn own(&mut self, serving: &mut Serving<T>, trigger: T);
}

/// A staged volume served: its domains, its view host, and each open view's
/// client.
pub(super) struct Serving<T> {
    pub(super) domains: Domains,
    host: ViewHost,
    /// Each open view's client: detached when the view ends.
    pub(super) views: BTreeMap<ViewId, ClientId>,
    triggers: mpsc::Receiver<Trigger<T>>,
}

impl<T: Send + 'static> Serving<T> {
    /// Starts the view host over the staged volume, its events sent on the
    /// loop's channel. Answers the channel's sender too.
    ///
    /// # Errors
    ///
    /// The view host's start.
    pub(super) fn start(
        staged: Staged<T>,
        state: &Path,
        metrics_output: Option<&Path>,
    ) -> Result<(Self, mpsc::Sender<Trigger<T>>)> {
        let Staged {
            domains,
            sender,
            triggers,
        } = staged;
        let notify = {
            let sender = sender.clone();
            Arc::new(move |event: HostEvent| {
                // The receiver goes only once the loop has ended.
                let _ = sender.send(Trigger::Host(event));
            })
        };
        let host = ViewHost::new(domains.sessions.clone(), state, metrics_output, notify)
            .context("serving tree views")?;
        Ok((
            Self {
                domains,
                host,
                views: BTreeMap::new(),
                triggers,
            },
            sender,
        ))
    }
}

impl<T> Serving<T> {
    /// Mounts `binding`'s view at `at`; its client is detached when it ends.
    ///
    /// # Errors
    ///
    /// The view host's refusal; the client stays attached.
    pub(super) fn open(
        &mut self,
        at: &Path,
        binding: Binding,
        options: MountOptions,
    ) -> std::result::Result<ViewId, ViewError> {
        let client = binding.client.clone();
        let view = self
            .host
            .open(MountPoint(at.to_path_buf()), binding, options)?;
        self.views.insert(view, client);
        Ok(view)
    }

    /// Asks the view host to unmount `view`: [`Driver::view_ended`] follows.
    pub(super) fn close(&mut self, view: ViewId) {
        self.host.close(view);
    }

    /// Closes every open view.
    pub(super) fn close_all(&mut self) {
        for view in self.views.keys() {
            self.host.close(*view);
        }
    }

    /// The Stop drain begins: the volume unstages; a stopped actor records
    /// nothing more, so its views are closed at once.
    pub(super) fn stop(&mut self) {
        self.domains.unstage();
        if self.domains.stopped() {
            self.close_all();
        }
    }

    /// The next trigger; `None` once every sender is gone.
    pub(super) fn next(&self) -> Option<Trigger<T>> {
        self.triggers.recv().ok()
    }

    /// The loop, until Volume releases (or, with the actor stopped, every
    /// view has ended). Each trigger is an event: no flag is polled and no
    /// timer runs. A stopped actor records nothing more, so no drain can
    /// finish: serving ends once its views have, and [`Self::finish`]
    /// reports the stop.
    pub(super) fn run(&mut self, driver: &mut impl Driver<T>) {
        loop {
            if self.domains.released || (self.views.is_empty() && self.domains.stopped()) {
                break;
            }
            if self.domains.draining {
                self.domains.draining = false;
                let ids: Vec<ClientId> = self.views.values().cloned().collect();
                self.domains.detach_all(&ids);
                // Every client of this launcher ends, not only the views':
                // the release is judged from the Clients index alone.
                self.domains.reconcile(&ClientSet::new());
                self.close_all();
                continue;
            }
            let Some(trigger) = self.next() else { break };
            match trigger {
                Trigger::Stop => {
                    driver.stopping(self);
                    self.stop();
                }
                Trigger::Host(HostEvent::ViewEnded(view)) => {
                    if let Some(client) = self.views.remove(&view) {
                        self.domains.detach_all(&[client]);
                    }
                    driver.view_ended(self, view);
                }
                Trigger::Host(HostEvent::Landed) => self.domains.settle(),
                Trigger::Own(own) => driver.own(self, own),
            }
        }
    }

    /// Once the loop has ended: the view host's workers stopped and joined.
    ///
    /// # Errors
    ///
    /// A worker's failure, or the actor's stop.
    pub(super) fn finish(self) -> Result<()> {
        self.host.wait().context("serving tree views")
    }
}
