//! The agentfs engine: records change boundaries durably, then turns them
//! into git history. See `docs/architecture.md` and
//! `docs/library/core.md`.
//!
//! The FUSE layer calls [`volume::Recorder::record`] at each change boundary
//! and lets the triggering call return once it succeeds. A
//! [`volume::Committer`] turns records into commits in the background, and a
//! [`push::RemotePusher`] per remote pushes them. [`service::Service`] runs
//! both as background loops. Alarm conditions are reported as
//! [`events::Event`]s, and [`telemetry::Telemetry`] turns them, with the
//! engine's own measurements and spans, into OpenTelemetry signals.

pub mod checkpoint;
pub mod checks;
pub mod claims;
pub mod command;
pub mod config;
pub mod credentials;
pub mod events;
pub mod fork;
pub mod fsync_class;
pub mod gitmodules;
pub mod history;
pub mod history_service;
pub mod identity;
pub mod journal;
pub mod known_hosts;
pub mod lfs;
pub mod local_socket;
pub mod lock;
pub mod nested;
pub mod non_empty;
pub mod operations;
pub mod operator_settings;
pub mod patch;
pub mod pending;
pub mod probe;
pub mod proxy;
pub mod push;
pub mod push_size;
pub mod reconcile;
pub mod record;
pub mod release;
pub mod remote_auth;
pub mod remote_url;
pub mod repack;
pub mod restore;
pub mod rules;
pub mod service;
pub mod session;
#[cfg(feature = "ssh")]
pub(crate) mod ssh_transport;
pub mod stamps;
pub mod state_area;
pub mod store;
pub mod telemetry;
pub mod tracked;
pub mod tree_actor;
pub mod tree_git;
pub mod tree_telemetry;
pub mod visibility;
pub mod volume;
pub(crate) mod watched;
