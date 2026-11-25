# agentfs-core along domain lines: what remains

**Design of what remains after the crate split's built steps (0 and A to G): M4's two crates and M9's deletion of
the recording engine. The owner's answers are in [OWNER-GUIDANCE.md](../../../OWNER-GUIDANCE.md) ("Performance",
2026-11-24); no question is open.** Layer: module (crate boundaries). **[read]**: in code. **[measured]**: timed in a
4-CPU container, or `docs/development.md` "Build settings". What is built is described by the
[architecture](../../architecture.md) (crates, ranks, module map) and the library pages; what the repository's
checks of the crates do not read is in [development](../../development.md#build-settings).

The owner's words, all general guidance: "Just two? Can we break it down more along domain lines?"; "I'm also
interested in any easy-wins of decomposing agentfs-core"; the recording engine stays whole until M9: "ok"; "consumers
should link against something named "agentfs""; the front is the library only until M9: "B is fine"; the library
hands consumers a working volume: "Q1 A"; cargo-nextest locally and in CI: "Q2 A".

## 1. The end state (after M9)

```
 KUBERNETES   agentfs-csi                      spawns `agentfs init` / `agentfs host`
 LIBRARY      agentfs (front, rank `umbrella`: re-exports only; from M9 a `fuse` feature, the rank then using `filesystem`)
 FILESYSTEM   agentfs-cli (binary `agentfs`: composition root, the OpenTelemetry SDK)
                 └─ agentfs-fuse (tree views, stale mounts)
 LIBRARY      agentfs-service (one volume: the domains wired onto one store, event dispatch, drain;
                 uses the domains below and agentfs-engine's adapters: state area, store, sessions)
        ┌────────────┬──────────────┬──────────────┬───────────────────────┐
   agentfs-volume agentfs-clients agentfs-trees agentfs-replication (M4)     pure domains over ports
        └────────────┴──────┬───────┴──────────────┘
                    agentfs-contracts (values incl. RepoPath and entry metadata, capabilities, traits, events, ports)
                            ▲ implements the ports
   agentfs-engine (the git model's aggregate: actor, journal, history, recording, sessions, store adapters, object store)
        ├─ agentfs-referee (pure: land, sync, touches, their values) ─ agentfs-lines (line maps) ─ agentfs-align
        ├─ agentfs-rules (pure)
        ├─ agentfs-git (git_cli, libgit2 setup, pack settings)
        └─ agentfs-os (file locks, fsutil, confined, mountinfo, shared_fs, listening sockets, allocator) · agentfs-failpoints
   agentfs-transport (M4: SSH and HTTP clients, credentials) implements replication's Remote port
   tests only: agentfs-model, agentfs-conformance, agentfs-repo (checks over the whole repository),
               agentfs-test-metrics (an in-memory metric reader both engines' tests use)
```

| Crate | Why it is a crate |
|---|---|
| agentfs-engine | The aggregate: one actor, one lock, one unit of work. Its modules are one cycle [read]: `impl TreeActor` in 8 files, `tree.rs` importing journal, recording, history and referee, 497 `pub(in crate::tree_actor)` items. It moves whole, module paths kept, and is not split further until a measurement says a boundary pays |
| agentfs-referee | The landing rules are pure; as a crate with no git2 in what it links and `no_std` outside its tests, that is checked, and its tests run in seconds |
| agentfs-lines | `tree_reconcile.rs` depends only on std, serde and align; the referee's values build on it |
| agentfs-os, agentfs-git, agentfs-failpoints | The leaves both engines share; once out, M9 deletes agentfs-core whole |
| agentfs-service | The library is an integration point (principle 10); before F only the binary wired a volume (its launcher's `domains.rs`, which F moved to `crates/agentfs-service/src/domains.rs`) |
| agentfs (front) | Consumers' one name; internal crates keep names that say what they hold |
| agentfs-repo | The repository-wide checks, out of core's documentation test; a docs-only change stops compiling core |
| agentfs-test-metrics | Both engines' tests read their instruments; a dev edge between the engines is refused, so the reader is its own crate (rank `tests`). After M9 the engine is its one user |

**While both engines exist:** agentfs-core (the recording engine) and agentfs-engine are siblings on agentfs-os and
agentfs-git, neither depending on the other. agentfs-fuse and agentfs-cli depend on both; agentfs-conformance on the
engine only. agentfs-csi depends on the engine and on agentfs-fuse (the `kubernetes` rank uses `filesystem`), for the
stale-mount judge (`agentfs_fuse::mount_state`, `stale::is_disconnected_io`): the cost is that csi links the recording
engine through fuse until M9 deletes fuse's recording half. Giving that judge a home below fuse is not part of this
course. The front re-exports neither agentfs-core nor agentfs-fuse until M9.

**The `git` program's home.** agentfs-align, the lowest crate and the only one with no git2 that runs `git`, holds the
program's isolated environment (`isolated`, `GIT_PATH`), so the model, which depends on align alone, runs `git` as the
engines do. agentfs-git builds its repack and merge commands on it and re-exports `GIT_PATH` for the ranks that do
not use `line-identity` (the binary); that is why the `leaf` rank uses `line-identity`. The counted spawn
(`git_cli::counted`) stays in agentfs-git: line maps take the spawner from their caller rather than counting by
construction, a convention that the spawn check in agentfs-repo and clippy's ban of `agentfs_align::histogram`
hold.

## 2. What remains [read]

- **M4** adds agentfs-replication, a pure domain over the shared kernel's ports, and agentfs-transport (the SSH and
  HTTP clients and credentials), which implements replication's `Remote` port.
- **M9** deletes agentfs-core whole, and the FUSE layer's recording half with it; the front gains a `fuse` feature,
  and its rank `umbrella` then uses `filesystem`. That is why no rank uses `umbrella` (a rank using it under
  `filesystem` would then be a cycle) and why it is listed after `kubernetes`. The deletion also ends:
  - the tree path's two reads of the recording engine at launch: `crates/agentfs-cli/src/otel.rs` builds core's
    `telemetry::Telemetry` (with `OtelAlarmLog`) at every `otel::init`, a tree mount's included, so a tree process's
    meter also carries the recording engine's `agentfs.nested.gitdir_heuristic`; and `daemon.rs` names core's
    `service::Stop` in `Daemon::watch`, the recording mount's (a tree mount's hold uses `start` and `wait` only);
  - the binary-local `otel::Identity`, which replaces core's `identity::Context` there;
  - agentfs-repo's `NOT_REEXPORTED` exception for agentfs-core, and `ci/check-deps.sh`'s refusal of the recording
    engine in what the library links, which the ranks cannot say while both engines share the rank `engine`;
  - csi linking the recording engine through fuse, for the stale-mount judge (`agentfs_fuse::mount_state`,
    `stale::is_disconnected_io`). Giving that judge a home below fuse is not part of this course.
- **Later:** a journal or history crate only if a measurement shows the boundary pays.

## 3. Not delivered by the crate split

- The duplicated fork rules (a fork's upstream in the Trees row and in `Upstreams`; "no forks of forks" checked in
  both), and a kernel-independent tree view: `agentfs-fuse/src/tree_view.rs` uses `fuser::*` and implements
  `Filesystem`, so it is tested only where `/dev/fuse` exists. Each is its own work item.
- `agentfs-fuse/src/backing.rs` keeps its own confined access beside `agentfs_os::confined` (they share the
  resolution flags and `proc_path`); it is the recording engine's FUSE half, so M9's deletion ends the copy.
- The engine stays one crate of about 38k lines; a tree edit still runs every tree test: 64 s against the 77 s run
  of all of core [measured, development.md "Build settings"]. The owner's test-time goal rests on nextest and the
  smaller local volumes, not on crate boundaries.
- The tree library (engine and service alike) reports failures it cannot hand back as `tree <Name>` lines on stderr,
  not through OpenTelemetry; moving them to an alarm port is one change for both.
