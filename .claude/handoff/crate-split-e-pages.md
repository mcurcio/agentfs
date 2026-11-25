# Crate split, PR E: frozen pages

Design: `docs/target-architecture/future/crate-split.md` (section 3 row E; section 1 the end state). Branch
`claude/bold-hopper-72ixu2`, PR #47, base of this run 5479dba. One slice, E1. Owner's words (OWNER-GUIDANCE.md
"Performance", all general guidance): "consumers should link against something named "agentfs"" (2026-11-24), the thin
re-export front: "A sounds right"; the front is the library only until M9: "B is fine"; on carrying D to G on PR #47
(2026-11-25): "Yes". Counts are `git grep` lines at 5479dba, excluding `.claude/handoff/`, `.claude/errata.md`,
CHANGELOG.md and OWNER-GUIDANCE.md. A to D idioms hold: ranks, agentfs-repo's checks, no shims, docs in the same commit.
**The skeleton commit** (the one carrying this file) holds the crate whole as interface: `crates/agentfs/{Cargo.toml,
src/lib.rs}` (rank `library`; six normal dependencies; six `pub use ... as ...;` lines; no dev-dependencies, no
doctest), the member in `Cargo.toml`, the rank `{ name = "library", uses = ["shared-kernel", "domain", "engine"] }`
after `engine`, `Cargo.lock`, the Dockerfile's stub (COPY, `mkdir`, `touch`), docs/architecture.md's Ranks row and
module-map section `agentfs` (`lib.rs`), and `doc = false` on agentfs-cli's `[[bin]]` (`cargo doc --workspace`
warned "output filename collision at target/doc/agentfs/index.html", cargo#6313; with it, no warning). Its `lib.rs`
names crate-split.md; E1 points it at the new page. Term: user docs never call the crate "the front": `agentfs front`
is a command. Say "the library crate `agentfs`".

## Page E1

**Title.** Consumers depend on one crate, `agentfs`, which opens a tree volume in its doctest; nothing in the workspace
depends on it, two crates are never documented under one name, and the domains' contract has a page of its own.
**The crate (frozen, in the skeleton).** `crates/agentfs`, package and lib `agentfs`, rank `library`. Its whole
public surface, each a crate re-exported whole (prior art: bevy's and gix's `pub use <crate> as <module>`):
```rust
pub use agentfs_clients as clients;     pub use agentfs_contracts as contracts;
pub use agentfs_engine as engine;       pub use agentfs_rules as rules;
pub use agentfs_trees as trees;         pub use agentfs_volume as volume;
```
No feature, no item of its own. Not re-exported: agentfs-core and agentfs-fuse (until M9), and the leaves (os, git,
lines, failpoints, align). E1 adds `[dev-dependencies] tempfile = "3.27.0"`, `uuid = { version = "1",
default-features = false, features = ["std"] }` (`VolumeId` wraps `uuid::Uuid`), and the crate doc: what it is, the
module-to-crate table in one sentence, "See docs/library/agentfs.md", then this doctest (prototyped at the skeleton:
`cargo test --doc -p agentfs`, 1 passed, 0.17 s), using `agentfs::` paths only:
```rust
use agentfs::contracts::clients::FileOwner;
use agentfs::contracts::name::{VolumeName, WriterLabel};
use agentfs::contracts::pod::{PodContext, WriterAt};
use agentfs::contracts::volume::{VolumeId, VolumeIdentity};
use agentfs::engine::tree_actor::{Attrs, Perm, TreeActor, TreeId};
// fn main() -> Result<(), Box<dyn std::error::Error>>, hidden with `# `
let state = tempfile::tempdir()?;  let service = PodContext::default();
let identity = VolumeIdentity { name: VolumeName::parse("notes")?, id: VolumeId(uuid::Uuid::from_u128(1)) };
drop(TreeActor::create(state.path(), &identity, None, &service)?);
let mut volume = TreeActor::open(state.path(), &service)?;
let by = WriterAt { writer: WriterLabel::parse("docs")?, at: service };
let file = Attrs { perm: Perm::from_mode(0o644), owner: FileOwner { uid: 1000, gid: 1000 } };
let handle = volume.write_open(TreeId::Root, "hello.txt", &by, Some(file))?;
volume.write(handle, 0, b"hello\n", false)?;  volume.close(handle)?;
assert_eq!(volume.read(TreeId::Root, "hello.txt")?, b"hello\n");
```
CI reaches it unchanged: ci.yml:90 runs `cargo test --doc --workspace --locked` (default and all features).
**agentfs-repo.** (1) `src/workspace.rs`: `pub struct Target { pub name: String, pub kinds: Vec<String>, /// cargo
metadata's `doc`: whether `cargo doc` documents it. pub doc: bool }`, read from `RawTarget`. Fixture sites: `member()`
(`doc: true`), `binary()` (`true`), the `csi_trees` test target (`false`). (2) `tests/repo/crates.rs`:
```rust
/// Each crate name two packages' documented targets share: rustdoc writes `target/doc/<crate>/`, so one overwrites
/// the other (cargo#6313). Documented: `doc` and kind `lib`, `proc-macro` or `bin`, but a bin named as its own
/// package's lib (cargo documents that lib alone). Crate name: the target's, `-` read as `_`.
fn doc_collisions(workspace: &Workspace) -> Vec<String>;
#[test] fn cargo_doc_writes_each_crate_once();   // Workspace::load(); asserts > 10 documented targets read
#[test] fn a_doc_collision_is_found();           // lib agentfs + other package's bin agentfs: 1; that bin doc=false: 0;
                                                 // agentfs-csi's lib and bin in one package: 0
```
(3) `rank_problems`: a dependency of any kind, dev included, on a member whose rank no rank uses (itself included) is
refused: `"{from} -> {to} (no rank uses {rank})"`. `ranks()` gains `library`; `today()` gains `member("agentfs",
"library", &["agentfs-contracts", "agentfs-volume"])`; the edge table gains `agentfs-fuse -> agentfs` and
`agentfs-csi -> agentfs` (normal) and a dev edge `agentfs-fuse -> agentfs`, each one problem. (4) The `front`
fixture's member gets rank `library`; its comments say "the library crate `agentfs`". Module docs name the new checks.
**ci/check-deps.sh.** A literal second closure (the literal rule refuses `-p "$x"`): `library() { cargo tree --locked
-p agentfs -e normal --prefix none --format '{p}' | awk '{print $1}' | sort -u; }` and one check, `ok   library`,
failing on `agentfs-core|agentfs-fuse|fuser|opentelemetry_sdk|opentelemetry-otlp|` plus the TLS/SSH set
(`ring|rustls|russh|aws-lc-rs|aws-lc-sys|openssl|openssl-sys|native-tls|libssh2-sys`). Header comment says so. At the
skeleton the closure holds none of them (tokio is in it, through the leaves; not refused).
**Docs.** New `docs/library/agentfs.md` ("The library: `agentfs`": what it is; the module table, each row linking its
page; depending on it by path or git, not on crates.io; the doctest named, not copied; what it does not re-export and
why; `ci/check-deps.sh`'s check; `cargo nextest run -p agentfs` finds no tests: `cargo test --doc -p agentfs`). New
`docs/library/domains.md` ("The library: the shared kernel and the domains"): engine.md's "Domain invariants" moved
whole, and from "Rows and domain events" the domains' half: each domain's service, its ports and their calls, its rows
and events, `StoreError` and each domain's `From`, the event sink and the composition root's dispatch. engine.md keeps
the adapter's half under a new heading `### The domains' store` (`TreeStore`, chains, `RowEvent`, one journal step,
`drop_fork`, `service_at`, `TreeStateArea`'s three ports, `store::io_kind`) and links domains.md; the rename makes every
old anchor fail the link check. Re-point: recording-failures.md:286, domains/clients.md:5, domains/volume.md:5-6,
one-engine.md:124, system.md:8,78,124. Code comments citing engine.md for C1-C7, VO1-VO6 or "Rows and domain events"
→ domains.md: contracts `caps.rs` 2, `clients.rs` 4, `ports.rs` 1, `trees.rs` 1, `volume.rs` 2; volume `release.rs` 1;
cli `domain_events.rs` 1, `shared.rs` 1; the clients, volume, trees and contracts crate docs name domains.md. engine.md
and core.md intros: engine reached as `agentfs::engine`; core is not in `agentfs`. SUMMARY.md and docs/README.md: both
pages, `agentfs.md` first under Library. architecture.md: Crates row `agentfs` (library); Ranks prose: a rank no rank
uses takes no dependency, dev included. AGENTS.md: principle 10 ("The library is the crate `agentfs`, over the two
engines ... and the domains; it re-exports the tree engine's side"), the `docs/library/` row, a Layout row
`crates/agentfs`. README.md:253 → `docs/library/agentfs.md`, "Building on `agentfs`". supported-configurations.md:62:
the tree engine's row names crate `agentfs` (`agentfs::engine::...`). development.md: the doctest sentence (:75) and
check-deps (:231). crate-split.md: status "A to E built", row E "(built)" with its rank sentence as built, section 2's
E item gone, Traps gain both new checks, row F: agentfs-service may not take rank `library` (no rank uses it): a rank
of its own below it, which `library` and `filesystem` use. CHANGELOG "Added": one bullet.
**Acceptance.** `cargo metadata`: `agentfs` at `crates/agentfs`, rank `library`, one lib target; agentfs-cli's bin
`doc: false`. `cargo doc --workspace --no-deps`: no collision warning. `bash ci/check-deps.sh`: seven `ok`. The Traps
prose grep and `git grep -nE 'engine\.md[^)]{0,40}(C[0-9]|VO[0-9]|Rows and domain|Domain invariants)'` (outside the
records) find nothing stale. Gates: `cargo fmt --all --check`; `cargo clippy -p agentfs -p agentfs-repo -p agentfs-cli
-p agentfs-contracts -p agentfs-volume -p agentfs-clients -p agentfs-trees --all-targets -- -D warnings`; `cargo
nextest run -p agentfs-repo -p agentfs-volume -p agentfs-clients -p agentfs-trees -p agentfs-contracts`; `cargo test
--doc -p agentfs`. NOT RUN locally: nothing CI-only is touched.
**Probes.** (1) `agentfs = { path = "../agentfs" }` in agentfs-fuse's `[dependencies]`, then in its
`[dev-dependencies]` → `dependencies_follow_the_ranks`. (2) drop `doc = false` → `cargo_doc_writes_each_crate_once`.
(3) `agentfs-core` in agentfs's dependencies → check-deps.sh "library links agentfs-core". (4) `-p agentfs-cli` →
`-p agentfs` at alerts.yml:63 → `cargo_commands_name_what_the_workspace_has` ("cargo build -p agentfs: it has no binary
for this file to run"; run at the skeleton: fails as stated). (5) `pub use agentfs_engine as tree;` → the doctest.
**Files.** `crates/agentfs/{Cargo.toml,src/lib.rs}`; `Cargo.lock`; agentfs-repo `src/workspace.rs`,
`tests/repo/crates.rs`; `ci/check-deps.sh`; the crate docs and comments named; `docs/library/{agentfs,domains,engine,
core}.md`, `docs/{SUMMARY,README,architecture,supported-configurations,development}.md`, the five docs re-pointed,
crate-split.md; AGENTS.md, README.md, CHANGELOG.md.

## Decisions

**Slice order.** One slice: E adds one crate of re-exports, and everything else is its doctest, two checks and docs
that must travel with it. Split, the docs-only half would be a slice under an hour with nothing to compile.

**Design choices.**
1. One module per crate, each re-exported whole (`pub use agentfs_engine as engine`), not a curated list or a glob.
   Each item keeps one home (its crate's `pub`); a curated list in the front would be a second home deciding what is
   public, and drift. A glob (`pub use agentfs_engine::*`) would let a later module of the engine be shadowed silently
   by the front's `volume` or `rules`. Prior art: bevy (`bevy::ecs`), gix (`gix::hash`). Cost: paths are one segment
   longer (`agentfs::engine::tree_actor::TreeActor`), and the engine's whole `pub` surface is the library's.
2. Rules re-exported: a domain (rank `domain`), and the engine's public `rules()` returns `Arc<agentfs_rules::RuleSet>`.
   Leaves not re-exported: they are mechanisms, not the library's concepts. rustdoc of the engine shows the leaks: a
   `FileMap` implements `agentfs_lines::Image`, `tree_actor::Error` has `From<agentfs_lines::Error>` and
   `From<agentfs_rules::RulesError>`, `tree_git::TreeGit` takes an `agentfs_os::file_lock::FileLock`. A consumer of
   those depends on the leaf by name; agentfs.md says so.
3. Deviation from row E: no rank uses `library`. Row E said `filesystem` and `tests` use it, but no package of either
   needs the front, and the brief's "workspace crates never depend on the front" then rests on the rank check, not on a
   habit. The check is widened so a dev edge counts too (a rank nothing uses is a top). Cost: F cannot put
   agentfs-service in `library`; crate-split.md row F is amended to say so. The rank does not use itself in E (one
   member); F's front-to-service edge goes down a rank.
4. Deviation, stronger than the brief's "set doc = false": a check that no two packages document one crate name. The
   collision is silent at every gate (a cargo warning, not rustdoc's) and the loser could be the library's own pages.
   Prior art: cargo's own warning; cargo metadata exposes `doc` per target, so the check reads cargo, not a list.
5. check-deps.sh guards what a consumer links: no recording engine (the rank check allows `library` to use `engine`,
   which holds agentfs-core), no FUSE, no OpenTelemetry SDK (the library uses the API only, docs/library/engine.md),
   no TLS or SSH (the engine opens no remote). One home for linkage checks, the script CI's lint job already runs.
6. The doctest lives in `lib.rs` only; agentfs.md names it and does not copy it (one tested home). Not
   `#![doc = include_str!("../../../docs/...")]`: a path outside the package breaks `cargo package`, and the page's
   links would be broken in rustdoc.
7. A separate `domains.md` rather than a section of `agentfs.md`: the crate's page is the entry point (what to depend
   on), the domains' page their contract (invariants, ports, events). engine.md's heading is renamed so the link check
   finds every stale anchor; label citations in comments are a grep in the acceptance (no check reads them).
8. The skeleton is the frozen interface as code: the six re-exports compile and pass every gate, so the slice cannot
   drift on names. `doc = false` is in the skeleton because the skeleton introduces the collision.

**Measured at the skeleton.** `cargo test --doc -p agentfs` with the doctest above: 1 passed (13 s from a warm target,
0.17 s run). `cargo nextest run -p agentfs`: "no tests to run", exit 4, hence the doctest gate. D's script rule on a
real file: `cargo build --locked -p agentfs` at alerts.yml:63 fails `cargo_commands_name_what_the_workspace_has` (the
brief's confirmation; reverted). A stale `cargo test -p agentfs` or one in markdown still passes (crate-split.md Traps).

**For F's architect.** A volume made directly needs `uuid` (`VolumeId(pub uuid::Uuid)`): the service's one-call
volume should make the id (as `TreeStateArea`'s `ensure` does), so a consumer never names uuid.

**New terms.** None for users. "The front" stays design-page shorthand: user docs say "the library crate `agentfs`",
since `agentfs front` is a command and "fronts" are its processes.

**Design review outcome.** Self-checked against the code at 5479dba (implementation-workflow: no review of the cut
before code): every count is a `git grep`; the collision, the doctest, nextest's empty run and probe (4) were run.
Untested until E1 lands: the new checks, the check-deps line, probes (1) to (3) and (5), the docs split.
