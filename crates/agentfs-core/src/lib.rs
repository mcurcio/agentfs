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
pub mod credentials;
pub mod events;
pub mod history;
pub mod identity;
pub mod journal;
pub mod lock;
pub mod nested;
pub mod paths;
pub mod pending;
pub mod probe;
pub mod push;
pub mod record;
pub mod rules;
pub mod service;
pub mod telemetry;
pub mod tracked;
pub mod volume;
