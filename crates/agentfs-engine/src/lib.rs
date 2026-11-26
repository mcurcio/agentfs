//! The tree engine of agentfs: a tree view's one actor over its journal,
//! history and rows ([`tree_actor`]), the git store it writes
//! ([`tree_git`]), sessions and the store adapters of the Clients and Trees
//! domains ([`session`], [`store`]), the volume's state area
//! ([`state_area`]), the operator's settings ([`operator_settings`]) and its
//! OpenTelemetry instruments ([`telemetry`]). It is the sibling of the
//! recording engine (`agentfs-core`): neither depends on the other, and the
//! ranks refuse an edge between them (docs/architecture.md). See
//! docs/library/engine.md.

pub mod operator_settings;
pub mod session;
pub mod state_area;
pub mod store;
pub mod telemetry;
pub mod tree_actor;
pub mod tree_git;
