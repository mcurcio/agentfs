//! The agentfs engine: records change boundaries durably, then turns them
//! into git history. See `docs/design.md`.
//!
//! The FUSE layer calls [`volume::Recorder::record`] at each change boundary
//! and lets the triggering call return once it succeeds. A
//! [`volume::Committer`] turns records into commits in the background.

pub mod checks;
pub mod claims;
pub mod config;
pub mod events;
pub mod history;
pub mod identity;
pub mod journal;
pub mod nested;
pub mod paths;
pub mod pending;
pub mod probe;
pub mod record;
pub mod rules;
pub mod tracked;
pub mod volume;
