# Crate split, PR D: frozen pages

Design: `docs/target-architecture/future/crate-split.md` (section 3 row D; section 1 the end state). Branch
`claude/bold-hopper-72ixu2`, PR #47, base of this run 5823ee0. One slice, D1. Owner's words: OWNER-GUIDANCE.md
"Performance" (2026-11-24, 2026-11-25): consumers "should link against something named "agentfs"" (E takes the name);
on carrying D to G on PR #47, 2026-11-25: "Yes". Counts are `git grep` lines at 5823ee0, excluding `.claude/handoff/`,
`.claude/errata.md`, CHANGELOG.md and OWNER-GUIDANCE.md (dated text keeps its paths). A, B and C idioms hold: ranks,
agentfs-repo's checks, no shims, docs in the same commit. The skeleton commit holds this file only: D adds no crate,
and the rename cannot land half-way green (every `-p agentfs` fails A's check until rewritten).

## Page D1

**Title.** The CLI package is `agentfs-cli`, its binary still `agentfs`; no package is named `agentfs`, and a reference
left to the old package or its directory fails a check.
**The package.** `git mv crates/agentfs crates/agentfs-cli`, whole; rank `filesystem`, dependencies, features, sources
unchanged. Manifest: `name = "agentfs-cli"`, plus `[[bin]] name = "agentfs"`, `path = "src/main.rs"` (Cargo's idiom).
Root members `"crates/agentfs"` → `"crates/agentfs-cli"` in place; commit `Cargo.lock`. `src/main.rs`:
`#[command(name = "agentfs", version = VERSION, about = ...)]` (clap's derive otherwise names it `CARGO_PKG_NAME`:
`--version` would print `agentfs-cli 0.1.0`). No Rust import changes: no manifest depends on package `agentfs`, and a
binary's crate name is its target's, so `crate::` paths and `CARGO_BIN_EXE_agentfs` (40 lines, 24 test files) stay.
**New tests.** `src/main.rs` `mod tests`: `#[test] fn the_command_is_named_agentfs()`: `Cli::command()`'s `get_name()`
is `"agentfs"`, `render_version()` is `format!("agentfs {VERSION}\n")`, `render_usage().to_string()` starts with
`"Usage: agentfs "`. New `tests/cli/cli_name.rs` (`mod cli_name;` in `tests/cli/main.rs`), on the real binary:
`#[test] fn the_binary_names_itself_agentfs()`: `--version` prints exactly `agentfs {CARGO_PKG_VERSION}`, `+failpoints`
when `cfg!(feature = "failpoints")`, `\n`; `--help` contains `"\nUsage: agentfs "`. Clap's usage name is argv[0]'s, so
only the unit test judges the usage line.
**ci/fuse-tests.sh.** `test_binary NAME` reads cargo's JSON on stdin (`jq` select `.executable != null and
.target.name == $name`, `tail -n 1`, `fail` unless `-x`), so each of its 4 calls is a literal command the check reads:
`bin=$(cargo test --locked -p agentfs-cli --test launcher_mount --no-run --message-format=json 2>/dev/null |
test_binary launcher_mount)`; likewise `launcher_host`, `csi_trees`, and `-p agentfs-fuse --lib` for `agentfs_fuse`.
**agentfs-repo.** (1) `cargo_commands_name_what_the_workspace_has` also refuses a non-literal selection
(`!s.literal`) in a file that runs, any scanned file but markdown and Rust ("selects a package through a variable: name
it, so this check can read it"); its doc: after D no package is named `agentfs`. `command_files()` reads all of
`.github/` (the PR template holds cargo commands). (2) Fixtures: `fn binary(package: &str, bin: &str, features:
&[&str]) -> Member`; `LINES` are the lines after D (`-p agentfs-cli`, `agentfs-cli/https,...`); `now` = `agentfs-cli`
(bin `agentfs`, features `https` `ssh` `otlp-tls`, test `csi_trees`) and `agentfs-csi`; `front` = `now` plus
`member("agentfs", "filesystem", &[])`. Every `LINES` entry passes on both; on `now` `cargo build -p agentfs`,
`--features agentfs/https`, `cargo run -p agentfs -- init`, `-p agentfs --bin agentfs`, `-p agentfs --test csi_trees`
are refused; on `front` those with a feature, `--bin`, `--test` or `run` are, and `cargo build -p agentfs` and `cargo test
--doc -p agentfs` pass; `-p "$p"` is refused for `ci/x.sh`, not for `docs/x.md`. (3) `workspace.rs`
`loads_this_workspace`: member `agentfs-cli`, dir `crates/agentfs-cli`, one bin target, `agentfs`. (4) In `crates.rs`:
```rust
/// Each path under `crates/` that `text` names, with its line: `crates/...`, or `<crate>/{src,tests,benches,proto}/...`,
/// `<crate>/Cargo.toml`, `<crate>/build.rs` read under `crates/` (`<crate>` = `agentfs(-[a-z]+)*`). It starts a line or
/// follows whitespace or one of `` ` ( [ " ' < : , = `` (then any `./`, `../`), runs over `[A-Za-z0-9_.*/-]`, and
/// drops trailing `.` and `/`.
fn crate_paths(text: &str) -> Vec<(usize, String)>;
/// `source` with each line that is not a comment (trimmed start `//`) emptied; line numbers kept.
fn rust_comments(source: &str) -> String;
/// Whether a scanned file runs (its cargo commands execute): any but `.md` and `.rs`. The literal rule reads it.
fn runs(path: &std::path::Path) -> bool;
/// Every path under `crates/` that `command_files()` name (Rust: comments only) exists. Asserts > 150 read.
#[test] fn crate_paths_name_what_exists();
#[test] fn a_crate_path_naming_nothing_is_found();
```
Judged by `path_problem` (C's `filter_problem`, renamed) over `repository_paths()`, both moved from `ci.rs` to `main.rs`
as `pub(crate)`, bodies unchanged. Fixture: `crates/agentfs-core/src/lib.rs.`, `` `agentfs-core/tests/it/main.rs` ``,
`(../crates/agentfs/tests/csi_trees.rs)`, `crates/*/tests`, `crates/agentfs-engine/src/{a,b}.rs` (→ `.../src`) are read;
`refs/heads/agentfs/src-1`, `https://crates.io/crates/fuser`, `` `agentfs/src` `` are not; against a path set without
`crates/agentfs/tests/csi_trees.rs` that one alone is a problem. (5) `main.rs`'s module doc names both rules.
**References** → `agentfs-cli`. Cargo selections, 32 lines: ci.yml 4, phase-1.yml 7, alerts.yml 1, ci/check-deps.sh 1,
crash-tests.sh 1, csi-sanity.sh 2, fuse-tests.sh 1, Dockerfile 2 (6 selections), README.md 1, AGENTS.md 1 (2 commands),
development.md :41-43 :60 :115 :165, configuration.md :520 :1136, trees.md:58, `tests/cli/cli_verify.rs:7`,
`tests/crash_daemon.rs:12`. Paths `crates/agentfs/`, `agentfs/tests/`: 42 mentions, 21 files (development.md 9,
fuse-tests.sh 6, Dockerfile 4, one-engine.md 3, crate-split.md 2, configuration.md 2, alerts.yml 2, one each in
deployment.md, csi.md, architecture.md, hermes README.md, AGENTS.md, otel-certs.sh, crash-tests.sh, agentfs-repo
`tests/repo/main.rs`, csi `tests/grpc.rs`, core `src/patch/tests.rs`, `tests/crash_points.rs`,
`tests/support/{closed_port,crash_oracle,queue_patch}.rs`). Prose naming the package (not the binary, command, image or
project): AGENTS.md principle 10 ("`agentfs-fuse` and `agentfs-cli`, whose binary is `agentfs`"), its `docs/library/`
and Layout rows; architecture.md's Crates row, `### \`agentfs-cli\``, the `filesystem` rank's "What it holds";
library/filesystem.md's H1, SUMMARY.md:22, docs/README.md:39; development.md :109 :186 :198 :286; crate-split.md row D
"(built)", status line, :42, Traps (both new rules). CHANGELOG.md "Changed": one bullet. Kept: `CARGO_BIN_EXE_agentfs`,
`target/*/agentfs`, `/usr/local/bin/agentfs`, `--bin agentfs`; `.claude/handoff/`, errata, CHANGELOG's and
OWNER-GUIDANCE.md's dated text.
**Acceptance.** `cargo metadata --no-deps`: no package `agentfs`; `agentfs-cli` at `crates/agentfs-cli`, rank
`filesystem`, one bin target `agentfs`. `git grep` for `-p agentfs` not followed by `-`, `agentfs/(https|ssh|otlp-tls|
failpoints)` and `crates/agentfs/` finds only agentfs-repo's fixtures and crate-split.md's prose on the trap.
`bash ci/check-deps.sh`: six `ok`. Gates, one call each: `cargo fmt --all --check`; `cargo clippy -p agentfs-cli -p
agentfs-repo -p agentfs-core -p agentfs-csi --all-targets -- -D warnings`, and `-p agentfs-cli -p agentfs-core --features
failpoints`; `cargo nextest run` both ways; `-p agentfs-core --test config_reference`, `-p agentfs-cli --bin agentfs
reference_tests plan_reads_only_env_vars`, `-p agentfs-csi --bin agentfs-csi reference_tests`. NOT RUN locally (CI's):
the image, `ci/fuse-tests.sh`, `ci/crash-tests.sh`, docker, hermes.
**Probes.** (1) `-p agentfs` back at fuse-tests.sh:23 → the cargo check ("selects package agentfs, which the workspace
does not have"); `--features agentfs/https` in the Dockerfile → the same. (2) `-p "$package"` back in fuse-tests.sh →
the literal rule. (3) `crates/agentfs/tests/csi_trees.rs` in a fuse-tests.sh comment, or `agentfs/tests/crash_daemon.rs`
in architecture.md → `crate_paths_name_what_exists`. (4) drop `name = "agentfs"` → both name tests. (5) alerts.yml keeps
`crates/agentfs/src/otel.rs` → C's path-filter check; the Dockerfile keeps `COPY crates/agentfs/Cargo.toml` →
`dockerfile_stubs_every_workspace_member`.
**Files.** `crates/agentfs-cli/**` (was `crates/agentfs/**`); `Cargo.toml`, `Cargo.lock`; agentfs-repo
`src/workspace.rs`, `tests/repo/{main,ci,crates}.rs`; the core and csi test files named; `.github/workflows/{ci,phase-1,
alerts}.yml`; `ci/{check-deps,crash-tests,csi-sanity,fuse-tests,otel-certs}.sh`; `Dockerfile`; README.md, AGENTS.md,
CHANGELOG.md; `deploy/examples/hermes/README.md`; the docs named, and any file the greps or the checks name.

## Decisions

**Slice order.** One slice. The rename is one compile unit: the manifest, the lock, the workspace members, the
Dockerfile's stubs and every cargo selection must change together for `cargo_commands_name_what_the_workspace_has` and
`dockerfile_stubs_every_workspace_member` to stay green, and docs travel with the code (AGENTS.md "Git"). The new
checks are small, and the rename is their mutation probe; split out, the path check would be a slice under an hour.

**Design choices.**
1. An explicit `[[bin]] name = "agentfs"` is Cargo's idiom for a binary named apart from its package (The Cargo Book,
   "Cargo Targets"); `CARGO_BIN_EXE_<name>` is keyed by the target, so the 40 test lines and every `target/*/agentfs`
   stay.
2. `#[command(name = "agentfs")]` with two tests: the unit test pins the name, the version line and the usage line
   without argv[0]; the integration test proves `--version` and usage on the real binary (tests through the artifact).
   The runbooks' `agentfs --version` (upgrading-the-sidecar.md) is what it protects.
3. Deviation, stronger than row D: `crate_paths_name_what_exists`. Row D says every reference follows, but only cargo
   selections, workflow path filters, the Dockerfile's stubs and markdown links were checked; a path in prose, a comment
   or a code span was a grep habit. A prototype scan at 5823ee0 read 236 mentions and found none unresolved, so the check
   needs no allow-list. Prior art: C's `workflow_path_filters_name_what_exists` and its path judge, reused; link checkers
   (lychee) read links only. Not covered: a path not under `crates/` and not starting with an agentfs crate's directory
   plus `src/`, `tests/`, `benches/`, `proto/`, `Cargo.toml` or `build.rs`; Rust string literals (the compiler or the
   test that opens them judges those); after E, a bare `crates/agentfs` names the front's directory and passes.
4. Deviation, stronger than row D: a cargo selection through a variable is refused in a file that runs. fuse-tests.sh's
   `test_binary` was the one instance (`git grep` of ci, .github, examples, deploy, Dockerfile): three `agentfs`
   selections no check could read, which only the FUSE job would have caught. Markdown keeps placeholders such as
   `-p <crate>`.
5. Ranks: unchanged. Cargo.toml's `[workspace.metadata.agentfs] ranks` and the "Ranks" table's ranks and uses stay;
   `agentfs-cli` keeps rank `filesystem`; the table's `filesystem` row names `agentfs-cli`, whose binary `agentfs` is the
   composition root. AGENTS.md: principle 10's mapping, the `docs/library/` row, the Layout row and the local gates'
   two `-p agentfs` commands.
6. A CHANGELOG bullet, unlike A to C: README's build command changes for anyone building from source.
7. No skeleton code: no new crate, and a pre-committed rename would leave the gates red until every reference moves.

**For E's architect.** After E a bare `cargo build -p agentfs` meant for the binary is, in form, a right use of the
front (crate-split.md's trap; the literal rule does not help). The front's library and this binary are both named
`agentfs`: check `cargo doc --workspace` for an output collision and set `doc = false` on the bin if it warns.

**New terms.** None: "the CLI package" is crate-split.md's.

**Design review outcome.** Self-checked against the code at 5823ee0 (implementation-workflow: no review of the cut
before code): every count above is a `git grep`; clap 4.6.7's `_render_version` uses the display name or the command
name, and usage takes argv[0]'s file name (read in its source); the path scan was prototyped. Untested until D1
lands: every check, test and probe above.
