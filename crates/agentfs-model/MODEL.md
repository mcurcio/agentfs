# The landing model, in plain words

This crate runs the owner's landing rules on the real `git` program. Main and each sandbox (a "fork") are
git branches. Every save is a commit that names its one writer. Each line of a file gets an identity when a
save first records it, and keeps it through later edits around it (rule 10); a file's name and existence are
one more line (rule 11). Tests in
`tests/` check the rules after every step. How a change to these rules is verified (commands, seeds, planted
bugs, review, reading a failure) is in [VERIFYING.md](VERIFYING.md).

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
- 2026-11-03, on what a save records: "if we're trying to emulate a traditional linux filesystem, then
  wouldnt this depend on how the write occurred? Like an append vs a write? I dont think that agentfs should
  try to do anything beyond record the actions through the filesystem".
- 2026-11-03, on locks, after asking "If the sandbox has edited line 5 (the file is open) then doesnt that
  imply that the sandbox has a lock on the file and the upstream commit landing would be queued waiting for
  that lock?": "A sync should acquire a lock on every file that it aims to overwrite, yes". And 2026-11-02:
  "freezing only the files that a landing touches sounds reasonable".
- 2026-11-03, "Go", in reply to the recommended next steps: accept close-to-open visibility for open files;
  trim the superseded older pages; settle what a "line" is in a real file; a rule for deletes and renames.
  Close-to-open is therefore his decision; rules 10 and 11 below answer the last two, as design choices.
- 2026-11-03, on how much of a change a collision costs, after reading that under the per-line rule "one line
  loses": "“One line loses” sounds like a collision, and I thought we had already decided that each commit was a
  transaction"; then "some commits represent a transaction, and some commits represent a collection of changes.
  Those two could have different operational models"; then, on what a plain save is: "The whole file, right? That
  was the “traditional Linux fs” model we had discussed". He then asked "Wouldn’t a squashed changes still be
  merged file-by-file? Or does the squashing happen after the merge decisions are made — so only the winning
  changes get squashed? What happens to the losing files?", was told rule 7's answer below (each save judged on
  its own first, only winners squashed, losers kept as their own commits under collision refs), and did not
  object; rule 7 marks that answer a choice he did not object to, not his decision. Rule 3e replaces the per-line application the model had before, with no setting to keep it.
- Still binding from earlier: forks stay, kept in step by syncing quickly; sandboxes are peers; one history
  setting per volume; a failed transaction is an implicit rollback.

## The rules

"Parent" means main for a sandbox, and the sandbox for a sandbox inside it. A "sync" brings the parent's
version into a sandbox; a "landing" brings a sandbox's saved change into its parent. Two lines "collide"
when each was written without seeing the other. Rows marked *choice* were decided here, not by the owner;
each is open for him to change.

| # | Rule | Source | Tests |
|---|---|---|---|
| 1 | Works like a Linux filesystem. A close is never refused and its save is in the tree at once (the model has no reads through an open file: what a writer reads back through its own open file is the target's matter, views.md); no collision holds a sync, landing or transaction commit (the only waits are rule 9's locks). Changes flow both ways without anyone asking. *Choice* (read from the owner's 2026-11-03 question, not his words): a save's first parent is its tree's head when it is made, so whatever the tree held then counts as seen. | Owner; first parent at save time is a choice | `r1_writes_never_wait_and_forks_sync_both_ways` |
| 2 | Order without clocks. Each tree has one queue, taken in order. Which of two lines is newer is read from git, never from a clock. | Owner | `r2_one_queue_fixes_the_landing_order`, `r2_no_clock_commit_dates_decide_nothing` |
| 3a | Every collision between a sandbox and its parent is decided in exactly one place, the parent's landing queue, and recorded once, there: one decision per losing change, kept by one collision ref, with a record under it for each file and cause it lost by. A sync never decides one: the sandbox keeps its own line until the parent's queue has decided it, then takes the parent's decision. So a sandbox's line changes at most once per collision: never its own, then the parent's, then its own again. | Owner | `r3_a_collision_is_recorded_once_at_the_parents_queue`, `r3_a_fork_never_flips_back_to_its_own_edit` |
| 3b | There, the parent's version wins a collision. The sandbox's losing line stays in history (kept under `refs/collisions/<n>`, which Replication pushes to the shared remote, so its writer and its place stay readable there), the collision names both lines, and the sandbox then takes the parent's line. A save made before the parent's sync arrived is concurrent with it and loses. At every level: a sandbox is the parent of the sandboxes inside it. | Owner | `r3_landing_collision_the_parent_wins`, `r3_a_save_before_the_sync_arrives_loses_to_the_parent`, `r4_nested_forks_follow_the_same_rules` |
| 3c | A line written on top of the parent's current line (its writer saw it) is not a collision; it replaces it. So a save made after a sync brought the parent's line wins upstream by lineage. *Choice:* "saw" is read per line: the versions a version was written over, from git (the line maps and first parents), not which commits its version merged. | Owner; per line is a choice | `r3_fast_forward_when_the_base_is_current`, `r3_a_save_after_the_sync_wins_upstream_by_lineage`, `r3_an_edit_over_the_decided_line_replaces_it`, `r3_an_edit_over_an_undecided_line_still_collides` |
| 3d | A sync carries the parent's current decided version, never a stale one. *Choice:* the sandbox keeps exactly its lines whose writing commit the parent's queue had not decided when the sync read the parent's head: the parent's lineage does not reach it, and no collision ref of the parent or a tree above it then names a change that reaches it (under Squashed a losing save is in no graft, rule 7); when none is left and nothing of it waits in the parent's queue, the sandbox's branch moves to the parent's commit (a fast-forward). | Owner; how is a choice | `r3_a_sync_carries_the_parents_current_version`, `r3_a_sync_keeps_the_undecided_saves`, `r7_squashed_sync_never_undoes_the_forks_newer_line`, `r7_a_squashed_take_squashes_only_the_winners` |
| 3e | A change lands whole or loses whole at its parent's queue: if any of its lines collides and the parent wins, the whole change loses, its lines that did not collide included, and nothing of it lands; it stays in history, kept by its collision ref, and every line of it is named (a line that did not collide, `By::Whole`, for the parent's line that decided). An ordinary save is the whole file as written at one close (a rename-over, one call, its two name lines); a transaction's commit is all its files, and its answer tells the program (rule 5). Each later save is judged whole on its own. A loss by a file rule (rule 11) costs the whole change the same way, its other files included. Only a lost line costs a change: a line that is dropped (the parent has that version, or one written over it, or a delete that agrees) costs nothing. Within one change, lines are still judged per line (rules 3c, 10, 11): neighbouring lines of two different changes both apply. | Owner, 2026-11-03 | `r3_a_save_loses_whole_with_its_clean_lines`, `r3_a_change_whose_delete_agrees_lands_whole`, `r3_a_later_save_after_a_losing_one_is_judged_on_its_own`, `r3_a_transaction_loses_whole_and_is_told`, `r3_a_committed_transaction_loses_whole_at_main`, `r3_a_file_rule_loss_costs_the_whole_change`, `r4_nested_changes_land_whole_or_lose_whole_at_every_level`, `r3_neighbouring_lines_merge_per_line` |
| 4 | Nothing waits behind a collision: every sandbox commit is taken, in order, and lands or loses whole. | Owner | `r4_nothing_waits_behind_a_collision` |
| 5 | Transactions, opt-in. Begin opens a new branch frozen at the sandbox's version; the program sees its own edits while the sandbox keeps syncing. Rollback, abandon or the client dying drops the branch. Commit lands the branch in the sandbox as a sandbox lands in its parent, whole or not at all (rule 3e, owner): the sandbox's line wins a collision (one a sync brought in since begin) and then none of the transaction applies; no collision refuses the commit; its answer says it lost and names every staged line (`TxnOutcome::Lost`). *Choice:* a transaction that lost still writes its merge commit, with the sandbox's tree, so history shows the decision. The commit then lands upstream like any change, whole or not at all. *Choice:* a file open before begin keeps writing to the sandbox; a commit is refused (`OpenFiles`) only while the program itself holds open for writing a file of the transaction or a sandbox file the commit would write, since a wait would wait on the caller. | Owner; the losing merge commit and open files are choices | `r5_open_files_and_transactions`, `r5_transaction_commits_clean`, `r3_a_transaction_loses_whole_and_is_told`, `r5_sync_before_or_after_the_commit_ends_the_same`, `r5_a_committed_transaction_then_lands_like_any_change`, `r5_transaction_rollback_and_client_death_leave_nothing` |
| 6 | *Choice:* every change traces back to one writer. A sync commit (the merge a sync makes when the sandbox has commits of its own the parent has not taken, so it cannot fast-forward) is labelled `Sync`, credited to `agentfs`, with the parent's commit second; it has no edits of its own: a merge carries only lines a parent holds. | Choice | `r6_a_sync_is_a_merge_with_the_parent_second`, every test |
| 7 | One history setting per volume. Full lineage: every sandbox commit is taken on its own and stays in main's history, a losing one included. Squashed: main's history holds only main's own commits; a sandbox's plain saves sitting directly behind each other in the parent's queue are taken together; each transaction commit lands as its own unit. Every sandbox commit stays stored. A squashed changeset is history only, never a unit judged whole: each save of the take is judged on its own first, in order (rule 3e), and only the winners are the one squashed commit; a loser is in no commit of the parent, stays its own commit on the sandbox's branch, and its collision ref, which Replication pushes, names it. *Choice:* the winning saves are recorded beside history as a git graft (`refs/replace/`, the squash's parents read as the parent's head and each winning save); a nested sandbox's landing also lands on its own (it carries another writer's lines), and the parent above judges it as the saves its graft names; a squash takes only the saves directly behind its first, never one from behind another entry (another sandbox's, a sync, a transaction's commit), so it judges the same changes in the same order as one take each and the setting decides no collision; a take whose saves all lost still writes an (empty) commit with no graft, so the parent's history shows the decision and its sandboxes sync. | Owner (one setting per volume, squashing plain saves, transaction commits on their own, every commit stored); *choice (not objected to)*: only winners squashed, his question answered by this design, to which he did not object; the graft, adjacency and the rest are choices | `r7_squashed_saves_land_as_one_changeset`, `r7_a_squashed_take_squashes_only_the_winners`, `r7_the_history_setting_decides_no_collision`, `r7_history_setting_and_losers_stay_reachable`, `random_interleavings_history_settings_decide_the_same` |
| 8 | A save records what the program's write calls did, applied at close to the tree's head. POSIX gives byte calls only. An append (`O_APPEND`, which the save knows) adds a line after the file's last line as it is then, so it never writes over a line, another writer's concurrent append included. Byte writes at an offset change bytes; every line they leave unchanged keeps what the tree holds (a line a sync brought included). A truncate and rewrite replaces the whole file, so a stale rewrite really puts old content back, recorded as that program's write over what it replaced, the replaced version kept in history. Which lines the bytes changed is rule 10's alignment. | Owner; the alignment is a choice | `r8_concurrent_appends_never_overwrite_each_other`, `r8_an_append_after_the_sync_keeps_the_synced_line`, `r8_an_offset_write_elsewhere_keeps_the_synced_line`, `r8_a_stale_rewrite_overwrites_the_synced_line_and_history_keeps_it` |
| 9 | A sync into a tree acquires a lock on every file it will overwrite: it waits until no client of that tree has the file open for writing, holds the files while it writes, and a new write-open of them waits until it finishes. Every wait ends on an event (a close, a client's death, a release), never a timer. *Choice:* the same for a landing into a tree; a take waits for all its files at once (all or nothing, so it never holds one file while waiting for another), and its queue waits behind it in order; a client's death (its end event, a sandbox's client or one of main's) closes its open files, saving their write calls as the kernel's close on exit would, and drops its waiting opens; the lock covers every call that changes a held file: `unlink` and `rename(2)` onto or away from it wait like a write-open and the take's release runs them (path `truncate(2)` too, not modelled). | Owner; landings, all or nothing, death and path calls are choices | `r9_a_sync_waits_for_an_open_writer_then_applies`, `r9_a_landing_waits_for_the_parents_open_writer`, `r9_a_write_open_waits_while_a_sync_holds_the_file`, `r9_client_death_releases_its_locks`, `r9_a_main_clients_death_releases_its_locks`, `r11_an_unlink_or_rename_of_a_held_file_waits` |
| 10 | *Choice:* at close the save aligns the program's bytes with the head's lines once, by git's histogram diff, and records the result beside the bytes (a line map): unchanged lines keep their versions; a changed region pairs old and new lines in order, each new one a new version of its old one, written over the version the writer saw; extra new lines get new identities; extra old ones get a hidden deleted version. An append needs no alignment. A move is read as a delete plus a new line; among identical lines, which one changed is the diff's pick. From then on the referee (rule 3) and a sync compare versions of one identity, never positions or bytes: a line new to the parent applies; two deletes agree; of two inserts at one place the parent's comes first; an applied version is placed after the nearest line before it that both sides hold. | Choice, asked for by the owner's "Go" | `r10_an_insert_shifts_lines_without_changing_their_identity`, `r10_concurrent_inserts_at_one_place_keep_both_the_parents_first`, `r10_a_delete_against_a_concurrent_edit_the_parent_wins`, `r10_a_move_is_a_delete_plus_an_insert`, `r10_repeated_identical_lines_align_by_the_diff` |
| 11 | *Choice:* a file's identity is fixed at create and kept by a rename (as an inode); its path and existence are its name line, decided by rule 3 like any line: a rename is a new version, an unlink a deleted one, a rename-over both in one save. Coupled with the content, the parent winning: an unlink loses whole if the parent has a live line its writer never saw; an edit of a file the parent unlinked loses, all of it; a rename and an edit both apply (the edit follows the file); two files renamed to one path, the parent's keeps it; a rename-over (both of whose files the parent has) applies or loses whole. The atomic-save idiom, a rename-over from a file the parent never had, is a rewrite of the replaced file instead (git-model.md §7); it needs creates, which the model lacks. | Choice, asked for by the owner's "Go" | `r11_an_unlink_against_a_concurrent_edit_the_parent_wins`, `r11_an_unlink_after_the_sync_wins`, `r11_a_rename_and_a_concurrent_edit_both_apply`, `r11_concurrent_renames_the_parent_wins`, `r11_a_rename_over_applies_or_loses_whole`, `r11_a_rename_over_whose_rename_loses_keeps_the_replaced_file`, `r11_an_open_file_follows_a_rename_and_an_unlinked_one_saves_nothing` |

## What must always hold

Checked after every step, in every scenario and random run (`check.rs`), under both history settings.

| Invariant | Check |
|---|---|
| No tree silently loses a change: every version that reached a tree is still its line's version there, or replaced by a version written over it, or a delete its delete agrees with, or named lost by a collision in that tree or one upstream. | `no_silent_loss` |
| No stale version reverts a newer one, unless a collision decided against the newer one: its lost version and winner really collided (a lost unlink gives the file back), or it is a line of a change that lost whole because another of its lines really collided (rule 3e: a losing save's clean line goes back to the parent's). A version lost by no real collision of its change is still a stale revert. | `no_silent_loss` |
| No flip-flop: a version, once replaced in a tree, never shows there again, unless a collision decided against the version replacing it. | `no_flicker` |
| The parent's version wins every collision, decided only at the parent's queue: read from git, the result is that tree's own landing (or transaction) commit and the losing commit was made in a sandbox below it; the result holds no lost version, keeps the tree's own version of every lost line (or one a winning save of the same squashed take set) and holds every winner; the two versions really collided (for a file rule, at least one pair; for a line lost only with its change, a real collision of that change); the losing commit stays reachable under its ref. | `parent_wins` |
| No partial save or transaction ever reaches a parent: each change a take judged lost (named by a collision of that take) or won, never both; every version its own edit set (read from git) is, if it lost, absent from the take's result and named lost, and if it won, in the result (or a version written over it). | `changes_whole` |
| A squashed commit holds only judged winners: under Squashed, its lineage parents (stored parent plus graft) are the parent's previous head and exactly the take's winning saves, and every version it sets was set by one of them. | `changes_whole` |
| A sync records no collision, and carries the parent's head as it was when its take began (read before the step, not from the model's record). | `step_checked` |
| Every sync delivers the parent's decided version: the sandbox holds the parent's version of every line except its own versions the parent's queue had not taken when the sync's take began, and drops none of the parent's lines. | `syncs_deliver` |
| Nothing else waits: every close and commit returns at once; every queue step takes its queue's first entry (a squash: with the same sandbox's saves directly behind it, never one from behind another entry) and leaves the rest in order, or records that it waits. | `interleavings.rs` `take` |
| No sync or landing writes a file open for writing in that tree, or one it did not hold. | `step_checked` |
| No write-open succeeds while a take holds the file, and a waiting unlink or rename waits only while a take holds one of its files; no lock outlives its holder (a sandbox's client or one of main's, its transaction, a tree); every lock wait ends on an event: a waiting take waits only while one of its files is still open for writing, a waiting open only while a take holds its file; none is left once writes stop. | `locks`, `check_converged` |
| A transaction ends as the program was told, whole: a committed one has every staged line in the sandbox (a staged delete may meet the sandbox's own) and no collision; one that lost has none of its versions in the sandbox, a real collision, and every staged line named in the answer and by a collision; rolled-back ones leave no version in any tree or history (bytes a program read and writes back later are its new write, rule 8). | `transactions_end_as_told`, `discarded_leave_no_trace` |
| Every change traces back to one writer. | `one_writer` |
| An append writes over no line: each line an append wrote (recorded at the write call, before the close places it) is a new line after every line of its save's parent. | `one_writer` |
| A save's line map is a valid alignment, read from its blob and its first parent's alone: every line identity the parent holds is still there, in the same order, unchanged or one new version written over the parent's; every other line is new and live. | `save_aligned` (in `one_writer`) |
| A save holds exactly the bytes its write calls leave on the head at close, computed by the harness from its own record of the calls, not by the model's replay. | `interleavings.rs` `close_checked` |
| A rename-over applies whole: a file unlinked by one has the renamed file at that save's name or a later one (in a sandbox, unless it holds its own name for that file, not its parent's). | `rename_overs_whole` |
| Every losing change is replicated, eventually, and stays: once Replication's push (`Step::Replicate`, its own step, never a referee's) has drained, every collision of the run is read on the shared remote alone: its collision ref names the losing commit, each lost version is in it and was written by a commit naming that version's writer, and a collision decided in main has its result in the remote main's history. | `losers_pushed` (in `check_converged`) |
| Trees converge when writes stop: every sandbox equals main. | `check_converged` |
| The history setting holds, read by ancestry alone: a tree's lineage reaches every change that won in it (or, under Squashed, a collision ref upstream keeps it, once it lost there), and under full lineage every change it took. Across the CI seeds, changes really lose whole with clean lines in them, and under Squashed runs of saves really land as one changeset, some beside a losing save of the same take. | `history_setting`, `interleavings.rs` `run_all` |
| The history setting decides nothing: the same calls, in rounds drained between, decide the same trees and the same collisions (tree, file, lines, lost and winning versions, cause) under full lineage and squashed. | `r7_the_history_setting_decides_no_collision`, `random_interleavings_history_settings_decide_the_same` |
| No clock is read: the same calls with every commit dated before its parents decide the same trees. | `r2_no_clock_commit_dates_decide_nothing`, `random_interleavings_commit_dates_decide_nothing` |

The checks' record of who saw what (`Lineage`) is written by the save's replay, so on its own it cannot see a
wrong alignment; `save_aligned` and the harness's own record of each write call's bytes are the checks that do
not derive from the replay. The random runs also open files, make appends, writes at an offset,
inserts, deletes and moves (a third of the new lines repeating other lines' bytes: a blank or a brace), and
whole-file rewrites of a read taken earlier (often stale by then), and close them, so takes wait for
write-opens, opens wait for takes, and two opens append to one file at once; they unlink files and rename them,
to new paths or over another file. Main's clients die (a new one starts) as sandboxes' clients do. The random runs replay seeds 1 to
64 in CI (full lineage, squashed, and every record on a neighbouring git line). Half the time a write goes to the line a sandbox wrote last, so runs
often build "a sandbox edits over its own undecided line while main writes it" (finding 8). Run more with `AGENTFS_MODEL_SEEDS=500`, or one with
`AGENTFS_MODEL_SEED=<n>`. `random_interleavings_history_settings_decide_the_same` runs its own calls (saves, appends,
transactions, nested sandboxes, in four rounds drained between) twice, once per history setting, at seeds 1 to 16 in CI
(each seed runs and drains eight times) and at every seed the variables name.

## Coherence findings

An open or rejected finding has a test marked `#[ignore = "coherence finding N: <what fails>"]`; run them with
`--ignored` ([VERIFYING.md](VERIFYING.md) says what each is expected to show).

1. **git's own merge cannot tell decided from undecided (open, for git's merge).** A sync built by git's
   three-way merge, keeping the sandbox's side of a conflict: once a sandbox and main have each merged the
   other, git finds two merge bases and keeps a sandbox line main already decided against.
   `finding_1_git_merge_base_on_crossing_merges`, `random_interleavings_git_merge_base` (39 of 64 seeds; in 3
   of them git's line merge leaves one line identity twice in a file, so it cannot carry the line map either).
2. **Resolved: Squashed hid what a sync needs.** Without the graft (rule 7), a sync cannot tell that main's
   queue has taken a sandbox's line, so the sandbox keeps it forever. Costs: a clone needs `refs/replace/`
   too. Since rule 3e the graft names only winning saves, so a losing save at the end of a take is in no
   lineage of the parent's; the sync reads the parent's collision refs for it (rule 3d; with lineage alone,
   the planted bug of VERIFYING.md breaks `syncs_deliver` and convergence). `random_interleavings_squashed`.
3. **Resolved by the owner (2026-11-03): a losing change's clean lines.** Under per file, main's whole file
   won, so the sandbox's clean line in it was dropped and the sandbox then took main's older line over its
   newer one, which the checks counted as a stale revert. Rule 3e makes that the rule, for the whole change:
   the owner chose that a save or transaction lands whole or loses whole ("The whole file, right?"), so the
   clean line is decided lost, named by its change's collision (`By::Whole`), and the revert is that decision
   (`r3_a_committed_transaction_loses_whole_at_main`, the case the finding's test built). The per-line and
   per-file settings are gone, and with them `finding_3_per_file_keeps_clean_lines` and
   `random_interleavings_per_file_full_lineage`; the checks still count a revert that no real collision of
   its change decided as a stale revert.
4. **Resolved: the flicker.** Under "the incoming change wins" a sandbox showed x, then main's m, then x.
   The parent rule removes it, and `no_flicker` asserts it in every run.
5. **git reads commit dates when it walks a range.** `git rev-list A --not B` stops early by date, so the
   model reads ancestry only (`merge-base --is-ancestor`, parents, whole-history `rev-list`). An
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
9. **Resolved: open files.** A save is the program's write calls replayed at close on the tree's head (rule
   8), and an incoming change never writes a file open for writing (rule 9), so an append or a write at an
   offset never puts back a line it did not write. Only a truncate and rewrite of an earlier read does, as
   that program's write (`r8_a_stale_rewrite_overwrites_the_synced_line_and_history_keeps_it`). Its new
   line is written over the line it replaced, so `no_silent_loss` and `no_flicker` hold: the model's line
   is a new version holding the copied bytes.
10. **Resolved: a "slot" is a line identity (rule 10).** Fixed slots compared positions and bytes, which real
   files shift and repeat. A version is unique and written once, so its writing commit is exact: the model
   finds it by stepping to a parent that holds it. `git blame` cannot be used on the model's blobs: among
   repeated separator lines, git's diff showed a record whose neighbours moved as rewritten, and blame named
   the wrong commit under all three of git's diff algorithms. Whether blame is exact on a real line map is
   not tested.
11. **Open: two renames to one path before the parent decides.** The referee decides it (rule 11), but a
   sandbox that syncs first would show two files at one path. The model stops there, and the random runs
   rename only to new paths.
12. **Resolved: no write call says "insert" or "move".** POSIX gives byte writes, an `O_APPEND` flag and
   `ftruncate`, so a save cannot be told which lines a program meant; it aligns the bytes once at close
   (rule 10) and records that. Under the earlier model's insert, delete and move calls, nothing checked the
   alignment except through the replay that made it; `save_aligned` and `close_checked` now do. git's
   histogram diff can also put lines whose bytes did not change at the ends of a region (among repeated
   lines): the save keeps their versions, or every blank-line edit would version its neighbours too.
13. **Rejected: save files as written and let git merge them (2026-11-03).** A variant committed each file's
   bytes as written, merged with git's own merge (`merge-ort -X ours`) and rebased the sandbox onto its
   parent at every sync. It fails for two reasons that come from how git works, not from the variant's code.
   (a) A rebase makes the sandbox resolve a conflict itself, at the sync, so the parent's queue no longer
   decides alone (rule 3a): if the parent's side wins at the rebase, the sandbox drops its own line before
   the parent has decided it, and shows it again if the parent's queue then lets it through (343, 342 and
   348 of 500 seeds failed under full lineage, squashed and per file); if the sandbox's side wins, the sync
   withholds lines the parent has already decided (42 of 64 seeds, as the variant's builder measured it; not
   re-measured). (b) git merges by content: when the parent writes a line from A to B and back to A, the
   parent equals the merge base, so a sandbox's concurrent write lands over the parent's newer write and no
   collision is recorded, a silent loss. Its code cannot land, so neither can its tests: the variant is the
   unmerged branch `experiment/native-merge-variant` (commit `2bd9716`), whose ignored tests are tagged
   `coherence finding 14` (the variant numbers (a) and (b) 14a and 14b, and uses 13 for its own
   rename-pairing finding), among them `finding_14_the_parents_write_back_to_the_base_is_lost_silently`.
   The per-line referee stands (git-model.md §14).

## Costs

- A change loses whole for one line (rule 3e): a save's or transaction's lines that collided with nothing are
  dropped too, and the sandbox then takes the parent's version of them, newer lines of its own included. They
  stay in history, named by the collision. A program that wants a line to survive a collision elsewhere saves
  it in a separate close.
- The alignment can widen a save (rule 10): a byte-unchanged line between two changed lines of one changed
  region gets a new version with the same bytes (git's diff joins the region; only lines at its ends keep
  their versions), so a parent's concurrent edit of that line collides with it, and the whole save loses for
  a line its program never changed (seen building `r3_a_save_loses_whole_with_its_clean_lines` on six
  identical lines; the test now gives them distinct bytes).
- A move is a delete plus a new line (rule 10). Against the parent's concurrent edit of the moved line, the
  delete loses, so the whole save loses, its new line too: the parent's edit stays at the old place and the
  program's move is gone, kept in history (`r10_a_move_is_a_delete_plus_an_insert`).
- In a run of identical lines, which one an edit added, removed or changed is the diff's pick (it trims the
  common prefix first, so an inserted blank line is the last of its run): a concurrent edit of a neighbouring
  identical line can then land one line off from where the program's bytes put it, with no collision
  (`r10_repeated_identical_lines_align_by_the_diff`).
- A changed region pairs old and new lines by position. A wholesale rewrite (a reformat, a reindent) pairs
  unrelated lines, so a parent's concurrent edit of any of them makes the whole rewrite lose.
- Line maps keep every deleted version and unlinked file; no trim rule.
- A sync takes the parent's decided unlink, so a sandbox's undecided edit inside that file leaves its view
  before the parent's queue records the loss. An unlink or rename-over loses whole for one unseen line.
- A transaction's answer covers its commit into its sandbox only. Committed there, it can still lose whole at
  main's queue, later; that loss is a recorded collision (and its lines leave the sandbox at the next sync),
  not part of the commit's answer.
- Until main's queue decides, a sandbox shows its own line, not main's concurrent one, and keeps building
  on it. Once a sync has brought in main's concurrent line (kept out of view), the outcome is already fixed:
  the sandbox's line will lose, and so will every save that edits that line again before main's queue decides
  (each was made on the out-of-date copy), each whole. They stay in history and a collision names each, under
  Squashed too, but none reaches main (`r3_an_edit_over_an_undecided_line_still_collides`). This includes a
  save made after that sync, in the window after the sandbox closed its line and before main's queue decided
  it: by commit ancestry it descends from main's line, but per line (rule 3c's choice) it was written over its
  own, and loses.
- A program can hang on its own open file: a take is all or nothing, so a program that keeps file A open while
  it waits for a synced file B never gets B, if the sync that brings B also overwrites A. Linux has no such
  cross-file wait.
- A transaction commit is refused while the program holds its own files open (rule 5).
- A file held open for writing receives no syncs or landings while open: held open indefinitely, it drifts.
  And since a tree's queue is taken in order, its whole queue waits behind that take: a main client holding
  a file open stalls every sandbox's landing into main, not just that file's. A busy writer can starve a
  take (a new write-open is refused only while the take holds, not while it waits).
- A stale truncate and rewrite replaces a synced line in the parent and, by sync, in every sibling, with
  no collision, credited to the program's writer. Both lines stay in history.
- Under Squashed, main's history gains an empty commit for a take whose saves all lost; a losing save is in
  no graft. A loser before a winning save of its sandbox is still in main's lineage, through that winner's first
  parent; a loser after the last winner is not, so a reader of main's lineage alone does not see it: its collision
  ref does. A sandbox's sync
  reads the collision refs of its parent and the trees above (rule 3d).
- Losing commits and their collision refs are pushed and kept forever: any trim rule must keep who changed
  what when. The remote lags: a collision ref reaches it at Replication's next push.

## Left out

- **Files, in part.** The model creates no files (a rename goes to a new path or over an existing file), so
  not the atomic-save idiom's rename-over from a new file; no directories, hard links, or lines moved between
  files; no `O_TRUNC` open, path `truncate(2)` or `mmap`. Two renames to one path before the parent decides
  stop the model (finding 11).
- **Open files, in part.** An open file is private until close (close-to-open, the owner's decision): the
  model has no reads through an open file, and two writers' calls on one file meet at their closes, each
  applied to the head then. An offset write's position is read on the head at close, so the model's base for
  its alignment is that head, not the file as the program opened it (git-model.md §6). Appends never write over a line (checked); two offset writes to one
  line end with the last close's line, as two Linux writers at one offset end with the last write's.
- **Where the line map lives** in a real tree: the model keeps it in each file's blob (git-model.md §16).
- Binary files (the alignment diffs as text, so a NUL byte cannot hide a change, but no binary rule is
  designed); approvals and when landings run; integrator patches; restarts; space limits; history
  truncation.
