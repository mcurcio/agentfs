# Target architecture

This directory holds two kinds of page.

- **The baseline.** The git model, the rules ledger, the executable model and the demo. They are the proof and the
  testbed for how trees, saves, landings and syncs behave. They stay.
- **Open slices.** Designs for work that is not built. A design page lives only until its slice is built. The
  commit that lands the slice deletes the page and moves the behaviour into the library or operator docs. Each page
  opens with what it proposes.

How the built parts work is in [trees](../trees.md) and the library pages, not here.

## The baseline

| Page | What it holds |
|---|---|
| [agentfs in git terms](git-model.md) | every tree, save, lock, landing, sync, collision and transaction as git branches, commits and refs. It is the one source of truth for that, and no other page restates it |
| [The rules ledger](rules-ledger.md) | every rule in plain words, and whether the owner approved it. CI fails while any rule is `proposed` |
| [MODEL.md](../../crates/agentfs-model/MODEL.md) | the rules, invariants and findings the executable model checks |
| [VERIFYING.md](../../crates/agentfs-model/VERIFYING.md) | how a design change is verified on the model: what to run, seeds and replays, planted bugs, reading a failure, and the conformance replay on the real library and mounts |
| [Walkthrough](demo/walkthrough.md) | the fork and upstream rules run on plain `git`, with `demo.sh` as the script CI runs |

## The open slices

Pages for a whole domain or the system:

| Slice | Page |
|---|---|
| System composition and contracts: events, capabilities, versions, deployment compositions | [The system](system.md) |
| Rules, the unbuilt part: view profiles, database groups, size and store hints, rule-set versions across view hosts (what a tree records is built: [trees](../trees.md#rules-and-the-untracked-area)) | [Rules](domains/rules.md) |
| Clients: grants with view profiles, snapshot clients, operator actors | [Clients](domains/clients.md) |
| Views: snapshot views, consistency settings, several view hosts | [Views](domains/views.md) |
| Trees storage: confinement, space, pinned snapshots | [Trees](domains/trees.md) |
| Replication on the tree model: remotes, credentials, pushes, divergence | [Replication](domains/replication.md) |
| Volume: the start point, the upgrade drain, settlement by the remotes, what happens on release | [Volume](domains/volume.md) |
| Health: liveness of the history service and view hosts | [Health](domains/health.md) |
| The target telemetry domain: metrics, traces and alarms that never block | [Telemetry](domains/telemetry.md) |
| Snapshot opens (parked) | [Snapshot opens](snapshot-opens.md) |

Smaller slices, one page each:

| Slice | Page |
|---|---|
| Moving every launcher onto the tree views and deleting the recording engine: milestones M4 to M9 (M1 to M3 are built), the baseline (the first Hermes deploy, on a CSI volume claim), the deferred extras | [One engine](future/one-engine.md) |
| Cutting the tree actor into six modules | [Module cut](future/module-cut.md) |
| The name the root is published under, apart from its identity | [Root branch name](future/root-branch-name.md) |
| Transactions | [Transactions](future/transactions.md) |
| Nested forks on trees | [Nested forks](future/nested-forks.md) |
| Telemetry export from the tree views | [Tree-view telemetry](future/tree-view-telemetry.md) |
| Landing approvals and timing on trees | [Landing approvals](future/landing-approvals.md) |
| Integrator patches, restore and revert on trees | [Integrator patches](future/integrator-patches.md) |
| Files changed while open, database groups | [Open files and groups](future/files-open-groups.md) |
| Large and binary files on trees | [Large and binary files](future/large-and-binary-files.md) |
| Space limits and history truncation | [Space limits](future/space-limits.md) |
| Where the line map lives; two renames to one path | [Line map and renames](future/line-map-and-renames.md) |
| Refusing shared mappings | [Shared mappings](future/shared-mappings.md) |
| Faster saves and landings with large files or many saves | [Save and take speed](future/save-and-take-speed.md) |
| Landing patches while a workload runs | [Live landing](future/live-landing.md) |
| Recovery tooling beyond restore and revert | [Recovery tooling](future/recovery-tooling.md) |
| Sandbox overlay: shared locks, inode gaps, hosting on a front | [Overlay gaps](future/sandbox-overlay-gaps.md) |
| Wider power-loss coverage | [Power-loss coverage](future/power-loss-coverage.md) |
| Reloading OTLP TLS files | [OTLP TLS reload](future/otlp-tls-reload.md) |
| Cloning a claim that is in use | [Clone in use](future/clone-in-use.md) |
| Forge CI on hidden fork refs | [Forge CI](future/forge-ci-on-hidden-refs.md) |
| Handing a fork's change to a receiver; forks that never land | [Fork delivery](future/fork-delivery.md) |
| The integrator's checkpoint and the patch queue's notices | [Checkpoints and patch notices](future/integrator-hooks.md) |
| Recording, committing and pushing with no mount | [Recording without a mount](future/recording-without-a-mount.md) |
| A state area on NFS | [State on NFS](future/state-on-nfs.md) |

## How to read a page

- Sketches are Rust and show surfaces only. Nothing on an open-slice page is compiled or run. The git model's
  rules run as tests in `crates/agentfs-model`.
- The owner's guidance that shaped these pages is in `OWNER-GUIDANCE.md` at the repository root. Everything else on
  a page is a design choice and says so.
- Where an open-slice page seems to disagree with [git-model.md](git-model.md), the git model wins and the page has a bug.
