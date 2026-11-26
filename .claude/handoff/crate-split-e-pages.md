# Crate split, PR E: frozen pages

Design: `docs/target-architecture/future/crate-split.md` (section 3 row E; section 1 the end state). Branch
`claude/bold-hopper-72ixu2`, PR #47, base of this run 5479dba. One slice, E1. Owner's words (OWNER-GUIDANCE.md
"Performance", all general guidance): "consumers should link against something named "agentfs"" (2026-11-24), the thin
re-export front: "A sounds right"; the front is the library only until M9: "B is fine"; on carrying D to G on PR #47
(2026-11-25): "Yes". Counts are `git grep` lines at the skeleton, excluding `.claude/handoff/`, `.claude/errata.md`,
CHANGELOG.md and OWNER-GUIDANCE.md. A to D idioms hold: ranks, agentfs-repo's checks, no shims, docs in the same commit.
**The skeleton** (the commits carrying this file, 05e0d0e and its revision) holds the crate whole as interface:
`crates/agentfs/{Cargo.toml,src/lib.rs}` (rank `umbrella`; six normal dependencies; six `pub use ... as ...;`; no
dev-dependency, no doctest), the member, the rank `{ name = "umbrella", uses = ["shared-kernel", "domain", "engine"] }`
listed after `kubernetes`, the root manifest's rank comment cut to a pointer at docs/architecture.md "Ranks",
`Cargo.lock`, the Dockerfile's stub, architecture.md's Ranks row and module-map section `agentfs`, `doc = false` on
agentfs-cli's `[[bin]]` (cargo#6313), crate-split.md amended (rank `umbrella`, row F's rank, M9, Traps, section 4) and
the errata entry. User docs never call the crate "the front" (`agentfs front` is a command): "the library crate `agentfs`".

## Page E1

**Title.** Consumers depend on one crate, `agentfs`, whose doctest opens a tree volume through `agentfs::` alone;
no workspace crate depends on it; the library's supported surface and the domains' contract each have a page.
**The crate (frozen, in the skeleton).** `pub use agentfs_{clients,contracts,engine,rules,trees,volume} as
{clients,contracts,engine,rules,trees,volume};` nothing else, no feature. Not re-exported: agentfs-core, agentfs-fuse
(until M9) and the leaves (os, git, lines, failpoints, align). E1 adds `[dev-dependencies] tempfile = "3.27.0"` and the
crate doc: what it is, one sentence mapping modules to crates, "See docs/library/agentfs.md", then this doctest
(run at the skeleton: 1 passed, 0.07 s). The engine assigns the id; `tempfile` is hidden scaffolding:
```rust
use agentfs::contracts::TreeId;                       // the shared kernel's path, not tree_actor's re-export
use agentfs::contracts::clients::FileOwner;
use agentfs::contracts::name::{VolumeName, WriterLabel};
use agentfs::contracts::pod::{PodContext, WriterAt};
use agentfs::contracts::ports::StateArea;
use agentfs::engine::state_area::TreeStateArea;
use agentfs::engine::tree_actor::{Attrs, Perm};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let root = dir.path();
let at = PodContext::default();
let mut area = TreeStateArea::at(root);
// The engine gives the new volume its id.
let (identity, _created) = area.create(&VolumeName::parse("notes")?, None, &at)?;
assert_eq!(area.open(&at)?, identity);
let volume = area.recording().ok_or("open serves the volume")?;
let by = WriterAt { writer: WriterLabel::parse("docs")?, at };
let file = Attrs { perm: Perm::from_mode(0o644), owner: FileOwner { uid: 1000, gid: 1000 } };
let handle = volume.actor()?.write_open(TreeId::Root, "hello.txt", &by, Some(file))?;
volume.actor()?.write(handle, 0, b"hello\n", false)?;
volume.close_with(handle, |_| Ok(()))?;
assert_eq!(volume.actor()?.read(TreeId::Root, "hello.txt")?, b"hello\n");
# Ok(())
# }
```
CI reaches it unchanged: ci.yml:90 `cargo test --doc --workspace --locked`.
**agentfs-repo** (`tests/repo/crates.rs`). `rank_problems`: a dependency of any kind, dev included, on a member whose
rank no rank uses (itself included) is refused: `"{from} -> {to} (no rank uses {rank})"`. Fixtures: `ranks()` gains
`umbrella`; `today()` gains `member("agentfs", "umbrella", &["agentfs-contracts", "agentfs-volume"])`; the edge table
gains `agentfs-fuse -> agentfs` and `agentfs-csi -> agentfs` (normal) and a dev `agentfs-fuse -> agentfs`, one problem
each. The `front` fixture's member takes rank `umbrella`. architecture.md's Ranks prose: a rank no rank uses takes no
dependency, dev included. No doc-collision check (`doc = false` and crate-split.md's Traps carry it).
**ci/check-deps.sh.** Header: what the binary and the library link. A literal second closure, `library() { cargo tree
--locked -p agentfs -e normal --prefix none --format '{p}' | awk '{print $1}' | sort -u; }`, and one `ok   library`
failing on `agentfs-core|fuser|opentelemetry_sdk|opentelemetry-otlp` and `$NO_TLS|libssh2-sys`. agentfs-fuse is not
listed: the rank check refuses it. Why core is here and not in the ranks: crate-split.md Traps.
**Docs.** New `docs/library/agentfs.md` ("The library: `agentfs`"): the module table, each row linking its page;
depending on it by path or git; the doctest named, not copied; what is not re-exported and why; **the consumer API**:
the shared kernel's values, traits, events and ports; the domains' services; the engine's `state_area::TreeStateArea`
(volumes made and opened through the `StateArea` port), `tree_actor::Recording` and its guard's tree calls,
`session::Sessions`, `store::TreeStore`; and **outside it, `pub` for the workspace's own crates and unguarded for a
consumer** (this workspace's `clippy.toml` does not reach a consumer's build): `contracts::caps::Minter::new`;
`TreeActor::{fork_with, record_rows, drop_fork, close_writer, create, create_with}`; `tree_git::TreeGit::{open,
open_with_lease, init, init_with_lease}`; `tree_actor::{COMPACT_GATE_VAR, ChannelGate, CompactGate, Gap}`; PR F owns
the fix. `cargo nextest run -p agentfs` finds no tests; `cargo test --doc -p agentfs` runs it. New `docs/library/
domains.md` ("The library: the shared kernel and the domains"): engine.md's "Domain invariants" whole, its enforcement
column saying where a ban holds in this workspace only (CL1's `Minter::new`), and from "Rows and domain events" the
domains' half (each domain's service, ports and calls, rows and events, `StoreError` and each `From`, the event sink and
the composition root's dispatch). engine.md keeps `### The domains' store` (`TreeStore`, chains, `RowEvent`, one
journal step, `drop_fork`, `service_at`, `TreeStateArea`'s three ports, `store::io_kind`), linking domains.md.
**Labels.** The Clients domain keeps C1-C7 and Volume VO1-VO6, now on domains.md. Compaction's C1-C9 and C2a (defined
in `journal/compaction.rs`'s module doc) become JC1-JC9 and JC2a: 24 lines in built.rs, compactor.rs,
compactor/tests.rs, journal/compaction.rs, fold.rs, journal/mod.rs, tree_actor/mod.rs. Domain citations name
domains.md: by path, contracts caps.rs:27,59, clients.rs:215,224,259,265, ports.rs:193, trees.rs:100, volume.rs:115,142;
volume release.rs:1; cli domain_events.rs:1-2, shared.rs:2; bare (the file names domains.md once): clients
service.rs (8), volume service.rs:413, engine session.rs:5-6, tests it/tree_clients.rs (4), it/tree_store.rs:745, cli
launcher/mod.rs:108. Docs re-pointed: recording-failures.md:286, domains/clients.md:5, domains/volume.md:5-6,
one-engine.md:124, system.md:8,78,124. Left: the fork-delivery tests' own C1-C7 (core fork/deliver.rs:717,
cli tests cli_fork_deliver.rs), the recording engine's, which M9 deletes.
**Other docs.** engine.md and core.md intros (engine reached as `agentfs::engine`; core is not in `agentfs`);
SUMMARY.md and docs/README.md (both pages, agentfs.md first under Library); architecture.md Crates row `agentfs`;
AGENTS.md principle 10's mapping, the `docs/library/` row and a Layout row `crates/agentfs`; README.md:253;
supported-configurations.md:62; development.md:75 and :231; crate-split.md row E "(built)", section 2's E item gone;
CHANGELOG "Added".
**Acceptance.** `cargo metadata`: `agentfs` at `crates/agentfs`, rank `umbrella`. `bash ci/check-deps.sh`: seven `ok`.
`for f in $(git grep -lE '\b(C[1-7]|VO[1-6])\b' -- crates ':!crates/agentfs-core' ':!crates/agentfs-cli/tests'
':!*.patch'); do grep -q docs/library/domains.md "$f" || echo "$f"; done` prints nothing; `git grep -nE
'\bC[1-9]a?\b' -- crates/agentfs-engine` hits only session.rs and the two tests; `git grep -n 'engine.md#\(domain-
invariants\|rows-and-domain-events\)'` finds nothing; the Traps prose grep, every hit read. Gates: `cargo fmt --all
--check`; clippy `-D warnings` and `cargo nextest run` of agentfs-repo, -contracts, -volume, -clients, -trees, -engine
(and `--features failpoints`), agentfs-cli (clippy only); `cargo test --doc -p agentfs`.
**Probes.** (1) `agentfs = { path = "../agentfs" }` in agentfs-fuse's `[dependencies]`, then `[dev-dependencies]`
→ `dependencies_follow_the_ranks`. (2) `agentfs-core` in agentfs's dependencies → check-deps "library links
agentfs-core". (3) `-p agentfs` at alerts.yml:63 → `cargo_commands_name_what_the_workspace_has` (run: fails "cargo
build -p agentfs: it has no binary for this file to run"). (4) `pub use agentfs_engine as tree;` → the doctest.
**Files.** `crates/agentfs/{Cargo.toml,src/lib.rs}`, `Cargo.lock`; agentfs-repo `tests/repo/crates.rs`;
`ci/check-deps.sh`; the comments and crate docs named under Labels; `docs/library/{agentfs,domains,engine,core}.md`,
`docs/{SUMMARY,README,architecture,supported-configurations,development}.md`, the docs re-pointed, crate-split.md;
AGENTS.md, README.md, CHANGELOG.md.

## Decisions

**Slice order.** One slice: a crate of re-exports whose doctest, rank rule, linkage check and docs must land together.

**Design choices.**
1. Each crate re-exported whole (bevy's and gix's `pub use <crate> as <module>`): each item keeps one home. Cost: the
   library's surface is the crates' `pub`, which also serves fuse, the binary and conformance; agentfs.md names what is
   outside the consumer API, and F owns the facade or visibility fix (crate-split.md section 4).
2. Rules is re-exported (a domain; the engine's `rules()` returns `Arc<agentfs_rules::RuleSet>`); leaves are not. Leaf
   types that reach the engine's API (`agentfs_lines::Image`, `From<agentfs_lines::Error>`, `FileLock`) need the leaf
   by name; agentfs.md says so.
3. Rank `umbrella`, used by no rank, listed after `kubernetes`. At M9 the front gains `fuse` and its rank uses
   `filesystem`; a rank under `filesystem` using it would then be a cycle. Inside the workspace each item has one path,
   its crate's. Both are design choices, not the owner's (errata 2026-11-25, E). F's service takes its own rank below.
4. The doctest makes its volume through the engine's `StateArea` port, so the engine assigns the id (VO5); F's one-call
   volume replaces this route.
5. check-deps.sh keeps the library's linkage, agentfs-core included: two engine ranks would lose the dev rule that keeps
   one engine's tests from building the other.
6. Compaction's labels are renamed JC, not the Clients domain's: they are engine-internal (24 comment lines, one
   module tree), while C1-C7 appear in runbooks and design pages.

**Review round (2026-11-25), one line per finding.** P1 doctest: applied (state-area route, no uuid, run). P1 surface:
applied (agentfs.md's consumer API and its outside list; F owns the fix; crate-split.md section 4). P2 rank errata:
applied (errata entry; M9 grounds it; section 1, rows E and F, "Later"). P2 check-deps second home: partly applied
(agentfs-fuse dropped; core kept, justified in Traps). P2 `TreeId`: applied (doctest uses `contracts::TreeId`; the
engine's re-export routed to G). P2 labels: applied (exact list, bare ones included; compaction renamed JC; correction:
its labels have a home, `journal/compaction.rs`'s module doc, not the deleted page). P2 root manifest comment: applied
(a pointer, in the skeleton). Nit doc check: applied (dropped; Traps name cargo#6313). Nit rank word: applied
(`umbrella`; new term below).

**New terms.** `umbrella`: the rank of the crate `agentfs`, the one name consumers depend on; Rust's usual word for a
crate that re-exports others; not `library`, the Layer of five ranks. `JC1`-`JC9`: compaction's invariant labels.
