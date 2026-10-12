//! One history service per volume, fed by a front per pod (design §14).
//!
//! In single-process mode (the default) every mount is its own writer of
//! the branch and holds the state-area lock. With a history service, one
//! process (`agentfs serve-history`) holds the lock and is the only writer
//! of the branch and journal; each pod's mount is a *front* that freezes
//! into the shared `pending/` in a slot it leases ([`FrontLease`]) and sends
//! the record to the service, which journals it and replies once it is
//! durable. The front fails the boundary with `EIO` if no durable reply
//! comes (design §3.5).

mod lease;

pub use lease::{FrontLease, LeaseError, MAX_SLOT};
