# The landing model, in plain words

This crate runs the owner's landing rules on the real `git` program. Main and each sandbox (a "fork") are
git branches. Every save is a commit that names its one writer. Tests in `tests/` check the rules after
every step.

The owner's words this model rests on:

- 2026-11-02: "I generally like the metaphor of a typical Linux filesystem. I want to try that out." On
  transactions: "Frozen snapshot was my intention, yes. That's why I said “new branch” earlier."
- 2026-11-02, agreeing to the parent rule below (every collision between a fork and its parent is decided in
  one place, the parent's landing queue; the fork adopts that decision): "ok, that tracks".
- 2026-11-03, on who wins there: "i think i would assume main wins. because if sandbox forked from main and
  then main moved forward, the sandbox is working from an out-of-date copy. so even though its history can
  be preserved for posterity, i think the updated main should win in a conflict." This replaces his answer
  of 2026-11-02 that the incoming change wins.
- 2026-11-03, on squashing: "the sandbox commits could be squashed to one changeset (if they are the naive
  simple commits) and merged into upstream as one unit. if its a smarter integration that is creating
  transactions, it might be best to leave the transaction commits as they are"; and "every sandbox commit
  stays stored ... that makes sense, because thats basically the design we want".
- 2026-11-03, on losing changes: "yes, \"losing\" patches should still be part of the history. i want to know
  who changed what when".
- 2026-11-03, on a save against a sync: "if the program saves after the sync has landed, then arguably the
  sandbox was the last writer and its change should take effect. wouldnt the upstream then detect the shared
  lineage and allow the sandbox to \"win\"?"
- Still binding from earlier: forks stay, kept in step by syncing quickly; sandboxes are peers; one history
  setting per volume; a failed transaction is an implicit rollback.

## The rules

"Parent" means main for a sandbox, and the sandbox for a sandbox inside it. A "sync" brings the parent's
version into a sandbox; a "landing" brings a sandbox's saved change into its parent. Two lines "collide"
when each was written without seeing the other. Rows marked *choice* were decided here, not by the owner;
each is open for him to change.

| # | Rule | Source | Tests |
|---|---|---|---|
| 1 | Works like a Linux filesystem. A write, sync, landing or transaction commit is never blocked, refused or held, and each writer sees its own write at once. Changes flow both ways without anyone asking. *Choice* (read from the owner's 2026-11-03 question, not his words): a save's first parent is its tree's head when it is made, so whatever the tree held then counts as seen. | Owner; first parent at save time is a choice | `r1_writes_never_wait_and_forks_sync_both_ways` |
| 2 | Order without clocks. Each tree has one queue, taken in order. Which of two lines is newer is read from git, never from a clock. | Owner | `r2_one_queue_fixes_the_landing_order`, `r2_no_clock_commit_dates_decide_nothing` |
| 3a | Every collision between a sandbox and its parent is decided in exactly one place, the parent's landing queue, and recorded once, there. A sync never decides one: the sandbox keeps its own line until the parent's queue has decided it, then takes the parent's decision. So a sandbox's line changes at most once per collision: never its own, then the parent's, then its own again. | Owner | `r3_a_collision_is_recorded_once_at_the_parents_queue`, `r3_a_fork_never_flips_back_to_its_own_edit` |
| 3b | There, the parent's version wins a collision. The sandbox's losing line stays in history (kept under `refs/collisions/<n>`, which Replication pushes to the shared remote, so its writer and its place stay readable there), the collision names both lines, and the sandbox then takes the parent's line. A save made before the parent's sync arrived is concurrent with it and loses. At every level: a sandbox is the parent of the sandboxes inside it. | Owner | `r3_landing_collision_the_parent_wins`, `r3_a_save_before_the_sync_arrives_loses_to_the_parent`, `r4_nested_forks_follow_the_same_rules` |
| 3c | A line written on top of the parent's current line (its writer saw it) is not a collision; it replaces it. So a save made after a sync brought the parent's line wins upstream by lineage. *Choice:* "saw" is read per line: the lines a line was written over, from git (first parent and `git blame`), not which commits its version merged. | Owner; per line is a choice | `r3_fast_forward_when_the_base_is_current`, `r3_a_save_after_the_sync_wins_upstream_by_lineage`, `r3_an_edit_over_the_decided_line_replaces_it`, `r3_an_edit_over_an_undecided_line_still_collides` |
| 3d | A sync carries the parent's current decided version, never a stale one. *Choice:* the sandbox keeps exactly its lines whose writing commit the parent's history does not reach yet; when none is left and nothing of it waits in the parent's queue, the sandbox's branch moves to the parent's commit (a fast-forward). | Owner; how is a choice | `r3_a_sync_carries_the_parents_current_version`, `r3_collision_scope_per_line_sync_keeps_the_clean_line`, `r7_squashed_sync_never_undoes_the_forks_newer_line` |
| 3e | *Choice:* only the colliding lines lose (per line); the change's other lines apply, neighbouring lines included. | Recommended | `r3_collision_scope_per_line_keeps_clean_lines`, `r3_neighbouring_lines_merge_per_line` |
| 4 | Nothing waits behind a collision: every sandbox commit lands, in order. | Owner | `r4_nothing_waits_behind_a_collision` |
| 5 | Transactions, opt-in. Begin opens a new branch frozen at the sandbox's version; the program sees its own edits while the sandbox keeps syncing. Rollback, abandon or the client dying drops the branch. *Choice:* commit lands the branch in the sandbox as a sandbox lands in its parent: the sandbox's line wins a collision (one a sync brought in since begin), the other lines apply, the commit is never refused, and its answer names each overwritten line. The commit then lands upstream like any change. | Owner; commit is a choice | `r5_transaction_commits_clean`, `r5_transaction_commit_reports_the_overwritten_line`, `r5_sync_before_or_after_the_commit_ends_the_same`, `r5_a_committed_transaction_then_lands_like_any_change`, `r5_transaction_rollback_and_client_death_leave_nothing` |
| 6 | *Choice:* every change traces back to one writer. A sync commit (the merge a sync makes when the sandbox has commits of its own the parent has not taken, so it cannot fast-forward) is labelled `Sync`, credited to `agentfs`, with the parent's commit second; it has no edits of its own: a merge carries only lines a parent holds. | Choice | `r6_a_sync_is_a_merge_with_the_parent_second`, every test |
| 7 | One history setting per volume. Full lineage: every sandbox commit lands on its own and stays in main's history. Squashed: main's history holds only main's own commits; a sandbox's plain saves waiting in the parent's queue when it takes the first land as one changeset; each transaction commit lands as its own unit. Every sandbox commit stays stored. *Choice:* the last commit a landing carried is recorded beside history as a git graft (`refs/replace/`); a nested sandbox's landing also lands on its own (it carries another writer's lines); a squash takes the sandbox's later saves ahead of other sandboxes' entries between them; a landing whose lines all lost still writes an (empty) commit, so the parent's history shows the decision. | Owner; the graft and the rest are choices | `r7_squashed_saves_land_as_one_changeset`, `r7_history_setting_and_losers_stay_reachable` |

## What must always hold

Checked after every step, in every scenario and random run (`check.rs`), under both history settings.

| Invariant | Check |
|---|---|
| No tree silently loses a change: every line that reached a tree is still there, or replaced by a line written over it, or named lost by a collision in that tree or one upstream. | `no_silent_loss` |
| No stale line reverts a newer one. | `no_silent_loss` |
| No flip-flop: a line, once replaced in a tree, never shows there again. | `no_flicker` |
| The parent's version wins every collision, decided only at the parent's queue: read from git, the result is that tree's own landing (or transaction) commit and the losing commit was made in a sandbox below it; the tree keeps its own line, the two lines really collided, the losing commit stays reachable. | `parent_wins` |
| A sync records no collision, and carries the parent's head as it was at that moment (read before the step, not from the model's record). | `step_checked` |
| Every sync delivers the parent's decided version: the sandbox holds the parent's line on every slot except its own lines the parent's queue has not taken yet. | `syncs_deliver` |
| Nothing is held: every save and commit returns at once; every queue step takes its queue's first entry (with the saves a squash carries) and leaves the rest in order. | `interleavings.rs` `take` |
| A transaction ends as the program was told: committed lines are in the sandbox, each overwritten one is named in the answer and by a collision; rolled-back ones leave nothing in any tree or history. | `transactions_end_as_told`, `discarded_leave_no_trace` |
| Every change traces back to one writer. | `one_writer` |
| Every losing change is replicated, eventually, and stays: once Replication's push (`Step::Replicate`, its own step, never a referee's) has drained, every collision of the run is read on the shared remote alone: its collision ref names the losing commit, each lost line is in it and blames to a commit naming that line's writer, and a collision decided in main has its result in the remote main's history. | `losers_pushed` (in `check_converged`) |
| Trees converge when writes stop: every sandbox equals main. | `check_converged` |
| The history setting holds, read by ancestry alone; under Squashed, across the CI seeds, runs of saves really land as one changeset. | `history_setting`, `interleavings.rs` `run_all` |
| No clock is read: the same calls with every commit dated before its parents decide the same trees. | `r2_no_clock_commit_dates_decide_nothing`, `random_interleavings_commit_dates_decide_nothing` |

The checks' record of who saw what is kept apart from git: a line saw the line its writer read on that
slot, and everything that line saw. The random runs replay seeds 1 to 64 in CI (full lineage, squashed, and
every slot on a neighbouring line). Half the time a write goes to the line a sandbox wrote last, so runs
often build "a sandbox edits over its own undecided line while main writes it" (finding 8). Run more with `AGENTFS_MODEL_SEEDS=500`, or one with
`AGENTFS_MODEL_SEED=<n>`.

## Coherence findings

An open finding has a test marked `#[ignore = "coherence finding N"]`; run them with `--ignored`.

1. **git's own merge cannot tell decided from undecided (open, for git's merge).** A sync built by git's
   three-way merge, keeping the sandbox's side of a conflict: once a sandbox and main have each merged the
   other, git finds two merge bases and keeps a sandbox line main already decided against.
   `finding_1_git_merge_base_on_crossing_merges`, `random_interleavings_git_merge_base` (36 of 64 seeds).
2. **Resolved: Squashed hid what a sync needs.** Without the graft (rule 7), a sync cannot tell that main's
   queue has taken a sandbox's line, so the sandbox keeps it forever. Costs: a clone needs `refs/replace/`
   too; the system design's squash shape (`domains/history.md`) is a merge, not the model's single-parent
   commit, though the same graft serves it. `random_interleavings_squashed`.
3. **Per file reverts newer lines (open, for per file).** Main's whole file wins, so the sandbox's clean
   line in it is dropped; the sandbox then takes main's older line over its newer one.
   `finding_3_per_file_keeps_clean_lines` (every time); `random_interleavings_per_file_full_lineage` only rarely
   builds it (0 of 64 seeds, 3 of 500).
4. **Resolved: the flicker.** Under "the incoming change wins" a sandbox showed x, then main's m, then x.
   The parent rule removes it, and `no_flicker` asserts it in every run.
5. **git reads commit dates when it walks a range.** `git rev-list A --not B` stops early by date, so the
   model reads ancestry only (`merge-base --is-ancestor`, `blame`, whole-history `rev-list`). An
   implementation must do the same.
6. **git's hunk is not a line (open, for git's merge).** Neighbouring changed lines form one git hunk;
   keeping the sandbox's side of it withholds main's decided neighbouring line.
   `finding_6_git_hunks_on_neighbouring_lines`.
7. **The merge an implementation needs is not a git merge strategy.** Findings 1 and 6: so the rule lives in
   the model's own code (`World::resolve`, `World::adopt`), using git only to store commits and answer "who
   wrote this line" and "does this history reach that commit". An implementation needs the same.
8. **Resolved: "saw" must be read per line, not per commit.** A sync that keeps a sandbox's undecided line x
   still makes main's commit (with m) a parent of the sandbox's next commit. Read by commit ancestry, the
   sandbox's next edit y over x looks "made on m", so y replaces main's winning m with no collision: a
   silent loss. The model reads the lines y was written over (x, and what x was written over), so y
   collides with m and main wins. The checks' "saw" is per line too; read per commit they could not see
   this loss. `r3_an_edit_over_an_undecided_line_still_collides`.

## Costs and open questions for the owner

- A transaction can end split: a line lost to main, its other lines applied (per line, main wins). The
  program is told at its commit only of lines its own sandbox overwrote; a later loss at main's queue is a
  recorded collision, not part of the commit's answer.
- Until main's queue decides, a sandbox shows its own line, not main's concurrent one, and keeps building
  on it. Once a sync has brought in main's concurrent line (kept out of view), the outcome is already fixed:
  the sandbox's line will lose, and so will every edit it stacks on that line before main's queue decides
  (each was made on the out-of-date copy). They stay in history and a collision names them (under Squashed,
  one collision names the last), but none reaches main.
  `r3_an_edit_over_an_undecided_line_still_collides`. This holds even for an edit made after the sync: by
  commit ancestry it descends from main's line (the sync commit's second parent), so the owner's "shared
  lineage" would let it win; per line (rule 3c's choice) it loses. Open for the owner (git-model.md, owner
  question 3).
- Under Squashed, main's history gains an empty commit for a landing whose lines all lost.
- Losing commits and their collision refs are pushed and kept forever: any trim rule must keep who changed
  what when. The remote lags: a collision ref reaches it at Replication's next push.

## Left out

- **Deletes and renames.** Lines are only ever overwritten; a delete against an edit has no rule yet.
- **Inserted, removed, moved and repeated lines.** A file is a fixed number of lines, each write a new,
  unique line, so `git blame` finds its writer exactly.
- **Writers racing inside one tree.** Plain Linux behaviour, not modelled.
- **Open files.** A program here reads and writes in one call. A real one may read, wait, and write back. *Choice*
  (system design, not modelled): its save is its whole written file on the head at save time, as on Linux,
  where a write replaces what it wrote over. Cost: a program that read an old copy and writes it back
  overwrites a line synced in since. Its first parent is the head, so the referee reads the old line as
  written over the synced one: it replaces it in the parent and, by sync, in every sibling, with no collision,
  credited to the sandbox's writer. Unlike a Linux lost update it spreads; both lines stay in history. It
  breaks the model's "each write a new line" assumption, and `no_silent_loss` and `no_flicker` would report
  it. Alternative: commit only the lines the program changed since it opened the file (needs a line
  identity). Open for the owner (git-model.md, owner question 2).
- Binary files; approvals and when landings run; integrator patches; restarts; space limits; history
  truncation.
