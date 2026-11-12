# Brief: module cut step 6 — implement (model)

You are a Rust expert in executable specification models, transactions and replication. Implement ONLY step 6.
Checkout: /home/user/wt-step6 (git worktree, branch trees/module-cut-step-6 from main 22d55f9). Work only there. Git identity comes
from the environment (bot-einstein); don't change git config. Commits: Conventional Commits, <=72-char subject, body, trailers:
Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MtEdSUVjnqPuCUVqq1Gfg4
Push with `git push -u origin trees/module-cut-step-6`. Open no PR.

## The spec
The design: /tmp/claude-0/-home-user-agentfs/41ac0cfe-83c8-57b0-9f09-59a6e4c29f38/scratchpad/next/step6-design.md (rev. 3, reviewed twice;
also on branch wip/module-cut-designs, .claude/designs/step6-design.md). Report spec errors in .claude/errata.md rather than diverging.
Frame: docs/target-architecture/future/module-cut.md row 6, transactions.md, MODEL.md, rules-ledger.md, git-model.md, VERIFYING.md.

## The owner's answers (2026-11-12; guidance, not rules — record verbatim and dated in OWNER-GUIDANCE.md, and apply to the ledger)
- Q1 (R9.18): "Does a file open inside a transaction hold back every take into its fork, or only syncs?" — picked "every take"
  (option worded by the agent: "A file open inside a transaction holds back every take into its fork until it closes").
- Q2 (R10.11): asked "a transaction commit is a landing of the transaction into its fork, under every landing rule (R10.8 included),
  for every file, open or not — agreed?", answered "A". His model of a transaction, in his words: "when a transaction is open, the
  "save" operation isnt creating git commits; instead the changes are being accumulated and only get committed when the transaction
  itself closes/commits". So R10.11 is no longer "the agent's reading": retag it with his answer; the model's landing gate
  (`if dir == Direction::Landing` in `joins`, referee/placing.rs) goes.
- Terminology he learned and accepted (2026-11-12): save, landing (fork -> upstream), sync (upstream -> fork), transaction commit
  (one git commit of everything staged, which then lands into the fork). Use these words in docs you touch.
Source of these words: /tmp/claude-0/-home-user-agentfs/41ac0cfe-83c8-57b0-9f09-59a6e4c29f38/scratchpad/cut/owner-notes.md
(only the step-6 and terminology entries belong in this PR; the others land with the plan PR).

## Verification
Full model gate (step 6 changes rule logic), through ci/model-docker.sh, one container at a time (`docker ps` first; dockerd:
`nohup dockerd >/tmp/dockerd.log 2>&1 &` if down). Scripts: /tmp/claude-0/-home-user-agentfs/41ac0cfe-83c8-57b0-9f09-59a6e4c29f38/scratchpad/cut/gate.sh /home/user/wt-step6 <tag> (full) and
/tmp/claude-0/-home-user-agentfs/41ac0cfe-83c8-57b0-9f09-59a6e4c29f38/scratchpad/cut/gate-light.sh (light), tags s6, s6-2 ... Run long jobs with Bash run_in_background and wait for completion; never leave a job
unwatched. Check `df -h /` before builds (stop and report under 8 GB).
- S1 (service split) must be digest-identical: `check --all` all 2620 runs match, matrix reproduced, no STALE.
- The rule slices re-baseline ON PURPOSE (row 6): `ci/model-digest.sh record` and `run.py --write`, with the design's check that every
  changed run first differs at a transaction's commit (show it), every planted bug still caught, finding 30's test no longer ignored.
  This machine is ~3x slower than planted/budget.json's: after any record/--write, RESTORE planted/budget.json to its committed
  values (write back its committed content with `git show HEAD:crates/agentfs-model/planted/budget.json > crates/agentfs-model/planted/budget.json`; it is a generated timing file you just overwrote) and say so in the commit body.
- Also: model, conformance and safety-net tests, cargo fmt --all --check, cargo clippy --workspace --all-targets -- -D warnings.
- On this machine `check`/corpus exit 1 on their time budget only (baseline digest 1964 s, corpus 514 s); a budget-only line is fine.
- No phase-1 dispatch is needed unless conformance code changes; say if it does.

## Docs travel with code
MODEL.md, VERIFYING.md, rules-ledger.md (R9.18, R10.5, R10.11 and anything else the answers change), git-model.md (baseline: keep
numbering), transactions.md (delete it if this step builds its slice entirely, per AGENTS.md "Design docs"; otherwise update it),
module-cut.md row 6 (Built), and the design-rule-3 table as the design lists.

## Return (<= 50 lines)
Tip SHA; commits; net lines; each gate part per slice with rc and seconds (vs budget 643/224 and baseline 1964/514); the re-baseline
evidence (count of changed runs, proof each first differs at a transaction's commit, matrix diff); docs changed; errata; NOT RUN.

## Errata from the parallel design round (2026-11-12, after this brief was written) — the fix round applies these
1. Drop the `HEAD` refusal in `TreeNames::new` and its row-6 text "as the engine's RootName does": step 5b's redesign removes the
   engine's `RootName` (trees get identities; the root's branch name becomes configuration at the edge), so the check would be
   built only to be deleted. Leave `TreeNames` as main has it; a later named slice changes it.
2. `transactions.md` STAYS (update it, don't delete it): the engine runs no transactions, so its slice is not built by step 6.
