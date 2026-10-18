//! Forked volumes (design §19.1, "Forked volumes", 2026-10-16).
//!
//! A fork is an ordinary volume started from another volume's latest
//! commit, its source. It records, pushes and recovers like any volume.
//! When it finishes (its claim is deleted, or `agentfs fork finish`), it
//! publishes one change, a commit whose parent is the base it started
//! from, to its fork remote. Whatever receives that change does all the
//! gating: agentfs never reviews, accepts or merges. That is a linked
//! fork, the default; an unlinked one ([`ForkLink::Unlinked`], fixed at
//! create) is independent: it never finishes and delivers nothing.
//!
//! - [`identity`]: what a fork is, fixed at create, and its ref names;
//! - [`create`]: a fork from a source's state area, read without the
//!   source's lock and with no credentials;
//! - [`finish`]: the finisher, the one owner of `finished`, `published`
//!   and `refused`; [`finish::abandon`], the operator's discard; and
//!   [`finish::status`], what DeleteVolume asks;
//! - [`listing`]: the published changes on a fork remote, checked, read
//!   in a receiver's repository with its own git;
//! - [`deliver`]: the library's delivery of a published change (inbox,
//!   branch, command, webhook), for a receiver to program; never run by
//!   the CSI driver.
//!
//! Design §11.5 has the whole model.

pub mod create;
pub mod deliver;
pub mod finish;
pub mod identity;
pub mod listing;
pub mod receive;
pub mod start;

#[cfg(test)]
mod tests;

pub use identity::{ForkIdentity, ForkLink, PushTarget, change_ref, finish_ref};
