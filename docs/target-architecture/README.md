# Target architecture

This set describes the **target** architecture of agentfs: the shape the code should move to, domain by domain. It is not a description of today's code. For how agentfs works today, read [Design](../design.md) and [Architecture](../architecture.md).

The set is one system document and twelve domain documents. The system document names every domain, what each owns exclusively, and every contract, event and flow between them. Each domain document designs one domain's inside: its model, contract, invariants, ports, flows, tests and costs. A domain document may add internals, never a crossing the system document lacks.

## Status

| Document | Status |
|---|---|
| [The system](system.md) | approved by the owner on 2026-11-02 (revision 3); revision 4 resolves the gaps the domain documents found, each change listed in its change log |
| The twelve domain documents | proposed |

## Reading order

1. **[The system](system.md)** first. It is binding on every domain page.
2. Then the domains, in the order the system's §10 lists them:

| Domain | One line |
|---|---|
| [Rules](domains/rules.md) | pure path policy: recorded or not, class and group, what each view profile shows |
| [Clients](domains/clients.md) | clients, writers, grants, sessions and attachment; every capability |
| [Views](domains/views.md) | each client's filesystem view: serving, caches, following feeds, consistency, locks |
| [Trees](domains/trees.md) | the real filesystem: source tree, fork layers, staging, copy-up, rename into place |
| [Forks](domains/forks.md) | lineage, path bases, pinned forks, fork space limits, ending and dropping |
| [Journal](domains/journal.md) | one durable stream per tree, transactions, holds, the change feed, backpressure |
| [Landing](domains/landing.md) | the one landing process: sources, units, timing and acceptance, gate, merge, verdicts |
| [History](domains/history.md) | lines, the history setting, one-writer commits, objects, LFS, the one log's read side |
| [Replication](domains/replication.md) | remotes, credentials, pushing, fetching, settlement, divergence |
| [Volume](domains/volume.md) | lifecycle, start point, release verdict |
| [Health](domains/health.md) | liveness of the history service and view hosts, judged from outside them |
| [Telemetry](domains/telemetry.md) | metrics, traces and alarms that never block |

## How to read a page

- Requirements credited to the owner are quoted and dated; everything else is a design choice and says so.
- Questions for the owner are marked Q (system) or with a domain letter (QL, QJ, QH, ...), each with a recommendation and its cost.
- Each domain page ends with its escalations to the system document and how revision 4 resolved them.
- Sketches are Rust and show surfaces only. Nothing here was compiled or run.
