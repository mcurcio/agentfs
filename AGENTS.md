# AGENTS.md

Guidance for coding agents working in this repository. Humans should start with [README.md](README.md).

agentfs is a FUSE filesystem. It records every change a workload makes into git history that the workload cannot see. It pushes that history to remotes and applies patches queued by iac. It ships as a sidecar and as a CSI driver.

## Read first

- `docs/design.md` is authoritative. Decisions are in §19.1 and open questions in §19.2. The acceptance tests are in §18.
- `docs/architecture.md` maps modules to responsibilities, and a test enforces it.
- `docs/configuration.md` lists every key, attribute, env var and flag. A test enforces that too.
- `docs/reviews/` holds earlier adversarial reviews: read them to avoid repeating fixed mistakes.
- Read the existing code before changing it. Learn the current state from the repo, not from old conversation transcripts.

## Layout

| Path | What |
|---|---|
| `crates/agentfs-core` | Engine: journal, pending, history, push, credentials, lfs, patch, history_service, telemetry |
| `crates/agentfs-fuse` | FUSE operations, kernel-independent `Ops` plus a thin fuser adapter |
| `crates/agentfs` | CLI binary (`mount`, `run`, `serve-history`, `record`, `push`, `restore`, `revert`, `patch`, ...) |
| `crates/agentfs-csi` | CSI driver |
| `ci/` | End-to-end scripts run by GitHub Actions; `ci/lib.sh` has the shared helpers |
| `deploy/` | Kubernetes manifests for the sidecar and CSI, plus examples (Hermes) |
| `docs/` | Design, operator guide, configuration, deployment, CSI, performance, runbooks |

## Style

- Module doc comments cite design sections ("design §3.3").
- Errors are `thiserror` enums.
- Unit tests live in `#[cfg(test)]` modules; integration tests live under `crates/*/tests`.
- No `unwrap()` outside tests, except for documented invariants.
- Prose in docs: short sentences, concrete, and no claims the tests don't back.

## Local gates (every commit)

```sh
. ~/.devenv   # if the container uses the no-root toolchain (see below)
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`--all-features` needs OpenSSL and libssh2. CI builds it; a minimal container may not be able to.

## What only CI can test

These usually can't run in an agent container:
- FUSE (no `/dev/fuse`)
- docker, kind and helm
- root
- sshd, nginx and dm-log-writes

GitHub Actions runners can, so put such tests in a script under `ci/` and a job in a workflow. Run anything you can locally first.

| Workflow | Runs on | Typical duration | Covers |
|---|---|---|---|
| `ci.yml` | every push and PR | ~5 min | lint, docs checks, unit/integration tests, FUSE mounts, crash injection, remotes (http/https/ssh), LFS, telemetry, multipod, patches, runbooks, csi-sanity |
| `image.yml` | every push and PR | ~5–9 min | container image, kind sidecar e2e, CSI on kind; publishes to GHCR on main |
| `power-loss.yml` | pushes to main (not docs-only) | ~7 min | dm-log-writes replay on XFS and ext4 |
| `bench.yml` | pushes to main (not docs-only) | **~23–25 min** | benchmarks on XFS and ext4 against `ci/bench-budget.json` |

Rules:
- Pin third-party actions by full commit SHA (`gh api repos/OWNER/REPO/commits/TAG --jq .sha`).
- Pin runners to `ubuntu-24.04`, never the floating `-latest` label. A test (`workflows_pin_runners`) enforces this.
- A change is done only when **every** workflow run for your final pushed SHA is green. Wait for all of them, bench included: `gh run list -R mcurcio/agentfs --commit <sha>`, then `gh run watch <id> -R mcurcio/agentfs --exit-status`. Don't report success while a run is still in progress.
- Read failures with `gh run view <id> --log-failed`. Fix flaky jobs; a rerun alone is not a fix.

Known flakes to watch:
- the `e2e (operator runbooks)` history-rewrite step;
- `csi_service.rs::the_supervisor_runs_and_recovers_a_real_history_service` under heavy local load;
- one FUSE `e2e-recovery` failure, seen once.

## Git

- **Identity:** `bot-einstein <bot+github@mcurcio.com>`. Don't change it.
- **Commit messages:** an imperative subject of at most 72 characters and a short body, ending with the attribution trailer your harness specifies (currently `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`).
- **Pushing:** work goes straight to `main`, which is the owner's chosen practice for this repo. Run `git pull --ff-only origin main` before pushing. Never force-push.
- **Docs travel with the code:**
  - When behaviour changes, update `docs/design.md` in the same commit, and record any decision in §19.
  - Update `README.md` for anything users see.
  - Update `docs/configuration.md` for every new key or flag.
  - Add or update runbooks for new failure modes.
- **Reports:** list what is unfinished, untested or tested only locally. Don't round "mostly works" up to "done".

## The no-root toolchain

Some agent containers have no root and no C compiler. Earlier sessions used:
- **Rust:** stable via rustup.
- **C compiler:** Zig as the C compiler, through wrappers in `~/.local/bin`: `cc`, `c++`, `ar`, `ranlib`, and `gcc` as a symlink to `cc`. `cc` drops `--target` flags and runs `zig cc -target x86_64-linux-gnu.2.28 -fno-sanitize=undefined`, because libgit2's sha1dc trips UBSan.
- **gh:** the static binary.
- **Debian packages without root:**

  ```sh
  O="-o Dir::State::Lists=/tmp/apt/lists -o Dir::Cache=/tmp/apt/cache -o Debug::NoLocking=1"
  mkdir -p /tmp/apt/lists/partial /tmp/apt/cache/archives/partial
  apt-get $O update && apt-get $O download zip && dpkg-deb -x zip_*.deb /tmp/apt/root
  ```

Install real tools this way rather than writing substitutes.

## Large changes

For milestone-sized work, use the orchestration method in [.claude/skills/milestone-orchestration/SKILL.md](.claude/skills/milestone-orchestration/SKILL.md). Its steps are architect, implementer, adversarial reviewer, one bounded fix round, CI as the referee, and a completeness critic.
