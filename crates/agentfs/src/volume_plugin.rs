//! The container runtime's entry to sandbox sessions: a Docker volume
//! plugin endpoint (docs.docker.com/engine/extend/plugins_volume; design
//! §2.4; provenance slice 2).
//!
//! `agentfs mount --sandbox-plugin-dir DIR` listens on `DIR/agentfs.sock`
//! (through [`agentfs_core::listening_socket::Listener`]); a Docker daemon
//! that has `DIR` as its plugin directory (`/run/docker/plugins`) finds the
//! driver `agentfs` there. The directory is mounted into the Docker
//! daemon's container only, never the workload's. Connections are admitted
//! like the control socket's: the peer (`SO_PEERCRED`) must be root or the
//! daemon's own user.
//!
//! **Protocol.** HTTP/1.1 `POST /<Endpoint>` with a JSON body, answered
//! with JSON (`Content-Type: application/vnd.docker.plugins.v1+json`):
//!
//! - `Plugin.Activate` → `{"Implements":["VolumeDriver"]}`;
//! - `VolumeDriver.Capabilities` → `{"Capabilities":{"Scope":"local"}}`;
//! - `VolumeDriver.Create {Name, Opts}`: the one served volume name with no
//!   options → `{"Err":""}`; anything else is refused;
//! - `VolumeDriver.Remove`: always refused (agentfs data is never removed
//!   through Docker);
//! - `VolumeDriver.Get {Name}`, `VolumeDriver.List`: the one volume, with
//!   no `Mountpoint` (each mount has its own);
//! - `VolumeDriver.Path {Name}` → `{"Err":""}` with no `Mountpoint`;
//! - `VolumeDriver.Mount {Name, ID}` → opens session `sandbox/<ID>` and
//!   answers `{"Mountpoint":"<root>/<ID>","Err":""}`; idempotent per ID;
//! - `VolumeDriver.Unmount {Name, ID}` → closes it; an ID with no session
//!   succeeds.
//!
//! Every refusal is `{"Err":"<why>"}` with status 200 (Docker reads `Err`);
//! an unknown endpoint is status 404 with an `Err`. A name other than the
//! served one, an `ID` that is not a [`SandboxId`], and any `Opts` are
//! refused. Docker's plugin API has no authentication: whoever drives the
//! Docker daemon can open and close sessions (design §2.4, its costs).

#![expect(dead_code, reason = "stub: provenance slice 2 wires it in")]

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use agentfs_core::listening_socket::Listener;
use agentfs_core::record::SandboxId;

/// The plugin socket's file name in `--sandbox-plugin-dir`, which is also
/// the driver name Docker knows it by.
pub(crate) const SOCKET_NAME: &str = "agentfs.sock";

/// The media type of every plugin reply.
pub(crate) const CONTENT_TYPE: &str = "application/vnd.docker.plugins.v1+json";

/// What the plugin needs of the sandbox sessions: the seam that lets the
/// protocol be tested without FUSE. Implemented for
/// [`agentfs_fuse::sessions::Sandboxes`].
pub(crate) trait SandboxSessions: Send + Sync {
    /// [`agentfs_fuse::sessions::Sandboxes::open`]: the mount point, or why
    /// not, as the `Err` Docker shows.
    fn open(&self, id: &SandboxId) -> Result<PathBuf, String>;
    /// [`agentfs_fuse::sessions::Sandboxes::close`]: `Ok` also when no
    /// session `id` is open.
    fn close(&self, id: &SandboxId) -> Result<(), String>;
}

/// The protocol for one served volume name over the sessions.
pub(crate) struct VolumePlugin {
    volume: String,
    sessions: Arc<dyn SandboxSessions>,
}

/// One HTTP reply: a status and a JSON body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

impl VolumePlugin {
    /// Serves the Docker volume `volume` (`--sandbox-volume`, default the
    /// agentfs volume's name) over `sessions`.
    pub(crate) fn new(volume: String, sessions: Arc<dyn SandboxSessions>) -> Self {
        Self { volume, sessions }
    }

    /// The reply to one call: `endpoint` is the request path
    /// (`/VolumeDriver.Mount`), `body` its JSON. Pure but for the calls to
    /// [`SandboxSessions`]; never panics on any input.
    pub(crate) fn answer(&self, endpoint: &str, body: &[u8]) -> Reply {
        let _ = (endpoint, body, &self.volume, &self.sessions);
        todo!("provenance slice 2")
    }
}

/// A running plugin endpoint.
pub(crate) struct PluginServer {
    _private: (),
}

/// Serves `plugin` on `listener` (bound at `DIR/agentfs.sock`) from its
/// own thread, one request per HTTP/1.1 exchange (hyper's `http1` server
/// over the listener's async form), until stopped.
pub(crate) fn serve(plugin: Arc<VolumePlugin>, listener: Listener) -> io::Result<PluginServer> {
    let _ = (plugin, listener);
    todo!("provenance slice 2")
}

impl PluginServer {
    /// Refuses every later connection (the listener is shut down) and
    /// waits for the calls in progress to be answered. Called before the
    /// mount stops, so no session opens while the sessions close.
    pub(crate) fn stop(self) {
        todo!("provenance slice 2")
    }
}
