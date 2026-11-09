# Target architecture

This set describes the **target** architecture of agentfs: the shape the code should move to. It is not a description of today's code. For how agentfs works today, read [Design](../design.md) and [Architecture](../architecture.md).

**Start with [agentfs in git terms](git-model.md): it is the foundation for implementation.** It describes every tree, save, lock, landing, sync, collision and transaction as git branches, commits and refs, as the executable model in `crates/agentfs-model` runs them. It is the one source of truth for all of that; no other page here restates it.

The rest of the set covers only what the git model does not: [the system](system.md) (the owner's requirements and where each is designed, the domains around the git model, their contracts and events, versions, deployment and crate layout) and one page per domain.

## Status

| Document | Status |
|---|---|
| [agentfs in git terms](git-model.md) | proposed; the foundation for implementation; wins wherever another page disagrees |
| [The rules ledger](rules-ledger.md) | the authority on which rules the owner approved; CI fails while any is `proposed` |
| [The system](system.md) | revision 10, proposed (revision 3 approved by the owner on 2026-11-02) |
| The eight domain pages | proposed |

## Reading order

1. **[agentfs in git terms](git-model.md)**.
2. **[The rules ledger](rules-ledger.md)**: every rule in plain words, and whether the owner approved it.
3. **[The system](system.md)**: its §5 lists the owner's requirements the git model does not design yet.
4. The domains:

| Domain | One line |
|---|---|
| [Rules](domains/rules.md) | pure path policy: recorded or not, database groups, what each view profile shows |
| [Clients](domains/clients.md) | clients, writers, grants, sessions and attachment; every capability |
| [Views](domains/views.md) | each client's filesystem: open files until close, what a client sees and when, consistency, caches |
| [Trees](domains/trees.md) | where working trees live, confinement, space limits, the fork lifecycle, pinned snapshots |
| [Replication](domains/replication.md) | remotes, credentials, pushes (the one pusher), settlement, divergence |
| [Volume](domains/volume.md) | lifecycle, start point, release verdict, an upgrade's drain |
| [Health](domains/health.md) | liveness of the history service and view hosts, judged from outside them |
| [Telemetry](domains/telemetry.md) | metrics, traces and alarms that never block |

## How to read a page

- Requirements credited to the owner are quoted and dated; everything else is a design choice and says so.
- Sketches are Rust and show surfaces only. Nothing here was compiled or run, except the git model's rules, which run as tests in `crates/agentfs-model`. How a change to them is verified: [Verifying the design](verifying.md).
- The older Forks, Journal, Landing and History pages were removed on 2026-11-03: the git model replaces them. They are in git history.
