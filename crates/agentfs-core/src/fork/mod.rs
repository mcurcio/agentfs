//! Forked volumes (design §19.1, "Forked volumes", 2026-10-16).
//!
//! A fork is an ordinary volume started from another volume's latest
//! commit, its source. It records, pushes and recovers like any volume.
//! When it finishes (its claim is deleted, or `agentfs fork finish`), it
//! publishes one change, a commit whose parent is the base it started
//! from, to its fork remote. Whatever receives that change does all the
//! gating: agentfs never reviews, accepts or merges.
//!
//! - [`identity`]: what a fork is, fixed at create, and its ref names;
//! - [`create`]: a fork from a source's state area, read without the
//!   source's lock and with no credentials;
//! - [`finish`]: the finisher, the one owner of `finished` and
//!   `published`, and [`finish::status`], what DeleteVolume asks.
//!
//! Interfaces only for now: every operation returns its `NotBuilt` error
//! until forked volumes unit 1 lands.

pub mod create;
pub mod finish;
pub mod identity;

pub use identity::{ForkIdentity, PushTarget, change_ref, finish_ref};
