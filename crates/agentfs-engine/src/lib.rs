//! The tree engine: a volume of git-backed tree views whose saves one actor
//! orders (docs/trees.md). Each call and step is in the actor's journal
//! before it returns, and history is built from the journal; the store
//! adapters here implement the domains' ports (`agentfs-contracts`) over
//! that actor. It is the recording engine's sibling (`agentfs-core`):
//! neither depends on the other. docs/architecture.md maps its modules.
