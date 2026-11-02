# The landing model, in plain words

This crate runs the owner's landing rules of 2026-11-02 on the real `git` program. Main and each sandbox
(a "fork") are git branches. Every save is a commit that names its one writer. Tests in `tests/` check
the rules after every step.

The owner, 2026-11-02: "I generally like the metaphor of a typical Linux filesystem. I want to try that
out." On transactions: "Frozen snapshot was my intention, yes. That's why I said “new branch” earlier."
On two edits to the same line at once he first said "im incline to say that the upstream (main) wins in
that case", then corrected it the same day: "main only wins when there is a collision from upstream to a
fork. If a sandbox pushes a conflicting change upstream, then it should be applied." Still binding from
earlier: forks stay, kept in step by syncing quickly ("making the fork design so rock-solid that it is
almost as effective, by syncing forks quickly"); sandboxes are peers; one history setting per volume; a
failed transaction is an implicit rollback.

## The rules

"Parent" means main for a sandbox, and the sandbox for a sandbox inside it. A "sync" brings the parent's
changes into a sandbox; a "landing" brings a sandbox's saved change into its parent. Rows marked *choice*
were decided here, not by the owner; each is open for him to change.

| # | Rule | Source | Tests |
|---|---|---|---|
| 1 | Works like a Linux filesystem. A write, sync or landing is never blocked, refused or held, and each writer sees its own write at once. Changes flow both ways between a sandbox and its parent without anyone asking. | Owner | `r1_writes_never_wait_and_forks_sync_both_ways` |
| 2 | Order without clocks. Each tree (main, each sandbox) has one queue, and changes enter it in queue order. Which of two edits is newer is read from git history: an edit is newer if it was made on a version that already held the other. No clock is read. | Owner | `r2_one_queue_fixes_the_landing_order`, `r2_no_clock_commit_dates_decide_nothing` |
| 3a | An edit made on the target's current version simply replaces it (fast-forward). | Owner | `r3_fast_forward_when_the_base_is_current` |
| 3b | Two edits to the same line made without seeing each other: the incoming one wins in the tree it is applied to. In a sync the parent's line wins in the sandbox; in a landing the sandbox's line wins in the parent. The losing line stays in history, and the collision is recorded, naming the line that overwrote it. | Owner | `r3_landing_collision_the_fork_wins_in_the_parent`, `r3_sync_collision_the_parent_wins_in_the_fork_until_the_fork_edit_lands` |
| 3c | A sync carries the parent's current version, never one from when it was queued, and a line the sandbox already replaced never comes back over its newer line. *Choice:* per line, git decides which line is newer: the line whose writing commit (`git blame`) descends from the other's. A sync skips each line whose last change in the parent the sandbox already holds (`git blame --first-parent`). | Owner; how is a choice | `r3_landing_collision_the_fork_wins_in_the_parent` (its stale sync), `r3_a_sync_never_brings_back_a_line_the_fork_already_took`, `r3_crossing_merges_keep_a_newer_edit` |
| 3d | *Choice:* only the colliding lines lose (per hunk, like `git merge -X theirs`). The tree's other lines in that file stay. | Recommended; see below | `r3_collision_scope_per_hunk_keeps_clean_lines`, `r3_collision_scope_per_hunk_sync_keeps_the_clean_hunk` |
| 4 | Nothing waits behind a collision. Every sandbox commit lands in order. Sandboxes inside sandboxes follow the same rules, with their sandbox as the parent. | Owner | `r4_nothing_waits_behind_a_collision`, `r4_nested_forks_follow_the_same_rules` |
| 5 | Transactions, opt-in. Begin opens a new branch, frozen at the sandbox's current version: the program sees its own edits, while the sandbox keeps receiving syncs. Commit merges the branch into the sandbox's current version. *Choice:* if a line it changed was also changed in the sandbox since begin, the commit is rejected; the program is told and sees the sandbox's version. Rollback, abandon or the client dying drops the branch. After a commit, the transaction's lines follow rule 3 like any others. | Owner; rejection is a choice | `r5_transaction_commits_clean`, `r5_transaction_rejected_on_a_same_line_collision`, `r5_transaction_rollback_and_client_death_leave_nothing`, `r5_a_committed_transaction_then_follows_rule_3` |
| 6 | *Choice* (the owner has not answered yet): every change traces back to exactly one writer. A sync is a merge commit labelled `Sync`, with the parent's commit as its second parent. Only merge commits combine writers, and a merge only carries lines it took from one of its parents. | Choice | `r6_a_sync_is_a_merge_with_the_parent_second`, every test (invariant) |
| 7 | One history setting per volume. Full lineage: main's history includes every sandbox commit. Squashed: main's history holds only main's own commits. | Owner | `r7_history_setting_and_losers_stay_reachable` |

## What must always hold

These are checked after every step, in every scenario, and in the random runs (`check.rs`).

| Invariant | Check |
|---|---|
| No tree silently loses a change. Every change that reached a tree is still there, or replaced by a line whose writer saw it, or named as lost by a recorded collision in that tree or one upstream of it. | `no_silent_loss` |
| No stale line ever reverts a newer one: a line is never replaced by one its writer had already seen. | `no_silent_loss` |
| The incoming change wins every same-line collision in the tree it is applied to, and only real collisions are recorded (two lines whose writers never saw each other). The collision names the lost and the winning lines; the losing version stays reachable under `refs/collisions/<n>`. | `incoming_wins` |
| Nothing is ever blocked or held. Every save returns at once, and every queue step takes its queue's first entry. | `interleavings.rs` `take` |
| A transaction ends as the program was told. Committed: every staged edit is in the sandbox's commit. Rejected or rolled back: every staged edit is discarded. | `transactions_end_as_told` |
| A rolled-back or rejected transaction leaves nothing in any tree or in history. | `discarded_leave_no_trace` |
| Every change traces back to one writer. A commit with one parent carries only its writer's lines. A merge carries only lines one of its parents holds, and names its second parent's writer (`agentfs` for a sync). | `one_writer` |
| Trees converge when writes stop: once the queues drain, every sandbox equals main. | `check_converged` |
| The history setting holds. | `history_setting` |
| No clock is read. The same calls run again with every commit dated before its parents decide the same trees and collisions; and the source names no clock. | `r2_no_clock_commit_dates_decide_nothing`, `random_interleavings_commit_dates_decide_nothing` (seeds 1 to 8) |

"Saw" is the test's own record, kept apart from git: a writer saw every line in the version it wrote on,
and in every version that version merged or carried. Under `Squashed` that record still knows what a
squashed landing carried; git does not (finding 2). The random run under `Squashed` relaxes the three
checks finding 2 breaks: real collisions, no stale revert, and "the collision is in that tree or one
upstream" (any tree's record counts). Every other invariant is checked in full.

The random runs replay seeds 1 to 64 in CI. Run more with `AGENTFS_MODEL_SEEDS=500`, or one seed with
`AGENTFS_MODEL_SEED=<n>`. Each run mixes main writes, up to four sandboxes (some inside others),
transactions, client exits and queue steps.

## Per hunk or per file: per hunk

Per file means the incoming version of a whole file replaces the tree's when any line collides. Then the
tree's other lines in that file go back to whatever the incoming side had, often an older line: main's
clean line, written after the sandbox's version, is reverted. That breaks "no stale line reverts a newer
one" (finding 3; 29 of 64 random seeds fail). Per hunk passes every check over 500 seeds with full
lineage.

## Coherence findings

Each finding has a test asserting what the rules intend, marked `#[ignore = "coherence finding N"]`
because the rules or the chosen mechanism do not deliver it. Run them with `--ignored`.

1. **git's own merge base is not enough (rule 3b against 3c).** The sandbox takes main's line m while
   main takes the sandbox's line x (each tree's incoming wins, so the two merges cross and disagree).
   The sandbox then writes y over m. With git's own three-way merge, the next sync finds two merge bases,
   merges them leaving conflict markers in the base, and treats y as colliding with x, which y's writer
   saw: x reverts y. The model's per-line rule (3c) keeps y. So "causality from git ancestry" holds per
   line (`git blame` and `git merge-base --is-ancestor`), not through `git merge`; an implementation needs
   that rule, not a plain merge. `finding_1_git_merge_base_on_crossing_merges`,
   `random_interleavings_git_merge_base` (1 of 64 seeds).
2. **Squashed history hides what rule 2 needs (rule 7 against rules 2 and 3c).** Main's squashed commit
   does not record which sandbox commit it carried, so git sees main's copy of a sandbox's line x as
   written by main. If the sandbox then writes y over x, the next sync carries x as concurrent with y, and
   x reverts y in the sandbox: the program's own older write undoes its newer one, recorded as a
   collision between two edits of one writer. The trees still converge once y lands. For the same reason
   a squashed sandbox commit that carries a collision it decided (a nested landing) reaches main without
   main recording it; the record is in the sandbox. One fix: keep the lineage somewhere other than main's
   history (for example a note naming the carried commit).
   `finding_2_squashed_sync_never_undoes_the_forks_newer_line`, `random_interleavings_squashed_every_check`
   (46 of 64 seeds).
3. **Per file reverts newer lines (rule 3d's other option against "no stale revert").** See above.
   `finding_3_per_file_keeps_clean_lines`, `random_interleavings_per_file_full_lineage`.
4. **The parent's win in a sandbox never lasts (rule 3b's two halves together).** A sandbox's edit x and
   main's concurrent edit m: if main's sync reaches the sandbox first, m wins there; but x is already
   queued for main, lands after m, and wins in main; the next sync brings x back. So the sandbox shows x,
   then m, then x. Every concurrent collision between a sandbox and its parent ends with the sandbox's
   line everywhere; the parent's line only shows in the sandbox until then. The same holds for a committed
   transaction: a sync queued before the commit can overwrite part of it in the sandbox until it lands
   (`r5_a_committed_transaction_then_follows_rule_3`). The rules as stated require this flip, and it is not
   a stale revert (m and x are concurrent), so the model keeps it; the test asserting "no flip" is
   ignored. To avoid it, a sync would have to skip lines the sandbox has changed and not yet landed, which
   contradicts "the parent's version wins in the fork". This needs the owner's choice.
   `r3_sync_collision_the_parent_wins_in_the_fork_until_the_fork_edit_lands`,
   `finding_4_a_fork_never_flips_back_to_its_own_edit`.
5. **git reads commit dates when it walks a range (rule 2).** `git rev-list A --not B` stops walking
   early by commit date. The model reads ancestry with `git merge-base --is-ancestor` and `git blame`
   only, and the date tests run every commit dated before its parents. An implementation must do the
   same: a wrong clock on one machine would otherwise decide what a sync applies.

## Left out

- **Deletes and renames.** Lines are only ever overwritten. A per-path rule for a delete against an edit
  (which side wins, and how a delete is dated) is not chosen yet. So rule 1, "never refused", is shown for
  line edits only.
- **Reverts, and two writers making the same line.** Every write is a new, unique line, so `git blame`
  finds the one commit that wrote it. A revert, or the same line written twice, would need the rules to
  say which write a line belongs to.
- **Real line layout.** Each slot sits between separator lines, so it is its own hunk. Edits to adjacent
  lines (which git treats as one conflict), and inserted or removed lines that shift the others, are not
  tested.
- **Writers racing inside one tree.** Each sandbox has one writer, and main's writers save one after
  another; two programs writing the same file of one tree at once is plain Linux behaviour, not modelled.
- Binary files; approvals and when landings run; integrator patches; service restarts; space limits;
  history truncation.
