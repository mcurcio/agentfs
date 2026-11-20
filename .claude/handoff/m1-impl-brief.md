# Brief: build one-engine M1 on PR #39 (2026-11-13)

Repo: /home/user/wt-step6, branch `trees/module-cut-step-6`, PR #39 (the owner's ONE PR; never open another, never push to main, never force-push). Start every agent with `git pull --ff-only origin trees/module-cut-step-6` and `git status` (must be clean). Read AGENTS.md first.

## The design (approved)
- `/tmp/claude-0/-home-user-agentfs/41ac0cfe-83c8-57b0-9f09-59a6e4c29f38/scratchpad/next/m1-design.md`, rev. 2, INCLUDING its last section "Gate outcome (2026-11-13)", which overrides S3's "Repositories and rows as Q1's answer sets them": volume events (clients attached/detached, forks created/ended, lifecycle moves) are commits in the volume's own repository, on refs beside the trees they concern. No database, no ORM. The journal stays the one commit point; each event's commit is written from it before the call that caused it returns.
- Plan: docs/target-architecture/future/one-engine.md (M1). Domains: docs/target-architecture/system.md and domains/.
- Slice pages (written by the code architect, step 0): `/tmp/claude-0/-home-user-agentfs/41ac0cfe-83c8-57b0-9f09-59a6e4c29f38/scratchpad/next/m1-pages.md`. An implementer reads its own slice's page plus the design section it cites.

## Owner answers that bind (verbatim, 2026-11-13; record in OWNER-GUIDANCE.md in step 0's commit, dated, in his words)
- "i only want one PR at the end"
- "Keep going" (on #39, until the baseline: Hermes sidecar CI on tree views, after M1…M7)
- "a transaction should be recorded as one commit"; "Ok" to: a landing merge when the fork moved, a crash rolls back an open transaction, an empty transaction makes no commit.
- Rust pin: "sure. just do it in this PR."
- Volume events: "On record is ideal. And hopefully, attached close to the git data for which it relates."
- The October repository rule "was about how database interfaces should be structured in a different project": it does not apply here.
- Earlier: "dont say sandbox" (say fork); break compatibility freely before 1.0.
Source of the exact words: `/tmp/claude-0/-home-user-agentfs/41ac0cfe-83c8-57b0-9f09-59a6e4c29f38/scratchpad/cut/owner-notes.md`. Skip any already in OWNER-GUIDANCE.md.

## Rails (every agent)
- Disk is tight (~8 GB free). Check `df -h /` before any build. Build and test through `ci/model-docker.sh` only, one container at a time (docker volume agentfs-model-target holds the target). Never create new docker volumes or a second target dir. If free space drops under 5 GB, `cargo clean` inside the container's target first; never delete another worktree.
- Run long commands in the FOREGROUND in one Bash call with a long timeout. Never background a command or arm a monitor.
- Gates per slice: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, the changed crates' tests (`--no-fail-fast`). Known local-only failures: `creds::tests::as_root_the_caller_owns_what_it_creates` (root in container), the tree_git `missing_objects` test (host git 2.43). Then commit, `git pull --ff-only`, push, and watch `ci` and `image` for the pushed SHA with `gh run watch <id> -R mcurcio/agentfs --exit-status` (on HTTP 502 poll `gh run view`). Slices marked "full" also dispatch `phase-1.yml` on the branch (`gh workflow run phase-1.yml -R mcurcio/agentfs --ref trees/module-cut-step-6`) and watch it to the end.
- Model gate: if a slice changes anything the model (crates/agentfs-model) or the conformance replay checks, run it per the `model-runs` skill through `ci/model-docker.sh`. A changed digest or matrix row is a finding to explain in the report. Never re-record `planted/budget.json`.
- Docs travel with the code (AGENTS.md "Docs travel with the code"): configuration.md for every new flag, library/human docs for behaviour, CHANGELOG Unreleased for user-visible changes.
- Commits: Conventional Commits, subject at most 72 chars, ending with exactly:
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01MtEdSUVjnqPuCUVqq1Gfg4
  No model IDs anywhere in commits, code or docs. Never bypass hooks except `--no-verify` when the hook only re-runs what you ran.
- Spec contradiction: append to `.claude/errata.md` and STOP. Red for a code reason after two fix attempts: push the work to `wip/m1-<slice>` (private-index recipe, implementation-workflow §7) and STOP with findings. Report starts with the word STOP in that case.
- Implement only your slice even if others look ready. Report: commits (sha), checks with exit codes, CI run ids and results, net line delta, reuse considered, errata, NOT RUN.
