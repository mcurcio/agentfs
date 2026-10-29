//! The agentfs engine: records change boundaries durably, then turns them
//! into git history. See `docs/design.md`.
//!
//! The FUSE layer calls [`volume::Recorder::record`] at each change boundary
//! and lets the triggering call return once it succeeds. A
//! [`volume::Committer`] turns records into commits in the background, and a
//! [`push::RemotePusher`] per remote pushes them. [`service::Service`] runs
//! both as background loops. Alarm conditions are reported as
//! [`events::Event`]s, and [`telemetry::Telemetry`] turns them, with the
//! engine's own measurements and spans, into OpenTelemetry signals.

pub mod checks;
pub mod claims;
pub mod config;
pub mod confined;
pub mod credentials;
pub mod events;
pub mod failpoints;
pub mod file_lock;
pub mod fork;
pub mod fsutil;
pub mod fsync_class;
pub mod git_cli;
pub mod gitmodules;
pub mod history;
pub mod history_service;
pub mod identity;
pub mod journal;
pub mod known_hosts;
pub mod lfs;
pub mod local_socket;
pub mod lock;
pub mod mountinfo;
pub mod nested;
pub mod non_empty;
pub mod operations;
pub mod patch;
pub mod paths;
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
pub mod shared_fs;
#[cfg(feature = "ssh")]
pub(crate) mod ssh_transport;
pub mod stamps;
pub mod telemetry;
pub mod tracked;
pub mod volume;
pub(crate) mod watched;
