# agentfs-core along domain lines: the crates, and the course to them

**Design; rows A and B of section 3 are built, the rest is not. The owner's answers are in [OWNER-GUIDANCE.md](../../../OWNER-GUIDANCE.md) ("Performance",
2026-11-24); no question is open.** Layer: module (crate boundaries). Counts read at main 7efd868. **[read]**: in
code. **[measured]**: timed in a 4-CPU container, or `docs/development.md` "Build settings". **[est]**: to be measured
by the step that claims it.

The owner's words, all general guidance: "Just two? Can we break it down more along domain lines?"; "I'm also
interested in any easy-wins of decomposing agentfs-core"; the recording engine stays whole until M9: "ok"; "consumers
should link against something named "agentfs""; the front is the library only until M9: "B is fine"; the library
hands consumers a working volume: "Q1 A"; cargo-nextest locally and in CI: "Q2 A".

## 1. The end state (after M9)

```
 KUBERNETES   agentfs-csi                      spawns `agentfs init` / `agentfs host`
 FILESYSTEM   agentfs-cli (binary `agentfs`: composition root, the OpenTelemetry SDK)
                 └─ agentfs-fuse (tree views, stale mounts)
 LIBRARY      agentfs (front: re-exports only; a `fuse` feature from M9)
                 └─ agentfs-service (one volume: the domains wired onto one store, event dispatch, drain)
        ┌────────────┬──────────────┬──────────────┬───────────────────────┐
   agentfs-volume agentfs-clients agentfs-trees agentfs-replication (M4)     pure domains over ports
        └────────────┴──────┬───────┴──────────────┘
                    agentfs-contracts (values incl. RepoPath, capabilities, traits, events, ports)
                            ▲ implements the ports
   agentfs-engine (the git model's aggregate: actor, journal, history, recording, sessions, store adapters, object store)
        ├─ agentfs-referee (pure: land, sync, touches, their values) ─ agentfs-lines (line maps) ─ agentfs-align
        ├─ agentfs-rules (pure)
        ├─ agentfs-git (git_cli, libgit2 setup, pack settings)
        └─ agentfs-os (file locks, fsutil, confined, mountinfo, shared_fs, listening sockets, allocator) · agentfs-failpoints
   agentfs-transport (M4: SSH and HTTP clients, credentials) implements replication's Remote port
   tests only: agentfs-model, agentfs-conformance, agentfs-repo (checks over the whole repository)
```

| Crate | Why it is a crate |
|---|---|
| agentfs-engine | The aggregate: one actor, one lock, one unit of work. Its modules are one cycle [read]: `impl TreeActor` in 8 files, `tree.rs` importing journal, recording, history and referee, 497 `pub(in crate::tree_actor)` items. It moves whole, module paths kept, and is not split further until a measurement says a boundary pays |
| agentfs-referee | The landing rules are pure; as a crate with no git2 dependency, and `std::fs` banned in its `clippy.toml`, that is checked, and its tests run in seconds |
| agentfs-lines | `tree_reconcile.rs` depends only on std, serde and align; the referee's values build on it |
| agentfs-os, agentfs-git, agentfs-failpoints | The leaves both engines share; once out, M9 deletes agentfs-core whole |
| agentfs-service | The library is an integration point (principle 10); today only the binary wires a volume (`agentfs/src/launcher/domains.rs`) |
| agentfs (front) | Consumers' one name; internal crates keep names that say what they hold |
| agentfs-repo | The repository-wide checks, out of core's documentation test; a docs-only change stops compiling core |

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

## 2. What the code does that the end state does not [read]

After A and B the tree engine names no recording-engine module (agentfs-repo's
`the_tree_engine_names_no_recording_engine_module`), and the shared leaves are their own crates. What remains:

- `telemetry.rs` reaches into the tree engine (`:672`, `:684`, `:801`; tests `:1886`, `:1947`). C's.
- agentfs-csi calls `tree_actor::inspect` (`controller.rs:79`, and in `supervisor.rs`) instead of the
  `StateArea::inspect` port. C's.
- The referee's purity is a convention: `referee/entries.rs` imports the actor's `Error` and `queue`. G's.
- The tree view has no kernel-independent layer (`agentfs-fuse/src/tree_view.rs:43` uses `fuser::*`; `:1592`
  implements `Filesystem`), so it is tested only where `/dev/fuse` exists.

## 3. The course

| PR | Does | Needs | Gate | Gain |
|---|---|---|---|---|
| 0 | Tests run with cargo-nextest locally and in CI; the two slow programs (`tree_batch_alloc`, 71 s; `tree_stages`, about 60 s [measured]) cut to a tenth of their volumes locally, keeping their ratios, and run at full size on GitHub Actions; the detection gaps this leaves, local only, are stated in each test | none | ci, image, phase-1 | `agentfs-core`'s test run, built first: 196 s under `cargo test`, 77 s under `cargo nextest run` with its doctests (128 s from nextest alone) [measured, 2026-11-24, development.md "Build settings"]; `agentfs`'s 15 s unchanged [measured] |
| A (built) | agentfs-repo: the repository-wide checks out of core's documentation test (`doc_tables` a module, not a `#[path]` include); the crate list derived from the workspace; checks that dependencies point only down the layers, that no bare `-p agentfs` remains until E (after E a package named `agentfs` exists, and the check cannot tell a stale reference from a right one), that every crate using failpoints forwards the feature | none | ci, docs | the repository's checks build in 6.9 s from an empty target directory, not core's 88 s [measured, development.md]; local runs only: CI's docs job still builds core and both binaries for the reference tables |
| B (built) | agentfs-os, -git, -failpoints, -lines; `RepoPath`, `parse_size`, `parse_duration` to contracts; the tree engine's back-edges into the recording engine cut (`FifoGate` generic, its `CompactGate` impl into the engine; `sha256_at` into `lfs`); csi calls `is_safe_name` | none | ci, image | none alone; makes C, and M9's whole deletion, possible |
| C | agentfs-engine: `tree_actor/`, `tree_git`, `session`, `store`, `state_area`, `operator_settings`, their tests (`it/tree_*`, `tree_*.rs`, the tree half of `crash_points.rs`); its instruments registered on a `Meter` the binary makes; csi through the `StateArea` port; `docs/library/core.md`'s tree half becomes `engine.md`. The crate boundary replaces agentfs-repo's recording-module check; the engine keeps git2, so the ban on opening a remote stays clippy's (`clippy.toml`, the SSH transport's registration rests on it) | B | ci, image, phase-1 | a recording-engine edit runs only the recording engine's tests; a tree edit runs the tree tests, against the 77 s nextest run of all of core [est] |
| D | The CLI package renamed `agentfs-cli` (directory too), its binary and `#[command(name)]` still `agentfs`; every `-p agentfs` rewritten | C | ci, image, docker, hermes | none; with no package named `agentfs`, a missed reference fails |
| E | The front `agentfs`: re-exports of the engine, contracts and the domains; a doctest opens a tree volume through it; AGENTS.md principle 10's mapping updated | D | ci, docs | the owner's one name |
| F | agentfs-service: `launcher/domains.rs`, `shared.rs`, `domain_events.rs` and the drain, used by the binary; the front re-exports it | E | ci, image | consumers compose a volume in one call |
| G | agentfs-referee: its own error enum (`From` into the engine's); the line values (`tree.rs:27-205`) to lines; `MergeFileIdentity`, `NameHistory`, `Meta`, `queue::Key` public; `queue::held` (`queue.rs:50`, takes `&Shape`) up to its caller; ref naming for `Upstreams::merges` (`upstreams.rs:69`) resolved | C | ci, phase-1 | purity checked; rule tests in seconds |

The waiting-marks PR lands in the engine after C, beside G (disjoint files); whether its `Waiters` gets a crate is its
architect's call. **Later:** a journal or history crate only if a measurement shows the boundary pays; M4 adds
agentfs-replication and agentfs-transport; M9 deletes agentfs-core whole and fuse's recording half, and the front
gains `fuse`.

**Traps, each closed by a check, not a habit, but the first after E.** A stale `-p agentfs` building the front: D
before E, then A's check, which after E cannot tell a stale reference from a right one; a reference added after E is
a habit.
Clap printing `agentfs-cli`: an explicit name and a test. A new crate missing from the module map: the list derived
from the workspace. An unforwarded `failpoints` feature, or a dependency its rank does not use: A's checks. Two libgit2 setups:
one `agentfs_git::init`. Visibility widened in C: module paths kept, the 497 count held. A library crate making its own
`Meter`: clippy bans `opentelemetry::global::meter*` outside the binary.

## 4. What this gives up

- The engine stays one crate of about 38k lines; a tree edit still runs the tree tests, about 11% less than today
  [est]. The owner's test-time goal rests on PR 0, not on crate boundaries.
- About 240 imports in the recording engine are rewritten in B, for code M9 deletes.
- Nine new crates (repo, failpoints, os, git, lines, engine, referee, service and the front), each a manifest, a module-map entry and a public surface to document; values moved down become `pub`.
- Not delivered here: the duplicated fork rules (a fork's upstream in the Trees row and in `Upstreams`; "no forks of
  forks" checked in both), and a kernel-independent tree view. Each is its own work item. `agentfs-fuse/src/backing.rs`
  keeps its own confined access beside `agentfs_os::confined` (B shares the resolution flags and `proc_path`); it is
  the recording engine's FUSE half, so M9's deletion ends the copy.
- Without release `lto`, calls across the new boundaries are not inlined unless generic or `#[inline]`; C and G re-run
  the release figures.

**Decided without the owner (design choices):** the engine crate is `agentfs-engine` (both architects, independently);
the front does not re-export the recording engine.

**For each PR's code architect:** exact re-exports and public surfaces; nextest's profile (doctests run separately,
test groups for the heavy programs, no retries); a home for the failpoints registry's per-engine groups; `phase-1.yml`
path filters by crate; tikv's `fail` as prior art for failpoints.
