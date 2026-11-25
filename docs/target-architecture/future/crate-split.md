# agentfs-core along domain lines: the crates, and the course to them

**Design; rows A to F of section 3 are built, the rest is not. The owner's answers are in [OWNER-GUIDANCE.md](../../../OWNER-GUIDANCE.md) ("Performance",
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
 LIBRARY      agentfs (front, rank `umbrella`: re-exports only; from M9 a `fuse` feature, the rank then using `filesystem`)
 FILESYSTEM   agentfs-cli (binary `agentfs`: composition root, the OpenTelemetry SDK)
                 └─ agentfs-fuse (tree views, stale mounts)
 LIBRARY      agentfs-service (one volume: the domains wired onto one store, event dispatch, drain;
                 uses the domains below and agentfs-engine's adapters: state area, store, sessions)
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

## 2. What the code does that the end state does not [read]

After A, B and C the tree engine is its own crate, agentfs-engine, beside the recording engine, and the shared leaves
are their own crates. What remains:

- The referee's purity is a convention: `referee/entries.rs` imports the actor's `Error` and `queue`. G's.
- The tree path's launch reads the recording engine in two places, which stay until M9 deletes what they serve (F
  moved the rest to the binary: the telemetry resource's identity, its scope and the export counter's name):
  `crates/agentfs-cli/src/otel.rs` builds core's `telemetry::Telemetry` (with `OtelAlarmLog`) at every `otel::init`,
  a tree mount's included, so a tree process's meter also carries the recording engine's
  `agentfs.nested.gitdir_heuristic`; and `daemon.rs` names core's `service::Stop` in `Daemon::watch`, the recording
  mount's (a tree mount's hold uses `start` and `wait` only).
- The tree view has no kernel-independent layer (`agentfs-fuse/src/tree_view.rs:43` uses `fuser::*`; `:1592`
  implements `Filesystem`), so it is tested only where `/dev/fuse` exists.

## 3. The course

| PR | Does | Needs | Gate | Gain |
|---|---|---|---|---|
| 0 | Tests run with cargo-nextest locally and in CI; the two slow programs (`tree_batch_alloc`, 71 s; `tree_stages`, about 60 s [measured]) cut to a tenth of their volumes locally, keeping their ratios, and run at full size on GitHub Actions; the detection gaps this leaves, local only, are stated in each test | none | ci, image, phase-1 | `agentfs-core`'s test run, built first: 196 s under `cargo test`, 77 s under `cargo nextest run` with its doctests (128 s from nextest alone) [measured, 2026-11-24, development.md "Build settings"]; `agentfs`'s 15 s unchanged [measured] |
| A (built) | agentfs-repo: the repository-wide checks out of core's documentation test (`doc_tables` a module, not a `#[path]` include); the crate list derived from the workspace; checks that dependencies point only down the layers, that no bare `-p agentfs` remains until E (after E a package named `agentfs` exists, and the check cannot tell a stale reference from a right one), that every crate using failpoints forwards the feature | none | ci, docs | the repository's checks build in 6.9 s from an empty target directory, not core's 88 s [measured, development.md]; local runs only: CI's docs job still builds core and both binaries for the reference tables |
| B (built) | agentfs-os, -git, -failpoints, -lines; `RepoPath`, `parse_size`, `parse_duration` to contracts; the tree engine's back-edges into the recording engine cut (`FifoGate` generic, its `CompactGate` impl into the engine; `sha256_at` into `lfs`); csi calls `is_safe_name` | none | ci, image | none alone; makes C, and M9's whole deletion, possible |
| C (built) | agentfs-engine: `tree_actor/`, `tree_git`, `session`, `store`, `state_area`, `operator_settings`, their tests (`it/tree_*`, `tree_*.rs`, the tree half of `crash_points.rs`); its instruments registered on a `Meter` the binary makes; csi through the `StateArea` port; `docs/library/core.md`'s tree half becomes `engine.md`. The crate boundary replaces agentfs-repo's recording-module check; the engine keeps git2, so the ban on opening a remote stays clippy's (`clippy.toml`, the SSH transport's registration rests on it) | B | ci, image, phase-1 | a recording-engine edit builds and runs only the recording engine's tests (23 s and 20 s), a tree edit only the tree engine's (25 s and 64 s), against the 77 s nextest run of all of core [measured, 2026-11-25, development.md "Build settings"] |
| D (built) | The CLI package renamed `agentfs-cli` (directory too), its binary still `agentfs` (`[[bin]] name`, which clap reads through `CARGO_BIN_NAME`); every `-p agentfs` rewritten; agentfs-repo refuses, in a file that runs, a package, feature, binary or test program selected through a variable and a `cargo build` of a package with no binary, and, in any tracked file, a path under `crates/` that names nothing | C | ci, image, docker, hermes | none; with no package named `agentfs`, a missed reference fails |
| E (built) | The front `agentfs`, in a new rank `umbrella` that no rank uses (it uses `shared-kernel`, `domain` and `engine`; listed after `kubernetes`, so M9 only adds `filesystem` to its uses): re-exports of the engine, contracts and the domains; a doctest opens a tree volume through it; AGENTS.md principle 10's mapping updated; the domain invariants and the rows-and-events contract out of `engine.md` to a library page of the shared kernel and the domains, which `engine.md` links | D | ci, docs | the owner's one name |
| F (built) | agentfs-service, in a rank of its own below `umbrella`, which `umbrella` and `filesystem` use: `launcher/domains.rs`, `shared.rs`, `domain_events.rs` and the drain, used by the binary; the front re-exports it; the launch's reads of core (the telemetry resource's identity a binary-local `otel::Identity` built from the pod flags, not core's `identity::Context`, which it replaces at M9: unlike contracts' `PodContext` it holds raw strings and the replica; the telemetry scope and the export counter's name to the binary). Acceptance: the front's doctest goes through the one-call service, and docs/library/agentfs.md's consumer API names that route alone | E | ci, image | consumers compose a volume in one call |
| G | agentfs-referee, rank `domain` (whose uses gain `line-identity`; the front re-exports it as `referee`): the referee's rules (`land`, `sync`, `touches`, `consulted`, `undecided` through its `Landed` port, whose error is the caller's), the queue and locks (`queue`: `Key`, `Plan`, `eligible`, `Cut`, `cut`), each fork's upstream (`Upstreams`) and the values they read (`MergeFileIdentity`, `NameHistory`, `parent_of`; `Meta` with `Kind`, `Perm` and `Attrs`), all `pub`, with its own error enum (`From` into the engine's, today's mapping). The line values (`tree.rs:22-205`: `EventNumber`, `LineVersion`, `Versions`, `Lines`) to lines, the small ones `#[inline]`. The two upward edges cut in the engine: `queue::held` (`queue.rs:50`, takes `&Shape`) becomes `Shape::held`, beside the `keys_of` it repeats; `Upstreams::merges` (`upstreams.rs:66`) becomes `refs::merges`, the ref layout's one home. Purity checked at compile time: the crate is `no_std` outside its tests (a crate's own `clippy.toml` would replace the workspace's, and clippy bans no module); agentfs-repo refuses a pure crate without it, the spawn check reads its sources, and `ci/check-deps.sh` refuses git2, libgit2-sys, libc, rustix or OpenTelemetry in what it links. Also the engine's open items: `Error::Reconcile` becomes `Error::Lines`, still `agentfs_lines::Error` (a leaf's types that reach the API are named by depending on the leaf, docs/library/agentfs.md); the engine's re-export of `TreeId` and `NotATree` removed, and agentfs-repo refuses a crate the front re-exports re-exporting another crate's item; the compaction labels JC6 and JC8 defined (`journal/compaction.rs`'s module doc; errata, E fix round); the engine's `pub` items whose only callers are tests (`History::wait_compacted`, `wait_compactor`, `stages::holds_on_this_thread`) behind `test-util`, by F2's rule (Traps). The trace of `finding-51-sync-fast-forward.patch` (errata, C fix round) is not re-recorded: VERIFYING.md re-records when the fixture's second test fails, which it does not, and the patch's header holds the open question | C | ci, image, phase-1 | purity checked; rule tests in seconds |

The waiting-marks PR lands in the engine after C, beside G (disjoint files); whether its `Waiters` gets a crate is its
architect's call. **Later:** a journal or history crate only if a measurement shows the boundary pays; M4 adds
agentfs-replication and agentfs-transport; M9 deletes agentfs-core whole and fuse's recording half, and the front
gains `fuse`: its rank `umbrella` then uses `filesystem`, which is why no rank uses `umbrella` (a rank using it under
`filesystem` would then be a cycle) and why it is listed after `kubernetes`.

**Traps, each closed by a check, and what each check does not read.** A stale `-p agentfs` meant for the binary: D
before E, then A's check; after E it names the front, so in a file that runs agentfs-repo refuses a `cargo build` of a
package with no binary (a script builds to run what it built). A stale `cargo test` or `nextest run -p agentfs` after
E, or one in markdown, is in form a right use of the front: not checked. Prose naming a package ("`agentfs` is a
binary only") is read by no check: each rename's acceptance (E, F, G) runs ``git grep -nE '`agentfs`[^|]{0,60}(crate|package|manifest|tests|program|library|binary only)'``
over everything but the records of past work, and every hit is read.
Clap printing `agentfs-cli`: the bin's name read through `CARGO_BIN_NAME`, and a test. A script selecting through a
variable, which no check can read: agentfs-repo refuses, in a file that runs (not markdown, not Rust, not an issue
template), a package, feature, binary or test program that is not a literal name; it reads shell words and Python
calls written as one list. Not read: cargo run through a variable (`$CARGO`) or built by code other than one list.
A path left to a renamed crate: agentfs-repo's check that every path under `crates/` a tracked file names exists, but
in the records of past work (vendor/, `.claude/handoff/`, the errata, CHANGELOG.md, OWNER-GUIDANCE.md, patches); in
Rust it reads whole-line comments only, so a comment after code, a `/* */` comment and string literals (left to the
compiler and the tests that open them) are not read. A new crate missing from the module map: the list derived
from the workspace. An unforwarded `failpoints` feature, or a dependency its rank does not use: A's checks. Two libgit2 setups:
one `agentfs_git::init`. Visibility widened in C: module paths kept, the 497 count held. A library crate making its own
`Meter`: clippy bans `opentelemetry::global::meter*`, `MeterProvider::{meter, meter_with_scope}` and `Meter::new`,
allowed only in the binary's `otel::meter_of` and agentfs-test-metrics (a library building an SDK provider is not
checked). Two crates documented under one name (cargo#6313: rustdoc writes `target/doc/<crate>/`, so the bin `agentfs`
overwrote the front's pages under `cargo doc --workspace`; with both crates rebuilt the run can fail outright, as both
write `target/doc/agentfs/` at once): `doc = false` on agentfs-cli's bin; no check, as no gate, workflow or page runs `cargo doc --workspace`. The front reaching the recording engine:
`ci/check-deps.sh` reads what `cargo tree -p agentfs` links and refuses agentfs-core, fuser, the OpenTelemetry SDK and
every TLS and SSH crate. The ranks cannot say it while both engines share the rank `engine`, and two engine ranks would
lose the dev-dependency rule that keeps one engine's tests from building the other (the reason agentfs-test-metrics is
a crate); M9 deletes core and with it that line.
The front growing code of its own, or missing a crate it should re-export (a domain added at M4, F's service):
agentfs-repo reads the front's `src/lib.rs` and refuses anything but comments and `pub use <crate> as <module>;`, and
requires one such module, named for its crate without `agentfs_`, for each library of the ranks the front's rank uses
but agentfs-core (named as the one exception until M9), with its normal dependencies exactly those crates. Not read:
what each re-exported crate makes `pub`.
A second composition of a volume beside the service: agentfs-repo's `only_the_service_composes_the_domains` refuses a
normal or build dependency on a domain the service composes from any crate but the `service`, `umbrella` and `tests`
ranks. Not read: dev-dependencies and the `tests` rank, so agentfs-conformance composes Trees itself
(`mounted.rs`), as a test may; and a consumer outside the workspace, which may wire the domains' services itself.
The library turning on a test build: `ci/check-deps.sh`'s `library features` refuses `test-util` or `failpoints` of
any agentfs crate in what `cargo tree -p agentfs` builds, and CI's lint job builds the libraries and the front's
doctest without them. Not read: a consumer that depends on agentfs-engine by name with `test-util`; and which `pub`
items only tests call. F2 narrowed a list read by hand, and no check finds an item a test-only caller still keeps
`pub` (G re-reads it, row G). The clippy bans of `TreeGit::open` and `TreeGit::init` carry `allow-invalid = true`, as
the paths resolve only under `test-util`: a rename or move of either drops its ban without a word. Not checked.

## 4. What this gives up

- The engine stays one crate of about 38k lines; a tree edit still runs every tree test: 64 s against the 77 s run
  of all of core [measured, row C]. The owner's test-time goal rests on PR 0, not on crate boundaries.
- About 240 imports in the recording engine are rewritten in B, for code M9 deletes.
- Ten new crates (repo, failpoints, os, git, lines, engine, test-metrics, referee, service and the front), each a manifest, a module-map entry and a public surface to document; values moved down become `pub`. test-metrics is about 160 lines in the `tests` rank, a crate only because the rank check keeps the engines apart.
- Not delivered here: the duplicated fork rules (a fork's upstream in the Trees row and in `Upstreams`; "no forks of
  forks" checked in both), and a kernel-independent tree view. Each is its own work item. `agentfs-fuse/src/backing.rs`
  keeps its own confined access beside `agentfs_os::confined` (B shares the resolution flags and `proc_path`); it is
  the recording engine's FUSE half, so M9's deletion ends the copy.
- The front re-exports each crate whole (E), so the library's surface is the crates' `pub`, which also serves the
  sibling crates (fuse, the binary, conformance), and a consumer's build never reads this workspace's `clippy.toml`.
  **F's decision:** the front keeps the engine whole, and the engine's items `pub` only for the workspace's tests
  (the store-only row writes, `close_writer`, the hand-made-id `create`s, the direct `open`s, `TreeGit`'s opens, the
  compaction gate) are `pub(crate)` or `pub` only under its `test-util` feature, which the library never enables
  (`ci/check-deps.sh`). The consumer's route is the service (docs/library/agentfs.md, "The consumer API"); a
  consumer's `main` is its composition root, so `Minter::new` stays consumer API. The alternative, a front of
  contracts, the domains and the service with the engine reached by name, narrows nothing (naming the engine reaches
  all of it), needs a second dependency or re-exports for the engine types the service's API carries, and drops the
  engine from principle 10's library. What stays reachable and outside the route, listed in docs/library/agentfs.md:
  the domains' services, `TreeStateArea` and `TreeStore` wired by hand, the `StateArea` port's `create`, the guard's
  calls that name a writer, and any consumer that turns `test-util` on itself. The tree library (engine and service
  alike) reports failures it cannot hand back as `tree <Name>` lines on stderr, not through OpenTelemetry; moving them
  to an alarm port is one change for both. The engine's `Error::Lines` (G's name for `Error::Reconcile`) exposes a
  leaf's type (`agentfs_lines::Error`), as the referee's API exposes lines' values: a consumer names them by depending
  on agentfs-lines; re-exporting the leaves from the front would make `umbrella` use `line-identity` and re-export
  align too. G removes the engine's re-export of the shared kernel's `TreeId` and `NotATree`, which gave the library
  two paths to one value.
- Without release `lto`, calls across the new boundaries are not inlined unless generic or `#[inline]`; C and G re-run
  the release figures (G's line values, hot in the builder's and the referee's maps, are `#[inline]`).

**Decided without the owner (design choices):** the engine crate is `agentfs-engine` (both architects, independently);
the front does not re-export the recording engine; no workspace crate depends on the front (inside the workspace each
item has one path, its own crate's, and at M9 a rank under `filesystem` using it would be a cycle).

**For each PR's code architect:** exact re-exports and public surfaces; the `umbrella` rank (E adds it to the
workspace's ranks and docs/architecture.md "Ranks"), and F's service rank; nextest's profile (doctests run separately,
test groups for the heavy programs, no retries); a home for the failpoints registry's per-engine groups; `phase-1.yml`
path filters by crate; tikv's `fail` as prior art for failpoints.
