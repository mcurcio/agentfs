# The landing model, in plain words

This crate runs the owner's landing rules of 2026-11-02 on the real `git` program. Main and each sandbox
(a "fork") are git branches. Every save is a commit that names its one writer. Tests in `tests/` check
the rules after every step.

The owner, 2026-11-02: "I generally like the metaphor of a typical Linux filesystem. I want to try that
out." On two edits to the same line at once: "im incline to say that the upstream (main) wins in that
case". On transactions: "Frozen snapshot was my intention, yes. That's why I said “new branch” earlier."
Still binding from earlier: forks stay, kept in step by syncing quickly ("making the fork design so
rock-solid that it is almost as effective, by syncing forks quickly"); sandboxes are peers; one history
setting per volume; a failed transaction is an implicit rollback.

## The rules

"Upstream" means the parent: main for a sandbox, the sandbox for a sandbox inside it. Rows marked
*choice* were decided here, not by the owner. Each one is open for him to change.

| # | Rule | Source | Tests |
|---|---|---|---|
| 1 | Works like a Linux filesystem. A write is never blocked, refused or held, and each writer sees its own write at once. Changes flow both ways between a sandbox and its parent without anyone asking. | Owner | `r1_writes_never_wait_and_forks_sync_both_ways` |
| 2 | Order without clocks. Each tree (main, each sandbox) has one queue, and changes enter it in queue order. Which edit came first is read from git history: the version each edit was made on. No clock is read. | Owner | `r2_one_queue_fixes_the_landing_order`, `r2_no_clock_same_calls_same_history` |
| 3a | An edit made on the target's current version simply replaces it (fast-forward). | Owner | `r3_fast_forward_when_the_base_is_current` |
| 3b | Two edits made at the same time are merged line by line. Where both changed the same line, the upstream line wins, in both directions: sandbox into parent, and parent into sandbox. The losing line stays in history, and the collision is recorded. *Choice:* each change is merged on the version it was made on, both ways. A sandbox commit lands on its own parent commit. A sync applies, in order, each parent commit the sandbox lacks, on that commit's own parent. It skips the parent's landing of a commit the sandbox already holds. | Owner; the bases are a choice (finding 2) | `r3_collision_on_landing_upstream_wins`, `r3_collision_on_sync_upstream_wins`, `r3_crossing_merges_keep_a_newer_edit` |
| 3c | *Choice:* only the colliding lines lose (per hunk, like `git merge -X ours`). The other lines of the same edit still land. | Recommended; see below | `r3_collision_scope_per_hunk_keeps_clean_hunks`, `r3_collision_scope_per_hunk_sync_keeps_the_clean_hunk` |
| 4 | Nothing waits behind a collision. Every sandbox commit lands in order. Sandboxes inside sandboxes follow the same rules, with their sandbox as the parent. | Owner | `r4_nothing_waits_behind_a_collision`, `r4_nested_forks_follow_the_same_rules` |
| 5 | Transactions, opt-in. Begin opens a new branch, frozen at the sandbox's current version: the program sees its own edits, while the sandbox itself keeps receiving changes. Commit merges the branch into the sandbox's current version. *Choice:* if any file it touched has a same-line collision, the commit is rejected; the program is told and sees the sandbox's version. Rollback, abandon or the client dying drops the branch. | Owner; rejection is a choice | `r5_transaction_commits_clean`, `r5_transaction_rejected_on_a_same_line_collision`, `r5_transaction_rollback_and_client_death_leave_nothing` |
| 6 | *Choice* (the owner has not answered yet): every change traces back to exactly one writer. A sync is a merge commit labelled `Sync`, with the parent's commit as its second parent. Only merge commits combine writers, and a merge only carries lines it took from one of its parents. | Choice | `r6_a_sync_is_a_merge_with_the_parent_second`, every test (invariant) |
| 7 | One history setting per volume. Full lineage: main's history includes every sandbox commit. Squashed: main's history holds only main's own commits. | Owner | `r7_history_setting_and_losers_stay_reachable` |

## What must always hold

These are checked after every step, in every scenario, and in the random runs (`check.rs`).

| Invariant | Check |
|---|---|
| Main never silently loses a change. Every change that reached a tree is either still there (or replaced by an edit made on it), or a recorded collision in that tree, or in a tree upstream of it, names it. | `no_silent_loss` |
| Upstream wins every same-line collision, and only real collisions are recorded (two edits made without seeing each other). The losing commit stays reachable under `refs/collisions/<n>`. | `upstream_wins` |
| Nothing is ever blocked or held. Every save returns at once, and every queue step takes its queue's first entry. | `interleavings.rs` `take` |
| A rolled-back or rejected transaction leaves nothing in any tree or in history. | `discarded_leave_no_trace` |
| Every change traces back to one writer. | `one_writer` |
| Trees converge when writes stop: once the queues drain, every sandbox equals main. | `check_converged` |
| The history setting holds. | `history_setting` |
| No clock is read. | `r2_no_clock_same_calls_same_history` |

The random runs replay seeds 1 to 64 in CI. Run more with `AGENTFS_MODEL_SEEDS=500`, or one seed with
`AGENTFS_MODEL_SEED=<n>`. Each run mixes main writes, up to four sandboxes (some inside others),
transactions, client exits and queue steps.

## Per hunk or per file: per hunk

Per file means the upstream's whole file wins a collision. It loses more: the clean lines of the losing
edit go too (`r3_collision_scope_per_hunk_keeps_clean_hunks`), and with full lineage the trees never
converge (finding 4). Per hunk is what the owner's own example names (`git merge -X ours`), and it
passed every check over 500 seeds with full lineage.

## Coherence findings

Each finding has a test that asserts what the rules intend. The test is marked `#[ignore = "coherence
finding N"]` because the rules do not deliver it today. Run them with `--ignored`.

1. **A committed transaction can land in pieces (rules 3 and 4 against rule 5).** The commit checks for
   collisions only against the sandbox's version at that moment, and later landings follow rule 3 line by
   line. Example: a program writes lines A and B in a transaction and commits, and the commit is accepted.
   Then main changes line A before the commit lands, and main's A wins. Main ends up with B but not A. The
   same happens inside the sandbox when a change from main was already queued at commit time.
   `finding_1_a_committed_transaction_reaches_main_whole_or_not_at_all`,
   `finding_1_a_committed_transaction_stays_whole_in_its_fork`.
2. **git's own merge base gets it wrong when merges cross (rule 2 against rule 3, with full lineage).**
   A sandbox takes main's colliding line while main is landing the sandbox's losing commit, so the two
   merges cross. The sandbox then writes over main's line. On the next sync, git finds two merge bases. It
   combines them leaving conflict markers in the base, so the sandbox's newer line "collides" with the line
   it was written over, and loses. That is a recorded collision between edits that saw each other. It
   failed 3 of 64 seeds. Two other bases were tried here and also failed: merging the several bases by
   rule 3 failed 7 of 500 seeds, and "the last parent commit synced" brought a sandbox's own landed
   line back over its newer one. The model therefore merges each change on its own base, both ways (row
   3b), which passes 500 seeds. So "causality from git ancestry" holds per edit, not through `git merge`.
   `finding_2_git_merge_base_on_crossing_merges`, `random_interleavings_git_merge_base`.
3. **Squashed history loses what rule 2 needs (rule 7 against rule 2).** Main's squashed commit does not
   record which sandbox commit it carried. So the next sync into the sandbox sees the sandbox's own earlier
   line, which has just landed, as a change made in main. It then wins over the sandbox's newer line. The
   trees still converge once the newer line lands, but in the meantime the program's write is undone, and a
   collision is recorded between two edits by one writer. One fix: keep the lineage somewhere other than
   main's history. `finding_3_squashed_sync_never_undoes_the_forks_newer_line`,
   `random_interleavings_squashed_collisions_are_real`. The CI run under `Squashed` checks every other
   invariant.
4. **Per file does not converge with full lineage (rule 3's choice against convergence).** A sync that
   takes main's whole file also drops the sandbox's clean line in that file. The sandbox's own commit of
   that line still lands cleanly in main, because there it collides with nothing. Main's landing of a
   commit the sandbox already holds brings it nothing new, so the sandbox never gets the line back.
   `finding_4_per_file_sync_converges`, `random_interleavings_per_file_full_lineage` (13 of 64 seeds
   fail).

## Left out

Deletes, renames and binary files; approvals and when landings run; integrator patches; service restarts;
space limits; history truncation.
