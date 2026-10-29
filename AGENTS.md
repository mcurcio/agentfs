# AGENTS.md

Guidance for coding agents working in this repository. Humans should start with [README.md](README.md).

agentfs is a FUSE filesystem. It records every change a workload makes into git history that the workload cannot see. It pushes that history to remotes and applies patches queued by iac. It ships as a sidecar and as a CSI driver.

## What agentfs is for

- **Origin.** The owner (mcurcio) runs AI agents on a Kubernetes cluster managed by [mcurcio/iac](https://github.com/mcurcio/iac). The first agent is Hermes (`workloads/hermes` there). Two problems needed solving:
  - **Configuration.** The agent must edit its own and its sub-agents' configuration (prompts, profiles, skills) at runtime, yet iac also owns that configuration. Overwriting whole files from iac would destroy the agent's changes.
  - **Audit.** Every change the agent makes should be on record, and that record should be out of the agent's sight and reach.
- **The earlier attempt.** An earlier attempt, ArtifactFS (iac PR 197), stalled on mounting one FUSE volume that both root and non-root processes could use. agentfs was redesigned from first principles. Its design was reviewed adversarially (`docs/reviews/`) before any code was written.
- **The goal is general.** agentfs is meant as a general tool for the cluster: any workload whose directory should carry an audit trail and accept versioned patches. Hermes is the first consumer, used to validate the design (design §16), not the design target. Don't specialise the code for it.

## Principles

These are the owner's priorities. When a choice is unclear, decide by them. Each links to where the design applies it.

1. **Recording is the baseline; everything else is an extra.** A change that reached the filesystem must be recorded before the call returns, or fail loudly (§1.2, §3). Never trade recording for a feature, a speedup or convenience.
2. **Invisible and untouchable by the workload.** No history, journal, state or credential is reachable through the mount, and nothing in the tree can turn recording off (§1.1, §4). The threat is an agent's mistakes, not malice, so don't over-engineer against an adversary.
3. **Git idioms, not invented configuration.** Use `.gitignore`, `.gitattributes`, `.lfsconfig`, `$GIT_DIR/info/*` and git-config syntax. Operator policy lives in the git directory, which iac sets. Rule files in the tree only classify untracked paths (§4).
4. **The right technology for the job.** The owner, 2026-10-28: "i never said "no native libraries". lets choose the right technology for the job". Today agentfs uses libgit2 through git2-rs and its own LFS, HTTP and SSH clients, and runs the `git` program where git does the job better, for example `git repack` (§17.3). Record each choice and its reason in §17.3.
5. **Keep operating.** A failed push, an unreachable remote or a divergence raises an alarm and is retried. The filesystem keeps working (§11.3). Recording failures are the only failures that surface to the workload.
6. **Any remote, or none.** GitHub is one option, not an assumption (§11.1). Several volumes may share one repository (§11.4).
7. **History is the product.** Never rewrite history or force-push automatically. A divergence is saved and an alarm is raised, never overwritten (§11.3). History rewrites are manual operator procedures (§15).
8. **Recovery is manual but supported.** Recovery uses `restore`, `revert` and the runbooks. Further undo tooling is added as real use shows the need (§15).
9. **Merging is an extra.** The core records history. The patch queue applies iac's changes only at mount: in order, stopping at the first failure, with the rest left queued (§12).
10. **Three layers, each an integration point.** The owner, 2026-10-29: "There is “the library” of agentfs, and “the filesystem” agentfs, and the kubernetes adapter layer — and each of these provides a unique integration point for consumers to tap into". The library is `agentfs-core`, the filesystem is `agentfs-fuse` and the `agentfs` binary, and the adapter is `agentfs-csi`. Put each primitive in the lowest layer that can own it, and give it a good API and docs there (design §19.1, "Three layers"). All are Rust (§17). OpenTelemetry is used for everything observable (§13).

## Working with the owner

- **Decisions.** The owner's decisions are recorded, dated, in design §19.1. Don't reopen them without new evidence. Record new ones there in the same commit.
- **Ask about what is genuinely the owner's call**: policy, security trade-offs, cost. Examples are relaxing iac's `docker_volumes` pin for Hermes, or choosing a licence. Decide the rest with the principles above, and say what you decided.
- **Large multi-agent runs need an explicit request.** Workflows and fan-outs are expensive. Describe the scale first.
- **Reports must be honest.** Say what is untested, tested only locally, or cut, with the reason.

## How these notes are organised

Every fact has one home, and other files link to it rather than copying it.

| Home | Holds |
|---|---|
| `README.md` | What users need: pitch, install, quick start |
| `docs/design.md` | The authoritative what and why: behaviour, guarantees, decisions (§19.1, dated) and open questions (§19.2) |
| `docs/architecture.md`, `docs/configuration.md` | Code map and reference. Tests check them against the code |
| `docs/runbooks/` | Operator procedures for each failure mode |
| `docs/reviews/` | Adversarial reviews and what changed because of them |
| `AGENTS.md` (this file) | Orientation for agents: purpose, principles, norms, environment, and links to the homes above. Keep it short: it is loaded into every session |
| `.claude/skills/` | Procedures agents run, such as milestone orchestration |

When you learn something durable, write it to its home in the same commit. Never leave it only in a conversation or a PR description.

## Read first

- `docs/design.md` is authoritative. Decisions are in §19.1 and open questions in §19.2. The acceptance tests are in §18.
- `docs/architecture.md` maps modules to responsibilities, and a test enforces it.
- `docs/configuration.md` lists every key, attribute, env var and flag. A test enforces that too.
- `docs/supported-configurations.md` lists the supported configurations: check a design against every example it touches.
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
| `examples/docker` | Docker examples: `docker run` and compose (docs/docker.md) |
| `docs/` | Design, operator guide, configuration, deployment, CSI, performance, runbooks; `docs/README.md` is the index and `docs/SUMMARY.md` the book's table of contents (a new page goes in both) |

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
| `ci.yml` | every push and PR | ~5 min | lint, docs checks, unit/integration tests, FUSE mounts, crash injection, remotes (http/https/ssh), LFS, telemetry, multipod, NFS (CLI state areas on one export with two clients; agentfs-csi refusing an NFS base path), patches, runbooks, csi-sanity |
| `image.yml` | every push and PR | ~5–9 min | container image, kind sidecar e2e, CSI on one kind node (no multi-node job); publishes to GHCR on main |
| `alerts.yml` | pushes and PRs touching `deploy/alerts`, its `ci/` scripts or the telemetry code | ~4 min | promtool checks and unit tests of the example alert rules, the generated `PrometheusRule`, the rules against the binary's metrics in a real Prometheus |
| `docker.yml` | pushes and PRs touching the image or `examples/docker` | ~5–8 min | the Docker examples (`docker run`, compose, compose with a git server remote) via `ci/e2e-docker.sh`, one job each |
| `helm.yml` | every push and PR | ~3–4 min | Helm charts: helm lint, ct lint, READMEs current with helm-docs, charts equal to `deploy/`; `ct install` of the CSI chart on kind and `ci/e2e-csi.sh` against it; publishes charts to `oci://ghcr.io/mcurcio/charts` on main and `v*` tags (docs/helm.md) |
| `docs.yml` | every push and PR | ~2–3 min | logo PNGs in `docs/assets/` match fresh renders of their SVGs (`ci/render-assets.sh`); the mdBook of `docs/` (`book.toml`, `docs/SUMMARY.md`; an mdbook warning fails it), deployed to GitHub Pages from main; the README's Docker quick start as written, against the published image |
| `f8.yml` | every push and PR | ~5–6 min | F8 end to end on kind, each job on its own runner: a volume moved between two workers by the moving runbook, with the example git server as a pod (`ci/e2e-csi-move.sh`); the chart's own git server, `server.enabled` (`ci/e2e-chart-server.sh`) |
| `power-loss.yml` | pushes to main (not docs-only), after landing | ~7 min | dm-log-writes replay on XFS and ext4 |
| `bench.yml` | pushes to main (not docs-only), after landing | ~15–18 min | benchmarks on XFS and ext4, each in two parallel shards, against `ci/bench-budget.json` |

Rules:
- Pin third-party actions by full commit SHA (`gh api repos/OWNER/REPO/commits/TAG --jq .sha`).
- Pin runners to `ubuntu-24.04`, never the floating `-latest` label. A test (`workflows_pin_runners`) enforces this.
- A change is done when the **ci** and **image** runs for your final pushed SHA are green. Wait for both: `gh run list -R mcurcio/agentfs --commit <sha>`, then `gh run watch <id> -R mcurcio/agentfs --exit-status`. Don't report success while either is still in progress.
- **bench** and **power-loss** run after the change lands on main and are checked afterwards (design §19.1, 2026-10-15). A bench budget warning or a power-loss failure is a follow-up to fix, not a reason to hold the change; report it.
- Read failures with `gh run view <id> --log-failed`. A test that fails only sometimes is a broken test: find the mechanism, then fix the test or the product bug it exposes. Never rerun it as the fix, and never keep a list of tolerated flakes. The owner, 2026-10-18: "“Known flake” is not an acceptable category. Broken tests must be fixed".

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

## Work items (beads)

Open work lives in [beads](https://github.com/steveyegge/beads) (`bd`), configured in `.beads/`. Its database is not in git: it syncs to GitHub under `refs/dolt/data`, and every change pushes automatically (`dolt.auto-push` in `.beads/config.yaml`).

- **Fresh clone or new machine:** run `bd bootstrap`. It clones the issues from `refs/dolt/data` and never deletes existing ones.
- **Pick work:** `bd ready` lists unblocked items; `bd show <id>` gives the brief.
- **Claim:** `bd update <id> --claim`. **Finish:** `bd close <id>` once the work has landed on main with CI green.
- Each bead names its files and done condition. A bead that needs the owner's answer (a design gate, deleting branches) says so: ask before acting.
- The repo runs bd's git hooks (`core.hooksPath` = `.beads/hooks`); never bypass them.
