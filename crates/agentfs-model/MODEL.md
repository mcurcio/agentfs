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
| 1 | Works like a Linux filesystem. A write, sync, landing or transaction commit is never blocked, refused or held, and each writer sees its own write at once. Changes flow both ways between a sandbox and its parent without anyone asking. | Owner | `r1_writes_never_wait_and_forks_sync_both_ways` |
| 2 | Order without clocks. Each tree (main, each sandbox) has one queue, and changes enter it in queue order. Which of two edits is newer is read from git history: an edit is newer if it was made on a version that already held the other. No clock is read. | Owner | `r2_one_queue_fixes_the_landing_order`, `r2_no_clock_commit_dates_decide_nothing` |
| 3a | An edit made on the target's current version simply replaces it (fast-forward). | Owner | `r3_fast_forward_when_the_base_is_current` |
| 3b | Two edits to the same line made without seeing each other: the incoming one wins in the tree it is applied to. In a sync the parent's line wins in the sandbox; in a landing the sandbox's line wins in the parent. The losing line stays in history, and the collision is recorded once, in the tree that decided it, naming the line that overwrote it. | Owner; "once" is a choice | `r3_landing_collision_the_fork_wins_in_the_parent`, `r3_sync_collision_the_parent_wins_in_the_fork_until_the_fork_edit_lands`, `r3_a_loss_decided_upstream_is_recorded_once` |
| 3c | A sync carries the parent's current version, never one from when it was queued, and a line the sandbox already replaced never comes back over its newer line. *Choice:* per line, git decides which line is newer: the line whose writing commit (`git blame`) descends from the other's. A sync skips each line whose last change in the parent the sandbox already holds (`git blame --first-parent`). | Owner; how is a choice | `r3_landing_collision_the_fork_wins_in_the_parent` (its stale sync), `r3_a_sync_never_brings_back_a_line_the_fork_already_took`, `r3_crossing_merges_keep_a_newer_edit` |
| 3d | *Choice:* only the colliding lines lose (per line). The tree's other lines in that file stay, neighbouring lines included. | Recommended; see below | `r3_collision_scope_per_line_keeps_clean_lines`, `r3_collision_scope_per_line_sync_keeps_the_clean_line`, `r3_neighbouring_lines_merge_per_line` |
| 4 | Nothing waits behind a collision. Every sandbox commit lands in order. Sandboxes inside sandboxes follow the same rules, with their sandbox as the parent. | Owner | `r4_nothing_waits_behind_a_collision`, `r4_nested_forks_follow_the_same_rules` |
| 5 | Transactions, opt-in. Begin opens a new branch, frozen at the sandbox's current version: the program sees its own edits, while the sandbox keeps receiving syncs. Commit merges the branch into the sandbox's current version. *Choice:* the commit is never refused: it is the incoming change into the sandbox, so its line wins a same-line collision with a line a sync brought in since begin, recorded like any other (rule 3b). Rollback, abandon or the client dying drops the branch. After a commit, the transaction's lines follow rule 3 like any others. | Owner; never refused is a choice | `r5_transaction_commits_clean`, `r5_transaction_commit_wins_over_a_concurrent_line`, `r5_sync_before_or_after_the_commit_ends_the_same`, `r5_transaction_rollback_and_client_death_leave_nothing`, `r5_a_committed_transaction_then_follows_rule_3` |
| 6 | *Choice* (the owner has not answered yet): every change traces back to exactly one writer. A sync is a merge commit labelled `Sync`, with the parent's commit as its second parent. Only merge commits combine writers, and a merge only carries lines it took from one of its parents. | Choice | `r6_a_sync_is_a_merge_with_the_parent_second`, every test (invariant) |
| 7 | One history setting per volume. Full lineage: main's history includes every sandbox commit. Squashed: main's history holds only main's own commits. *Choice:* under Squashed, the sandbox commit each landing carried is recorded beside main's history as a git graft (`git replace --graft`, under `refs/replace/`). git's ancestry and blame read it; main's stored history, as pushed, does not. | Owner; the graft is a choice | `r7_history_setting_and_losers_stay_reachable`, `r7_squashed_sync_never_undoes_the_forks_newer_line` |

Earlier versions of this model refused a transaction commit on a same-line collision. That fitted "upstream
wins", not the owner's correction, and its outcome depended on timing: the same concurrent main line made the
commit fail if its sync arrived first, and lost to the commit if it arrived after. Now both orders end with
the same trees (`r5_sync_before_or_after_the_commit_ends_the_same`).

## What must always hold

These are checked after every step, in every scenario, and in the random runs (`check.rs`), under both
history settings.

| Invariant | Check |
|---|---|
| No tree silently loses a change. Every change that reached a tree is still there, or replaced by a line whose writer saw it, or named as lost on that same file and line by a recorded collision in that tree or one upstream of it. | `no_silent_loss` |
| No stale line ever reverts a newer one: a line is never replaced by one its writer had already seen. | `no_silent_loss` |
| The incoming change wins every same-line collision in the tree it is applied to, and only real collisions are recorded (two lines whose writers never saw each other). The collision names the lost and the winning lines; the losing version stays reachable under `refs/collisions/<n>`. | `incoming_wins` |
| Every sync delivers the parent's version at once: afterwards the sandbox holds each of the parent's lines, unless it holds a newer one or already lost that line in a collision recorded in the sandbox. | `syncs_deliver` |
| Nothing is ever blocked or held. Every save and commit returns at once, and every queue step takes its queue's first entry. | `interleavings.rs` `take` |
| A transaction ends as the program was told. Committed: every staged edit is in the sandbox's commit. Rolled back: every staged edit is discarded. | `transactions_end_as_told` |
| A rolled-back transaction leaves nothing in any tree or in history. | `discarded_leave_no_trace` |
| Every change traces back to one writer. A stored commit with one parent carries only its writer's lines. A merge carries only lines one of its parents holds, and names its second parent's writer (`agentfs` for a sync). | `one_writer` |
| Trees converge when writes stop: once the queues drain, every sandbox equals main. | `check_converged` |
| The history setting holds: a tree's lineage reaches every commit that landed in it; its stored history does too under Full lineage, and under Squashed none (main's holds only main's commits). Read by ancestry alone. | `history_setting` |
| No clock is read. The same calls run again with every commit dated before its parents decide the same trees and collisions, under both history settings; and the source names no clock. | `r2_no_clock_commit_dates_decide_nothing`, `random_interleavings_commit_dates_decide_nothing` (seeds 1 to 8) |

"Saw" is the test's own record, kept apart from git: a writer saw every line in the version it wrote on,
and in every version that version merged or carried.

The random runs replay seeds 1 to 64 in CI: full lineage, squashed, and full lineage with every slot on a
neighbouring line (one git hunk). Run more with `AGENTFS_MODEL_SEEDS=500`, or one seed with
`AGENTFS_MODEL_SEED=<n>`. Each run mixes main writes, up to four sandboxes (some inside others),
transactions, client exits and queue steps; a transaction is mostly committed once it has staged something,
so commits meet concurrent lines.

## Per line, per hunk or per file: per line

Per file means the incoming version of a whole file replaces the tree's when any line collides. Then the
tree's other lines in that file go back to whatever the incoming side had, often an older line: main's
clean line, written after the sandbox's version, is reverted. That breaks "no stale line reverts a newer
one" (finding 3; 36 of 64 random seeds fail). Per hunk in git's sense (`git merge -X theirs`) is the same
fault in small: git puts neighbouring changed lines in one hunk, and takes the incoming hunk whole
(finding 6). Per line passes every check over 500 seeds under both history settings and both layouts.

## Coherence findings

Each open finding has a test asserting what the rules intend, marked `#[ignore = "coherence finding N"]`
because the rules or the chosen mechanism do not deliver it. Run them with `--ignored`.

1. **git's own merge base is not enough (rule 3b against 3c).** The sandbox takes main's line m while
   main takes the sandbox's line x (each tree's incoming wins, so the two merges cross and disagree).
   The sandbox then writes y over m. With git's own three-way merge, the next sync finds two merge bases,
   merges them leaving conflict markers in the base, and treats y as colliding with x, which y's writer
   saw: x reverts y. The model's per-line rule (3c) keeps y. `finding_1_git_merge_base_on_crossing_merges`,
   `random_interleavings_git_merge_base` (2 of 64 seeds).
2. **Resolved: Squashed history hid what rule 2 needs (rule 7 against rules 2 and 3c).** A squashed commit
   in main does not name the sandbox commit it carried. Without more, git saw main's copy of a sandbox's
   line x as written by main; when the sandbox then wrote y over x, the next sync carried x as concurrent
   and x reverted y (46 of 64 seeds), and a squashed nested landing reached main with no collision recorded
   there. The model now keeps that link beside history as a graft (rule 7), and Squashed passes every
   check. Costs: every sandbox commit stays in the repository, reachable from its graft, though main's
   pushed history does not reach it; a clone needs `refs/replace/` as well to decide syncs the same way;
   and the system design's squash shape (`domains/history.md`: a merge whose second parent is a squash
   commit on a per-fork chain) is not the model's single-parent commit, though the same graft serves it.
   `r7_squashed_sync_never_undoes_the_forks_newer_line`, `random_interleavings_squashed`.
3. **Per file reverts newer lines (rule 3d's other option against "no stale revert").** See above.
   `finding_3_per_file_keeps_clean_lines`, `random_interleavings_per_file_full_lineage`.
4. **The parent's win in a sandbox never lasts (rule 3b's two halves together). Needs the owner.** A
   sandbox's edit x and main's concurrent edit m: if main's sync reaches the sandbox first, m wins there;
   but x is already queued for main, lands after m, and wins in main; the next sync brings x back. So the
   sandbox shows x, then m, then x. Every concurrent collision between a sandbox and its parent ends with
   the sandbox's line everywhere; "the parent wins in the fork" holds only until then. In the sandbox's own
   view this also splits a multi-line edit for that time, a committed transaction included
   (`r5_a_committed_transaction_then_follows_rule_3`). No line is lost without a record and nothing stale
   reverts, so it is coherent, and the trees converge. To avoid the flip, a sync would have to skip lines
   the sandbox has changed and not yet landed, which contradicts "the parent's version wins in the fork".
   `r3_sync_collision_the_parent_wins_in_the_fork_until_the_fork_edit_lands`,
   `finding_4_a_fork_never_flips_back_to_its_own_edit`.
5. **git reads commit dates when it walks a range (rule 2).** `git rev-list A --not B` stops walking
   early by commit date. The model reads ancestry with `git merge-base --is-ancestor`, `git blame` and
   whole-history `git rev-list <head>` only, and the date tests run every commit dated before its parents.
   An implementation must do the same: a wrong clock on one machine would otherwise decide what a sync
   applies.
6. **git's hunk is not a line (rule 3d).** A sandbox changes line 1; main changes line 0. Per line, both
   stay. git's merge sees one conflicting hunk, and taking the incoming side (`-X theirs`) puts main's
   older line 1 back over the sandbox's newer one. `finding_6_git_hunks_on_neighbouring_lines`.
7. **The merge an implementation needs is not a git merge strategy.** Findings 1 and 6 show that git's
   own merges (any merge base, `-X theirs`) let a stale line revert a newer one under rule 3. So the rule
   lives in the model's own code (`World::resolve`): git stores the commits and answers "who wrote this
   line" (`git blame`) and "did this commit see that one" (`git merge-base --is-ancestor`); the model
   decides each line from those answers. An implementation needs the same per-line merge of its own. The
   test oracle ("saw") is kept apart from both, so the checks do not trust the code they check.

## Left out

- **Deletes and renames.** Lines are only ever overwritten. A per-path rule for a delete against an edit
  (which side wins, and how a delete is dated) is not chosen yet. So rule 1, "never refused", is shown for
  line edits only.
- **Inserted, removed, moved and repeated lines.** A file is a fixed number of lines, each write a new,
  unique line, so `git blame` finds the one commit that wrote each line exactly. With inserted or removed
  lines blame matches lines by diff, which is approximate for moved or repeated lines, and a revert or the
  same line written twice would need the rules to say which write a line belongs to. The per-line rule is
  therefore shown for in-place edits only, on separate and on neighbouring lines.
- **Writers racing inside one tree.** Each sandbox has one writer, and main's writers save one after
  another; two programs writing the same file of one tree at once is plain Linux behaviour, not modelled.
- Binary files; approvals and when landings run; integrator patches; service restarts; space limits;
  history truncation.
