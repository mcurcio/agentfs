//! What `mount` and `host` share once their flags are read (docs/trees.md,
//! "Volume lifecycle"): the volume staged and its domains restored as one
//! launcher through the service (`agentfs_service::Domains`), the view host
//! started, each view's client detached as its view ends, and the drain at
//! a stop (every client of the launcher ended, every view closed) until
//! Volume releases. One channel
//! carries every trigger of the loop; a [`Driver`] adds the command's own.
//! It wires domains to adapters and decides nothing.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, mpsc};

use agentfs_contracts::clients::{Binding, ClientId};
use agentfs_contracts::views::{MountOptions, MountPoint, ViewError, ViewId, Views};
use agentfs_contracts::volume::VolumeError;
use agentfs_engine::operator_settings::OperatorSettings;
use agentfs_fuse::stale::{MountState, mount_state};
use agentfs_fuse::tree_view::{HostEvent, ViewHost};
use agentfs_service::{Domains, Launcher, ServiceError, Unstaged};
use anyhow::{Context as _, Result, bail};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use super::{busy, other_format};

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

/// Stages the volume as `launcher` with the operator settings read at boot
/// ([`Domains::stage`]: it recovers it, and never creates one; the lag
/// gate's overlay limit is set before any view opens; Trees and Clients
/// are restored on one store), then installs the stop on SIGTERM and
/// SIGINT. `Err` holds the exit status of a refusal: busy (75) or another
/// format (1).
///
/// # Errors
///
/// A failed stage or restore, or signal install.
pub(super) fn stage<T: Send + 'static>(
    state: &Path,
    launcher: Launcher,
    settings: &OperatorSettings,
) -> Result<std::result::Result<Staged<T>, ExitCode>> {
    let domains = match Domains::stage(state, launcher, settings) {
        Ok(domains) => domains,
        Err(ServiceError::Volume(VolumeError::Busy)) => return Ok(Err(busy())),
        Err(ServiceError::Volume(VolumeError::OtherFormat)) => {
            return Ok(Err(other_format(state)));
        }
        Err(ServiceError::Volume(e)) => bail!("opening tree state: {e}"),
        Err(e) => return Err(e.into()),
    };
    let (sender, triggers) = mpsc::channel();
    stop_on_signals(sender.clone())?;
    Ok(Ok(Staged {
        domains,
        sender,
        triggers,
    }))
}

/// Sends a [`Trigger::Stop`] at each SIGTERM or SIGINT, from a thread of its
/// own: installed once the volume is staged and its domains restored, so a
/// signal while it boots waits in the channel for the loop.
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
    /// [`Self::leave`] was asked: the loop ends with the views left mounted.
    left: bool,
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
        let host = ViewHost::new(domains.sessions().clone(), state, metrics_output, notify)
            .context("serving tree views")?;
        Ok((
            Self {
                domains,
                host,
                views: BTreeMap::new(),
                triggers,
                left: false,
            },
            sender,
        ))
    }
}

impl<T> Serving<T> {
    /// Mounts `binding`'s view at `at`; its client is detached when it ends.
    /// Answers whether it was stacked over a stale mount (allowed by
    /// `options.over_stale`), which it logs.
    ///
    /// # Errors
    ///
    /// The view host's refusal; the client stays attached.
    pub(super) fn open(
        &mut self,
        at: &Path,
        binding: Binding,
        options: MountOptions,
    ) -> std::result::Result<(ViewId, bool), ViewError> {
        let client = binding.client.clone();
        let stacked = options.over_stale && matches!(mount_state(at), Ok(MountState::Stale { .. }));
        let view = self
            .host
            .open(MountPoint(at.to_path_buf()), binding, options)?;
        self.views.insert(view, client);
        if stacked {
            eprintln!(
                "stacked over a stale mount at {} left by a crashed daemon",
                at.display()
            );
        }
        Ok((view, stacked))
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

    /// The Stop drain begins: the volume unstages, its views' clients
    /// detached if that begins the drain ([`Domains::unstage`]); every view
    /// is closed then, or at once when the actor has stopped, as it records
    /// nothing more. Every unstage goes through here.
    pub(super) fn stop(&mut self) {
        let views: Vec<ClientId> = self.views.values().cloned().collect();
        let unstaged = self.domains.unstage(&views);
        if unstaged == Unstaged::Draining || self.domains.stopped() {
            self.close_all();
        }
    }

    /// Ends the loop at once, with no drain: every open view stays mounted
    /// and its client attached. The caller must then end the process
    /// without dropping `self` (see [`Self::abandon`]), so each mount goes
    /// stale as at a crash, and the next start rebinds its client and
    /// stacks over it (docs/csi.md, "Recovery").
    pub(super) fn leave(&mut self) {
        self.left = true;
    }

    /// Whether [`Self::leave`] ended the loop.
    pub(super) fn left(&self) -> bool {
        self.left
    }

    /// After a [`Self::leave`]: ends the volume's own processes and forgets
    /// the domains ([`Domains::abandon`]), forgets the view host without
    /// unmounting anything, and answers how many views were open. Every
    /// change a view took is in the journal already (docs/trees.md,
    /// "Recording and durability"); the process's exit closes each view's
    /// kernel channel and frees the volume's lease. An end that failed is
    /// logged (`tree AbandonIncomplete`): the lease is then free only once
    /// each git child has exited.
    pub(super) fn abandon(self) -> usize {
        let Serving {
            domains,
            host,
            views,
            triggers,
            ..
        } = self;
        if let Err(e) = domains.abandon() {
            eprintln!(
                "tree AbandonIncomplete: {e}: the volume's lease is free once each of its git \
                 children has exited"
            );
        }
        // Dropping the view host would unmount its views.
        std::mem::forget((host, triggers));
        views.len()
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
            if self.left
                || self.domains.released()
                || (self.views.is_empty() && self.domains.stopped())
            {
                break;
            }
            let Some(trigger) = self.next() else { break };
            match trigger {
                Trigger::Stop => {
                    driver.stopping(self);
                    if !self.left {
                        self.stop();
                    }
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
