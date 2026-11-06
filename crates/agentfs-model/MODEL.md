# The landing model, in plain words

This crate runs the owner's landing rules on the real `git` program. Main and each sandbox (a "fork") are
git branches. Every save is a commit that names its one writer. A file is a byte buffer and a program's
calls are byte calls, as POSIX gives them (rule 8). Each line of a file gets an identity when a save first
records it, derived from the bytes at close, and keeps it through later edits around it (rule 10); a file's
name and existence are one more line (rule 11). Tests in
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
  stays stored ... that makes sense, because thats basically the design we want". Squashing itself is dropped:
  2026-11-05, "drop squashing"; "every sandbox commit stays stored" stands.
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
  object. That answer went with squashing (2026-11-05: "drop squashing"). Rule 3e replaces the per-line application the model had before, with no setting to keep it.
- 2026-11-04, on finding 24 (P3 made cooperating programs deadlock): "that sounds backward from normal fs
  semantics. In a normal file system, two programs can both open a file simultaneously and the “last writer
  wins” rule would apply. And “landings” are an agent’s concept, and as such, it seems low risk for them to be
  indefinitely delayed while an application is keeping the fs busy with real work. In the worst case, the
  landing never occurs in a child fork, and the parent is left to resolve the commits." So P3 is deleted: no
  program's open or path call ever waits on a waiting landing. His lock rule of 2026-11-03 stays: while a sync or
  landing is writing its held files (a bounded step, not a wait), a new write-open of them waits for it.
- 2026-11-04, on P2 (the prefix sync, a sync that delivers the parent's changes in order up to the first that
  touches a file the sandbox has open for writing): "agreed — that sounds like the rule we had discussed. Every
  commit can be landed independently (according to the rules of the commit vs transaction, etc)."
- 2026-11-05, on finding 27 (a delete that agrees with the parent's delete of the same line was dropped: no
  change, no record): "(a) agree -- why would it be dropped? it sounds like some optimization was injected which
  is causing problems". So such a delete is an ordinary change of its save (rule 3e).
- 2026-11-05, ledger rule R10.3 (two deletes of one line agree): "i want minimal "special cases", so if both
  deletes are recorded and then agree, my hope is that a merge reports no conflicts and life is good".
- 2026-11-05, on a transaction's commit while files are open: "if a file is held by program A, and program A tries to
  commit a transaction, then it should be able to acquire a file lock"; "additional I/O on that file must be
  prohibited/locked/deferred until the transaction is complete. else we risk a client trying to write the transaction
  while also writing the the file"; "deferred, obviously. thats what i mean by "lock" -- its about ordering the
  operations so that agentfs has a change to mutate the filesystem without a client fighting it". The commit goes ahead
  while that program holds files open; those files stay open and their writes save into the sandbox at close as an
  ordinary later save (ledger R5.3).
- 2026-11-05, on what a program has seen, ledger R3c.2, replacing two earlier readings: "i think we can only say that a
  client has had a chance to "see" a landing is if the file was closed when the landing occurred. that is -- only if the
  client can read the updated content through normal file I/O procedures."
- Still binding from earlier: forks stay, kept in step by syncing quickly; sandboxes are peers; a failed
  transaction is an implicit rollback.

## The rules

"Parent" means main for a sandbox, and the sandbox for a sandbox inside it. A "sync" brings the parent's
version into a sandbox; a "landing" brings a sandbox's saved change into its parent. Two lines "collide"
when each was written without seeing the other. Rows marked *choice* were decided here, not by the owner;
each is open for him to change.

| # | Rule | Source | Tests |
|---|---|---|---|
| 1 | Works like a Linux filesystem. A close is never refused and its save is in the tree at once, unless an open transaction has its file (rule 5: R5.3 defers it) (the model has no reads through an open file: what a writer reads back through its own open file is the target's matter, views.md); no collision holds a sync, landing or transaction commit (the only waits are rule 9's locks). Changes flow both ways without anyone asking. A save's first parent is its tree's head when it is made. What its program has seen is R3c.2 (the owner, 2026-11-05): a version it could read through normal file I/O, because the file was closed when that version arrived. Rule 9 makes the first parent agree with that: no sync or landing writes a file open for writing, so what a parent brought to a file arrived while it was closed; another program's save in the tree while it was open leaves this save the later close, final inside the tree (R9.16). | Owner | `r1_writes_never_wait_and_forks_sync_both_ways` |
| 2 | Order without clocks. Each tree has one queue, taken in order. Which of two lines is newer is read from git, never from a clock. | Owner | `r2_one_queue_fixes_the_landing_order`, `r2_no_clock_commit_dates_decide_nothing` |
| 3a | Every collision between a sandbox and its parent is decided in exactly one place, the parent's landing queue, and recorded once, there: one decision per losing change, kept by one collision ref, with a record under it for each file and cause it lost by. A sync never decides one: the sandbox keeps its own line until the parent's queue has decided it, then takes the parent's decision. So a sandbox's line changes at most once per collision: never its own, then the parent's, then its own again. | Owner | `r3_a_collision_is_recorded_once_at_the_parents_queue`, `r3_a_fork_never_flips_back_to_its_own_edit` |
| 3b | There, the parent's version wins a collision. The sandbox's losing line stays in history (kept under `refs/collisions/<n>`, which Replication pushes to the shared remote, so its writer and its place stay readable there), the collision names both lines, and the sandbox then takes the parent's line. A save made before the parent's sync arrived is concurrent with it and loses. At every level: a sandbox is the parent of the sandboxes inside it. | Owner | `r3_landing_collision_the_parent_wins`, `r3_a_save_before_the_sync_arrives_loses_to_the_parent`, `r4_nested_forks_follow_the_same_rules` |
| 3c | A line written on top of the parent's current line (its writer saw it) is not a collision; it replaces it. So a save made after a sync brought the parent's line wins upstream by lineage. What its writer saw is R3c.2: a version it could read through normal file I/O. A parent's line a sync kept out of the sandbox's view is not one: the sandbox showed its own line, so its next edit is written over that, not the parent's. The model reads this from git (the line maps and first parents), never from which commits a version merged. | Owner | `r3_fast_forward_when_the_base_is_current`, `r3_a_save_after_the_sync_wins_upstream_by_lineage`, `r3_an_edit_over_the_decided_line_replaces_it`, `r3_an_edit_over_an_undecided_line_still_collides` |
| 3d | A sync carries the parent's current decided version, never a stale one. *Choice:* the sandbox keeps exactly its lines whose writing commit the parent's queue had not decided when the sync read the parent's head: the parent's history does not reach it; when none is left and nothing of it waits in the parent's queue, the sandbox's branch moves to the parent's commit (a fast-forward). | Owner; how is a choice | `r3_a_sync_carries_the_parents_current_version`, `r3_a_sync_keeps_the_undecided_saves` |
| 3e | A change lands whole or loses whole at its parent's queue: if any of its lines collides and the parent wins, the whole change loses, its lines that did not collide included, and nothing of it lands; it stays in history, kept by its collision ref, and every line of it is named (a line that did not collide, `By::Whole`, for the parent's line that decided). An ordinary save is the whole file as written at one close (a rename-over, one call, its two name lines); a transaction's commit is all its files, and its answer tells the program (rule 5). Each later save is judged whole on its own. A loss by a file rule (rule 11) costs the whole change the same way, its other files included. Only a lost line costs a change: a line that is dropped (the parent has that version, or one written over it) costs nothing. A delete that agrees with the parent's delete of the same line is an ordinary version of its change (the owner, 2026-11-05; R10.3: the merge reports no collision and the line stays deleted): it applies when its change wins, is named when it loses, and is carried upward and judged again at the next parent. Within one change, lines are still judged per line (rules 3c, 10, 11): neighbouring lines of two different changes both apply. | Owner, 2026-11-03 | `r3_a_save_loses_whole_with_its_clean_lines`, `r3_a_change_whose_delete_agrees_lands_whole`, `r3_a_transactions_agreeing_delete_is_part_of_its_change`, `r3_a_nested_sandboxs_agreeing_delete_is_carried_upward`, `r3_a_later_save_after_a_losing_one_is_judged_on_its_own`, `r3_a_transaction_loses_whole_and_is_told`, `r3_a_committed_transaction_loses_whole_at_main`, `r3_a_file_rule_loss_costs_the_whole_change`, `r4_nested_changes_land_whole_or_lose_whole_at_every_level`, `r3_neighbouring_lines_merge_per_line` |
| 4 | Nothing waits behind a collision: every sandbox commit is taken, in order, and lands or loses whole. | Owner | `r4_nothing_waits_behind_a_collision` |
| 5 | Transactions, opt-in. Begin opens a new branch frozen at the sandbox's version; the program sees its own edits while the sandbox keeps syncing. Rollback, abandon or the client dying drops the branch. Commit lands the branch in the sandbox as a sandbox lands in its parent, whole or not at all (rule 3e, owner): the sandbox's line wins a collision (one a sync brought in since begin) and then none of the transaction applies; no collision refuses the commit; its answer says it lost and names every staged line (`TxnOutcome::Lost`). *Choice:* a transaction that lost still writes its merge commit, with the sandbox's tree, so history shows the decision. The commit then lands upstream like any change, whole or not at all. *Choice:* a file open before begin keeps writing to the sandbox. Nothing refuses a commit (R5.2 rejected): it goes ahead while programs hold files open, those files stay open, and their writes (a transaction's opens now write in the sandbox) save at close as ordinary later saves; an open of a file only a lost transaction made saves nothing (R11.3). R5.3: while a transaction is open, other I/O on its files in the sandbox (a save at close, so a truncate or rewrite too; an unlink; a rename onto or away from one) is accepted and returns at once, keeps its file locked as an open does (rule 9), survives a crash in the journal with its transaction (rule 12), and applies in the order made after the commit or rollback, a client's death included: a save as any later close, on the head then. Nothing waits on a program. *Choice:* a transaction's files are those it has saved or has open for writing; each deferred call's files join them, so a file's calls keep their order. The model has no reads, so a file it only reads is none of them; while deferred, a reader sees the file as it was. Only a program other than the client that began it (`Client::Outside`) can make I/O in the sandbox after begin, apart from a file open before begin. | Owner; the losing merge commit, what its files are, `Client::Outside` and a deferred call across a crash are choices | `r5_open_files_and_transactions`, `r5_a_commit_goes_ahead_with_the_programs_own_files_open`, `r5_an_open_of_a_file_only_a_lost_transaction_made_saves_nothing`, `r5_io_on_a_transactions_files_is_deferred_past_its_commit`, `r5_a_rollback_applies_what_it_deferred`, `r5_a_clients_death_applies_what_its_transaction_deferred`, `r5_a_crash_keeps_what_a_transaction_deferred`, `r5_a_deferred_save_holds_back_a_sync_of_its_file`, `r5_transaction_commits_clean`, `r3_a_transaction_loses_whole_and_is_told`, `r5_sync_before_or_after_the_commit_ends_the_same`, `r5_a_committed_transaction_then_lands_like_any_change`, `r5_transaction_rollback_and_client_death_leave_nothing` |
| 6 | *Choice:* every change traces back to one writer. A sync commit (the merge a sync makes when the sandbox has commits of its own the parent has not taken, so it cannot fast-forward) is labelled `Sync`, credited to `agentfs`, with the parent's commit second; it has no edits of its own: a merge carries only lines a parent holds. | Choice | `r6_a_sync_is_a_merge_with_the_parent_second`, every test |
| 7 | Full lineage: every sandbox commit, a transaction's included, is taken on its own and stays in main's history, a losing one included. Every sandbox commit stays stored. | Owner (2026-11-05: "drop squashing"; "every sandbox commit stays stored ... that makes sense", 2026-11-03) | `r7_every_sandbox_commit_lands_on_its_own_in_mains_history`, `r7_main_reaches_a_losing_commit_and_its_ref_keeps_it` |
| 8 | A save records what the program's write calls did, applied at close to the tree's head. POSIX gives byte calls only, and so does the model: an `O_APPEND` write (which the save knows), `pwrite` at an offset (a gap past the end reads as NUL bytes), `ftruncate`, and an `O_TRUNC` open (a truncate at open). A line is its bytes up to and including a newline, or the bytes after the last newline (a partial last line: the file has no final newline). An append's bytes go after the file's last byte as it is at close, another writer's concurrent append included: each line they start is a new line after the file's last line, so it writes over none; but if the file then ends in a partial last line, the appended bytes up to and including their first newline finish that line, a new version of it written over it, not a new line (*choice*: "foo" and an append of "bar" is one line, "foobar"). Offset writes change bytes; every line they leave unchanged keeps what the tree holds (a line a sync brought included). A truncate and rewrite replaces the whole file, so a stale rewrite really puts old content back, recorded as that program's write over what it replaced, the replaced version kept in history. Which lines the bytes changed is rule 10's alignment. | Owner; the append onto a partial line and the alignment are choices | `r8_concurrent_appends_never_overwrite_each_other`, `r8_an_append_after_the_sync_keeps_the_synced_line`, `r8_an_offset_write_elsewhere_keeps_the_synced_line`, `r8_a_stale_rewrite_overwrites_the_synced_line_and_history_keeps_it`, `r8_an_append_onto_a_partial_last_line_finishes_it`, `r8_concurrent_appends_onto_a_partial_last_line_collide`, `r8_split_and_overlapping_writes_make_one_version`, `r8_truncates_holes_and_an_empty_o_trunc` |
| 9 | A sync into a tree acquires a lock on every file it will overwrite: it waits until no client of that tree has the file open for writing, holds the files while it writes, and a new write-open of them waits until it finishes. Every wait ends on an event (a close, a client's death, a release), never a timer. *Choice:* the same for a landing into a tree, whose lock is every file its changes touch, fixed when it begins, a losing change's included (so a replan on a moved head never writes an unheld file: a save of a parent client cannot flip a losing change to a win mid-take); a take waits for all its files at once (all or nothing, so it never holds one file while waiting for another), and its queue waits behind it in order; a client's death (its end event, a sandbox's client or one of main's) closes its open files, saving their write calls as the kernel's close on exit would, and drops its waiting opens; the lock covers every call that changes a held file: `unlink` and `rename(2)` onto or away from it wait like a write-open and the take's release runs them (path `truncate(2)` too, not modelled). Progress (git-model.md §6): P1 (*choice*), an entry waits only for its own files and for earlier waiting entries it shares a file or a source with (one sandbox's entries, or the syncs, keep their order), so a long-held file holds back only the changes that touch it. No P3 (owner, 2026-11-04): no program's open, `unlink` or `rename(2)` ever waits on a waiting take, only on a take writing its held files; a waiting take is woken once none of its files is open, and a new open of one of them before its step keeps it waiting, as long as overlapping writers keep the file open (it may never land; the parent then resolves its commits). P2, the prefix sync (owner, 2026-11-04): a sync delivers the parent's commits in order (its first parents since the sandbox last had them), each one change as the parent took it (a save, a transaction's commit whole, a landing), up to, not including, the first whose delivery would overwrite a file open for writing in the sandbox (*choice*: the sync's own write, so a commit that leaves the sandbox's file as the sandbox has it, such as its own save come back, touches nothing), and holds back that commit and every later one, the decisions in them included, waiting for the files that commit adds to close. A held-back commit's decisions are held back with it in causal order (the review of 2026-11-04 at `d552b3d`, P0-1): a decision from a tree further up (main's, on a nested sandbox's line) is not delivered while the parent's commit that carried the losing change up is held back, so a decision never arrives before the change it decided. | Owner (the locks; no P3; P2); landings, all or nothing, death, path calls, P1 and what "touches" means are choices | `r9_a_sync_waits_for_an_open_writer_then_applies`, `r9_a_landing_waits_for_the_parents_open_writer`, `r9_a_landing_holds_the_files_of_its_losing_changes`, `r9_a_write_open_waits_while_a_sync_holds_the_file`, `r9_client_death_releases_its_locks`, `r9_a_main_clients_death_releases_its_locks`, `r11_an_unlink_or_rename_of_a_held_file_waits`, `r9_a_long_lived_main_writer_holds_back_only_its_own_file`, `r9_new_writers_postpone_a_waiting_take`, `r9_a_writer_holding_a_waited_file_is_not_made_to_wait`, `r9_cooperating_programs_never_deadlock`, `r9_a_held_file_holds_back_only_its_saves`, `r9_a_sync_delivers_up_to_the_first_change_to_an_open_file`, `r9_a_prefix_sync_holds_back_a_transaction_whole`, `r9_a_prefix_sync_delivers_no_upstream_decision_before_its_commit`, `r9_a_prefix_sync_holds_back_the_decisions_of_the_commits_it_holds_back`, `r5_a_decision_is_in_its_result_commit_with_its_tree` |
| 10 | *Choice:* at close the save derives line identity from the bytes: it aligns the program's bytes with the head's lines once, by git's histogram diff, and records the result beside the bytes (a line map): unchanged lines keep their versions; a changed region pairs old and new lines in order, each new one a new version of its old one, written over the version the writer saw; extra new lines get new identities; extra old ones get a hidden deleted version. A line's bytes include its newline, so adding or removing the final newline changes the last line; a line written in two calls is one version, and of two overlapping writes the later bytes count. An append needs no alignment (rule 8). A move is read as a delete plus a new line; among identical lines, which one changed is the diff's pick. From then on the referee (rule 3) and a sync compare versions of one identity, never positions or bytes: a line new to the parent applies; two deletes agree, and the incoming one applies (rule 3e, R10.3); of two inserts at one place the parent's comes first; an applied version is placed after the nearest line before it that both sides hold. A line a landing would put after the parent's unfinished last line (no final newline) collides with it and the parent wins; agentfs never adds a newline no program wrote (the owner, R10.4: "collide"). | Choice, asked for by the owner's "Go" | `r10_an_insert_shifts_lines_without_changing_their_identity`, `r10_concurrent_inserts_at_one_place_keep_both_the_parents_first`, `r10_a_delete_against_a_concurrent_edit_the_parent_wins`, `r10_a_move_is_a_delete_plus_an_insert`, `r10_repeated_identical_lines_align_by_the_diff`, `r10_a_line_after_the_parents_unfinished_line_collides`, `r10_an_append_over_the_synced_unfinished_line_finishes_it`, every random run (`placed_and_unlinked`) |
| 11 | *Choice:* a file's identity is fixed at create and kept by a rename (as an inode); its path and existence are its name line, decided by rule 3 like any line: a rename is a new version, an unlink a deleted one, a rename-over both in one save. Coupled with the content, the parent winning: an unlink loses whole if the parent has a live line its writer never saw; an edit of a file the parent unlinked loses, all of it; a rename and an edit both apply (the edit follows the file); two files renamed to one path, the parent's keeps it; until the parent's queue decides that, a sync into the sandbox keeps the sandbox's own name and shows the parent's file under the name it has in the sandbox (its previous name; a file the sandbox never had is not shown), so a tree never shows two files at one path (finding 11, a choice recommended on 2026-11-04 without objection); on a chain (the parent then gave that previous name to a third file) the same rule applies again, each file showing its name in the sandbox until no two clash (*choice*, the same rule's reading, review of `d552b3d`, P1-1). A rename-over is one save holding both name lines, so it applies or loses whole by rule 3e, with no rule of its own: the path its unlink half would free is no claim against its rename half, so when the unlink loses, the rename loses with its change (`By::Whole`, finding 25). A create (`O_CREAT` of a new path) makes a new identity at once, one save of its name line, with no lines; its content is a later save. The atomic save (git-model.md §7): a rename-over from a file new since its create (an unborn identity whose name version is still its create's, never renamed; read from git, so it does not depend on whether the parent has taken the create yet: finding 21) is a rewrite of the replaced file with the new file's bytes: the replaced file keeps its identity and merges per line, and the new file ends. A file created and ended inside one change (a transaction's atomic save) changed nothing anyone sees: none of its versions are in the change. Objects apart from identities (git-model.md §7): an open holds an object (the inode), fixed at open; a rename moves it, an unlink or a rename-over leaves it named by no path, an atomic save moves the new file's object to the replaced path; a close saves to the file its object is named by, and an open no path names (an orphan) saves nothing: its calls are dropped with an `OrphanDropped` event (writer, path, bytes). An orphan has no file open, so no take waits for it. | Choice, asked for by the owner's "Go"; creates, objects and the atomic save are choices (review point 3) | `r11_an_unlink_against_a_concurrent_edit_the_parent_wins`, `r11_an_unlink_after_the_sync_wins`, `r11_a_rename_and_a_concurrent_edit_both_apply`, `r11_concurrent_renames_the_parent_wins`, `r11_two_renames_to_one_path_keep_the_sandboxs_name_until_decided`, `r11_a_chain_of_renames_to_one_path_keeps_every_previous_name`, `r11_a_rename_over_applies_or_loses_whole`, `r11_a_losing_rename_over_loses_its_rename_with_its_change`, `r11_a_rename_over_whose_rename_loses_keeps_the_replaced_file`, `r11_an_open_file_follows_a_rename_and_an_unlinked_one_saves_nothing`, `r11_a_create_lands_like_any_save`, `r11_an_atomic_save_keeps_the_files_identity_and_merges_per_line`, `r11_an_old_writer_after_an_atomic_save_saves_nothing`, `r11_an_open_on_the_new_file_follows_it_through_the_atomic_save`, every random run (`placed_and_unlinked`) |
| 12 | *Choice* (git-model.md §3, review point 1): every write call is in its tree's journal before it returns, and publication stays at close (rule 1). A crash of every agentfs process saves each open from the journal as its close would have, before any tree is served again, and the opens end with the mount; a save names its open (`Agentfs-Open: <n>`), so the replay never saves an open twice (a crash between a close's save and the journal dropping its records included). Calls that never returned (a waiting open or path call) are dropped. A take in progress wrote nothing (one ref transaction): its entries go back where they were in its queue. An orphan's close records nothing in git, so a crash during it raises its `OrphanDropped` event again (events are at least once). An open in a transaction is journaled and replayed like any other, into the transaction's branch. The object table is journaled with the calls that move objects and rebuilt from the journal, never by file identity, so a dead handle stays an orphan; open numbers never repeat across restarts (git-model.md §3; the review of 2026-11-04, P1-d and P2-e). The model's crash is a process death: every journaled call and the object table survive; power loss before an `fsync` is not modelled. | Choice; AGENTS.md principle 1, no dated owner quote | `r12_a_crash_saves_every_open_from_the_journal`, `r12_a_crash_during_a_close_saves_the_open_once`, `r12_a_crash_saves_an_open_inside_a_transaction`, every random run (`crash_checked`; crashes come more often while a transaction has a file open, and CI's seeds assert that some do) |

## What must always hold

Checked after every step, in every scenario and random run (`check.rs`). They hold
for the modelled executions (the scenarios and the seeds run): evidence about these rules, not a proof, and
nothing about what the model leaves out (below).

| Invariant | Check |
|---|---|
| No tree silently loses a change: every version that reached a tree is still its line's version there, or replaced by a version written over it, or a delete that agrees (R10.3) with a delete of it, or of a version written over it, that entered that tree or one upstream and no collision names lost, or named lost by a collision in that tree or one upstream. | `no_silent_loss` |
| No stale version reverts a newer one, unless a collision decided against the newer one: its lost version and winner really collided (a lost unlink gives the file back), or it is a line of a change that lost whole because another of its lines really collided (rule 3e: a losing save's clean line goes back to the parent's). A version lost by no real collision of its change is still a stale revert. | `no_silent_loss` |
| No flip-flop: a version, once replaced in a tree, never shows there again, unless a collision decided against the version replacing it. | `no_flicker` |
| The parent's version wins every collision, decided only at the parent's queue: read from git, the result is that tree's own landing (or transaction) commit and the losing commit was made in a sandbox below it; the result holds no lost version, keeps the tree's own version of every lost line and holds every winner; the two versions really collided (for a file rule, at least one pair; for a line lost only with its change, a real collision of that change); the losing commit stays reachable under its ref; and the decision is in git: the take's own result commit holds it (a trailer `Agentfs-Collision` per record), and that commit's `Agentfs-Tree` names the tree it belongs to (R5.1, R9.9). | `parent_wins` |
| No partial save or transaction ever reaches a parent: each change a take judged lost (named by a collision of that take) or won, never both; every version its own edit set (read from git) is, if it lost, absent from the take's result and named lost, and if it won, in the result (or a version written over it). | `changes_whole` |
| Each take's change and whether it won, re-derived from git alone, match the model's record (`Landed`): the harness rebuilds every tree's landing queue from the branch moves it observes, reads which change a take judged (the second parent or the fast-forwarded commit), and the losers from the collision refs that step created; a change won or lost, never both; a take whose change lost leaves the tree unchanged; every new collision ref belongs to a take. | `interleavings.rs` `gitledger` |
| A sync records no collision, and carries the parent's head as it was when its take began (read before the step, not from the model's record), or under P2 one of that head's first parents (`prefix_cut`). What the parent had decided then is read from git: its history. | `step_checked`, `syncs_deliver` |
| Every sync delivers the parent's decided version: the sandbox holds the parent's version of every line except its own versions the parent's queue had not taken when the sync's take began, or had taken only in takes whose result commit is not in the commit a prefix sync delivered (P2, read from git), and the name of a parent's file at a path the sandbox's own undecided rename holds (finding 11); it drops none of the parent's lines. | `syncs_deliver` |
| Nothing else waits: every close and commit returns at once; every queue step takes its queue's first entry and leaves the rest in order, or records that it waits. | `interleavings.rs` `take` |
| No sync or landing writes a file open for writing in that tree, or one it did not hold. | `step_checked` |
| A landing, taking or waiting, locks every file its changes touch, a losing change's included, read from git (not only what its plan writes). No write-open succeeds while a take holds the file; no lock outlives its holder (a sandbox's client or one of main's, its transaction, a tree); every lock wait ends on an event: a waiting take waits only while one of the files it waits for is still open for writing, a waiting open only while a take holds its file; none is left once writes stop. | `locks`, `check_converged` |
| No program's open ever waits on a waiting landing (owner, 2026-11-04): every call that waits (an open, `unlink`, `rename(2)`) waits in a tree where a take is writing, on a file that take holds; judged by the harness's own record of each take in progress (from the queue step that began it to its finish, its files read from git for a landing), not the model's waits; the model's own reading too (`locks`: a waiting call waits only on a hold). Cooperating programs therefore never deadlock (finding 24, `r9_cooperating_programs_never_deadlock`). | `interleavings.rs` `Oracle::no_wait_on_waiting`, `locks`, `run_all` |
| A sync never delivers past the first change to an open file (P2, owner, 2026-11-04): what a sync delivers is the parent's head or one of its first parents, read from git, so it delivers the parent's commits in order and none after its cut; its take overwrites no file open for writing in the sandbox (so no change to an open file is delivered); and if it holds commits back, the first it holds back touches an open file (its own change or a collision record in it names the file, and its version differs from the sandbox's: a necessary condition, since the sandbox may keep its own undecided lines over it). Across the CI seeds a sync delivers a prefix and holds the rest back. A decision reaches a sandbox only with its commit (causal order): `no_silent_loss` and `no_flicker` accept a move by a collision only once it is delivered, read from git (the parent's commit carrying it, its result for the parent's own, the parent's commit that carried the losing change up for one further up, is reached by a commit a sync delivered into the sandbox, or the parent's head no longer reaches it). | `step_checked` (`prefix_cut`, and the finish's write), `delivered`, `run_all` |
| A transaction ends as the program was told, whole: a committed one has every staged line in the sandbox (a staged delete, or a line the transaction deleted later, may meet the sandbox's own delete: finding 18) and no collision; one that lost has none of its versions in the sandbox, a real collision, and every staged line named in the answer and by a collision; rolled-back ones leave no version in any tree or history (bytes a program read and writes back later are its new write, rule 8). | `transactions_end_as_told`, `discarded_leave_no_trace` |
| R5.3: while a transaction is open, no save reaches its sandbox on a file it has saved, read from git; each deferred call is the harness's own (its reading of a transaction's files, from git and its object table), applies in that order once it ends, and the sandbox then holds the oracle's bytes; a deferred call lives only behind its open transaction. | `transactions_undisturbed`, `locks`, `interleavings.rs` `Oracle::defers`, `apply_deferred_checked` |
| Every change traces back to one writer. | `one_writer` |
| An append writes over no line, unless it finishes a partial last line: read from the save's blobs and the byte calls its program made (recorded as made, not the replay's output), the appends after a save's last other call wrote the file's tail; every line starting in those bytes is a new line after every line of the parent, and a line the appends finished starts with the partial line's bytes and, in a save of appends only, is the parent's last line, one version on. | `appends_exact` (in `one_writer`) |
| A save's line map is a valid alignment, read from its blob and its first parent's alone: every line identity the parent holds is still there, in the same order, unchanged or one new version written over the parent's; every other line is new and live. | `save_aligned` (in `one_writer`) |
| Bytes: an independent oracle (the harness's own byte array per tree, per open transaction and per file, and each open's calls, updated by the same calls by its own byte code) says what every close saves: the model's save holds exactly those bytes, or nothing if the file was unlinked meanwhile; after every step but a referee's (a queue step, a transaction's commit), every tree holds the oracle's bytes, and a take changes only the tree it finished in. | `interleavings.rs` `Oracle`, `close_checked`, `take` |
| Rule 10's placement and rule 11's unlinked file, read from git in every take: a new line a winning change added goes after the nearest line before it in the change's file that the result holds, past only lines the change never saw, and no line the parent added that the change never saw directly follows it (of two inserts at one place the parent's comes first); a change that edits a file the tree had unlinked (and does not unlink it too) loses, by a file rule. Across the CI seeds both really happen. | `placed_and_unlinked`, `interleavings.rs` `run_all` |
| Progress, P1: a queue step acts on the first entry no earlier waiting entry (or one waiting behind it) shares a file or a source with, the files read from git (a landing's own changes, a sync's every file its tree and the parent's head differ in), and takes it alone; a landing waits only when one of its own change's files is open (the harness's own object table), so a held file holds back only the saves that touch it; an entry free to go is offered as a step, so an unrelated change never waits on another file's writer; read from git alone, no take ever takes a sandbox's commit ahead of an earlier one of the same sandbox. | `step_checked`, `locks` (`p1_first`), `interleavings.rs` `take`, `gitledger` |
| Objects: every close saves what an independent object table (the harness's own: each file's object per target, each open's object, moved by its own reading of each path call) says: the bytes of the live file its object is named by, or, for an orphan, nothing and exactly one `OrphanDropped` event naming its writer, path and bytes; across the CI seeds atomic saves and orphans really happen. | `interleavings.rs` `Oracle`, `close_checked`, `dropped` |
| Journal: every save from an open names it, once; no open is saved twice across all history, crashes included; after a crash every granted open's calls are in the trees, as the byte oracle closes them, and no open survives. | `one_writer`, `interleavings.rs` `crash_checked` |
| A rename-over applies whole: a file unlinked by one has the renamed file at that save's name or a later one (in a sandbox, unless the name it holds for that file is not its parent's, or is a sandbox's version main has not taken: finding 17). | `rename_overs_whole` |
| Every losing change is replicated, eventually, and stays: once Replication's push (`Step::Replicate`, its own step, never a referee's; due on every move of main and every collision) has drained, the commits of the remote main's history hold exactly the run's collisions, each in its result commit, and each is read on the shared remote alone: its collision ref names the losing commit, and each lost version is in it and was written by a commit naming that version's writer. | `losers_pushed` (in `check_converged`) |
| Trees converge when writes stop: every sandbox equals main. | `check_converged` |
| Full lineage, read by ancestry alone: a tree's history reaches every change it took, won or lost. Across the CI seeds, changes really lose whole with clean lines in them. | `full_lineage`, `interleavings.rs` `run_all` |
| No clock is read: the same calls with every commit dated before its parents decide the same trees. | `r2_no_clock_commit_dates_decide_nothing`, `random_interleavings_commit_dates_decide_nothing` |

The checks' record of who saw what (`Lineage`) is written by the save's replay, so on its own it cannot see a
wrong alignment; `save_aligned`, `appends_exact` and the byte oracle are the checks that do not derive from the
replay. The random runs open files (a sixth with `O_TRUNC`) and make byte calls: `O_APPEND` writes, some of a
partial line; raw `pwrite`s at any offset, some past the end (a hole); split writes (a line's bytes in two calls)
and overlapping ones; `ftruncate`s shrinking and growing; and the byte calls a program makes for a line written in
place, inserted, deleted or moved (a third of the new lines repeating other lines' bytes: a blank or a brace), or
a whole-file rewrite of a read taken earlier (often stale by then). They close them, so takes wait for
write-opens, opens wait for takes, and two opens append to one file at once; they unlink files and rename them,
to new paths or over another file. While a take holds files in a tree, a client of that tree often opens, saves,
unlinks or renames a file the take's changes touch, mostly on a line in contention (one the take's change or the
tree's head set over the change's base, read from git), often removing it: the pattern the 2026-11-03 review found
by hand. While an entry waits, a client of that tree often opens, saves, unlinks or renames one of its files (each
open postpones it),
and a sandbox of that tree often saves a file it does not lock, followed by a queue step there (P1's overtaking). Main's clients die (a new one starts) as sandboxes' clients do. They create files and make atomic saves (a new
file written and renamed over a file), half of them while an old writer of the replaced file stays open and
writes on; every agentfs process crashes now and then, half the time during a close, and more often while a
transaction has a file open. A client blocked in a call
that waits makes no other call until it returns (it may still die), as a program does. The random runs replay seeds 1 to
64 in CI (each record on its own hunk, and every record on a neighbouring git line). A second generator, `rounds` (CI: seeds 1 to 16), makes
many calls with no queue step between them, drained to convergence each round, so a change meets many others queued
behind it (it found findings 25 to 28; it went with the settings comparison and came back on 2026-11-05). CI's seeds
assert that takes really wait for write-opens and opens for takes. Half the time a write goes to the line a sandbox wrote last, so runs
often build "a sandbox edits over its own undecided line while main writes it" (finding 8). Run more with `AGENTFS_MODEL_SEEDS=500`, or one with
`AGENTFS_MODEL_SEED=<n>`.

## Coherence findings

Findings written while the model had two history settings name seeds of a squashed run or of a settings comparison.
Both went with squashing (the owner, 2026-11-05: "drop squashing"), so those seeds replay nowhere now.

An open or rejected finding has a test marked `#[ignore = "coherence finding N: <what fails>"]`; run them with
`--ignored` ([VERIFYING.md](VERIFYING.md) says what each is expected to show).

1. **git's own merge cannot tell decided from undecided (open, for git's merge).** A sync built by git's
   three-way merge, keeping the sandbox's side of a conflict: once a sandbox and main have each merged the
   other, git finds two merge bases and keeps a sandbox line main already decided against.
   `finding_1_git_merge_base_on_crossing_merges`, `random_interleavings_git_merge_base` (40 of 64 seeds with
   the calls of programs outside a transaction, 2026-11-05; 34 with no P3 and the prefix sync, 2026-11-04; 35 after the fix pass of the review of 2026-11-04; 36 with creates, crashes and the progress rules; 32 with the byte calls alone; before them 39, in 2 of which git's line merge left one line identity twice in a file, so it
   cannot carry the line map either).
2. **Gone with squashing (the owner, 2026-11-05): a squashed history hid what a sync needs.** A sync could not tell
   that main's queue had taken a sandbox's line, which took a graft to read back. Full lineage has no such gap.
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
8. **Resolved: "saw" cannot be read from commit ancestry (R3c.2).** A sync that keeps a sandbox's undecided line x
   still makes main's commit (with m) a parent of the sandbox's next commit. Read by commit ancestry, the
   sandbox's next edit y over x looks "made on m", so y replaces main's winning m with no collision: a
   silent loss. The program could never read m (R3c.2, the owner, 2026-11-05): the sandbox showed x. The model reads
   the versions y was written over (x, and what x was written over), so y collides with m and main wins. The checks
   read it the same way; by commit ancestry they could not see this loss. `r3_an_edit_over_an_undecided_line_still_collides`.
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
11. **Resolved (2026-11-04): two renames to one path before the parent decides.** The referee decides it (rule
   11), but a sandbox that synced first showed two files at one path, and the model stopped there. Rule (a
   choice, recommended in the overnight report of 2026-11-04 without objection): the sandbox keeps its own name
   until the parent's queue decides it, and shows the parent's file under the name it has in the sandbox
   (`keep_own_names`); `syncs_deliver` allows exactly that name line to differ from the parent's.
   `r11_two_renames_to_one_path_keep_the_sandboxs_name_until_decided` reproduces it (a rename and a rename-over);
   seeds 253 (every main random run) and 216 (the settings comparison) of 1 to 500, which stopped there, pass.
   Cost: until the decision the sandbox shows the parent's file at its old path, and a file the sandbox never had
   is not shown at all.
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
14. (Number used by the unmerged variant branch of finding 13 for its own findings; not used here.)
15. **Resolved by the owner (2026-11-05), at a landing: a line after a partial line (2026-11-04, the byte oracle).** With bytes modelled, a merge can put a
   line after another writer's partial last line: main appends `}` with no final newline while a sandbox appends a
   line; rule 10 puts the parent's first, and the two line identities then make one line of bytes (`}n9`). The
   alignment, which diffs bytes by git line, then paired the wrong records, and a later truncate saved bytes its
   calls never left (seed 2 of the settings comparison; the oracle reported it). The owner chose (2026-11-05,
   R10.4: "collide"): the sandbox's line collides with the parent's unfinished last line and the parent wins, and
   agentfs never adds a newline no program wrote (`r10_a_line_after_the_parents_unfinished_line_collides`). Reading
   the line as terminated, this finding's first answer, is gone (R10.2). Findings 29 and 30 are what that leaves open.
16. **Resolved: Replication was due only after a collision (2026-11-04).** The push was an event of a recorded
   collision only, so a run without one never pushed main. Replication is now due on every
   move of main too.
17. **A sandbox can show its parent's rename-over in part, nested sandboxes included (2026-11-04, seed 124).** Main
   renames `d` over `b` while a sandbox renames `a` over `d`. A sync keeps the sandbox's undecided unlink of `d`
   and takes main's unlink of `b`, so the sandbox shows neither file `b` nor file `d` (only `a`, at path `d`) until
   main's queue decides (the cost of rule 3d's keep-undecided rule on a one-call change, already excused in the
   sandbox). A sandbox nested in it
   fast-forwards to that state and holds its parent's name for `d`, which the check's exception (its parent's
   name only) did not excuse: a check bug, not a model change. The exception now also covers a sandbox's version
   main has not taken. Found at seed 124 of 1 to 500 under every random run (the harness before the during-take
   bias); with the fix, that harness passes seed 124.
18. **Resolved: a check was stricter than rule 3e (2026-11-04, seeds 144 and 398 of the widened comparison).** A
   transaction writes a line, then deletes it in a later save of the same transaction, while the sandbox deletes
   it too. At commit the transaction's version of that line is its delete, which agrees with the sandbox's and
   costs nothing (rule 3e), so the answer does not name it; `transactions_end_as_told` still asked the answer to
   name the earlier, superseded version. A check bug, not a model change: the check now judges each staged edit
   by the transaction's last staged version of its line. An answer that omits a line still fails three
   scenarios (planted). Since the owner's decision of 2026-11-05 (finding 27) the transaction's delete applies and
   is named like any version, so the check's agreeing-delete clauses are deleted; judging by the last staged
   version stays.

19. **Resolved by the owner (2026-11-05): an agreeing delete was dropped and followed the delete it agreed with
   (2026-11-04, seed 47 of the squashed run).**
   A transaction deletes a line the sandbox has deleted too; at commit the two deletes agree and the transaction's
   costs nothing (rule 3e), so its answer is `Committed` and no record names it. The sandbox's delete then loses
   whole at main (another line of its save collided), the line comes back, and the program's delete is gone with no
   record naming it. Nothing this round added is involved: the check (`no_silent_loss`) was stricter than rule 3e
   and now excuses exactly that case (a dropped delete whose agreeing delete a collision names lost); without the
   excuse the new scenario fails (planted). The owner rejected the drop ("(a) agree -- why would it be dropped?",
   2026-11-05): the transaction's delete now applies in the sandbox, part of its change, and is carried upward
   with the transaction's commit, judged on its own (rule 4); when its change loses, the answer names it. The
   check's excuse is deleted. `r3_a_transactions_agreeing_delete_is_part_of_its_change`.
20. **Gone with P3 (2026-11-04): P3 as designed deadlocked a program waiting on itself.** P3 made a new
   write-open of a file a waiting take locks wait behind it, so a program holding file A open (the take waits for
   A) that opened B (also the take's) blocked and never closed A; the model let such a call go ahead
   (`Handle::jumped`). With P3 deleted (finding 24) no call waits behind a waiting take, so the exemption went
   too; `r9_a_writer_holding_a_waited_file_is_not_made_to_wait` still shows the program is never made to wait.
21. **The atomic-save rule as worded depended on timing (2026-11-04).** git-model.md §7 said a rename-over is an
   atomic save when its source is "a file the parent has never had (created since the fork last synced)". An
   editor writes the temporary file, closes it (a save, which queues a landing) and renames it: whether the parent
   has taken the create by then is a race, so the same calls would sometimes merge per line and sometimes replace
   the file's identity. The model reads it from git instead: the source is new since its create (its name version
   is still the one its create wrote), whatever the parent has taken; in the parent the rename then lands as a
   rewrite plus the new file's end, merged per line. A choice.
22. **An unlinked file's open no longer holds back a take (2026-11-04).** Before objects, an open of a file unlinked
   in its tree still counted as an open of that file, so a take writing it waited for its close. With objects
   (git-model.md §7) no path names that open's object and its close saves nothing, so it has no file open and no
   take waits for it; an unlink, rename-over or atomic save in a tree wakes a take that waited for it.
23. **Gone with P3 (2026-11-04): P3's "opens granted before it began waiting" was per file** (seeds 167 to 171
   and 426 of 500): under squashing a waiting landing's run grew and its lock set gained files after it began
   waiting. With no P3 a waiting take waits for every open of its files (`Waiting::before` went), so there is
   nothing to protect per file.
24. **Resolved by the owner (2026-11-04): P3 made a call wait on another client's close, so cooperating
   programs deadlocked** (the review's P0-1). Main's client m1 kept `log` open; a sandbox's change to `log` waited
   for m1's close; m1 started m2 and waited for it; m2's open of `log` waited behind the take under P3: m2 for the
   take, the take for m1, m1 for m2. The owner deleted P3 ("it seems low risk for them to be indefinitely delayed
   while an application is keeping the fs busy with real work", owner's words above): no program's open ever
   waits on a waiting landing. The finding's test is now `r9_cooperating_programs_never_deadlock` and passes;
   `Oracle::no_wait_on_waiting` checks every run. Cost: a file kept open by overlapping writers postpones its
   take for as long as they overlap, possibly forever; a sandbox's
   landing that never happens leaves its commits for the parent to resolve.
25. **Resolved (2026-11-04): a rename-over losing by a file rule named its rename half lost "by File"** (seeds
   287 and 436 of 1 to 500 of the settings comparison). A sandbox renames `b` to `e2` (save 1), then `c` over `e2`
   (save 2: `c` named `e2`, `b` replaced). Main edited `b` unseen, so save 2's unlink of `b` loses by the file rule
   and save 2 loses whole; but the path rule then saw `b` keep `e2` and recorded `c`'s rename as lost `By::File` to
   save 1's name of `b`. The
   model was wrong, not the check: the path a rename-over's own unlink would free is no claim against its rename
   half (rule 3e: one call, whole), so `c`'s rename now loses with its change (`By::Whole`).
   `r11_a_losing_rename_over_loses_its_rename_with_its_change` reproduces it; both seeds pass.
26. **Resolved by the owner (2026-11-05), first in the checks (2026-11-04): a stale revert in a nested sandbox after a sibling sandbox's client
   died** (seed 332 of 1 to 500 of the settings comparison). Under full lineage a nested sandbox f3 deletes a line;
   its parent f2 deletes it too, in a save that loses whole at main; f2 then takes f3's save, whose delete agrees
   with f2's and is dropped (finding 19), and later syncs bring the line back into f3. That is finding 19's rule
   (a dropped agreeing delete follows the delete it agreed with), which the "entered" half of `no_silent_loss`
   already allowed; its move half, the stale-revert check and `no_flicker` did not, so they called it a stale
   revert. They were taught to allow it the same way. The death only steered the seed there. Fixing it uncovered
   finding 27. With the owner's decision of 2026-11-05 f3's delete applies in f2, part of its change, and the
   check excuses are deleted. `r3_a_nested_sandboxs_agreeing_delete_is_carried_upward` runs the calls.
27. **Resolved by the owner (2026-11-05): a nested sandbox's dropped agreeing delete was decided by the history
   setting (2026-11-04, seed 332 of the settings comparison).** With finding 26's calls, full lineage judges f2's landing of f3's save at main as
   its change in f2, where the dropped delete is no change, so the line comes back in main; the squashed history
   judged the saves its graft named, so f3's delete, written over main's line, applied in main. The owner chose that
   the delete is not dropped ("(a) agree", 2026-11-05): it applies in f2 and is carried upward, and it applies in
   main, written over main's line. `r3_a_nested_sandboxs_agreeing_delete_is_carried_upward` runs the calls.
28. **Resolved by the owner's rule R10.3 (2026-11-05): an applied agreeing delete replaced the parent's delete,
   which the checks called a loss (found removing R3e.3; seed 15 of 1 to 64 of the settings comparison, seeds 44
   and 60 of 1 to 64 of the squashed run; seed 221 of 1 to 500 of the main random runs).** Main edits line 1 and then deletes it; a sandbox deletes line 1
   concurrently: its delete applies, and `no_silent_loss` called main's edit, which entered main, lost. Under
   squashing, a save's edit lost to main's delete and a later save of the same take deleted the line over that
   edit, and `parent_wins` found main's winning delete missing from the result; that reading went with squashing. A sandbox that held
   main's edit and syncs after both deletes moves from the edit straight to the agreeing delete (seed 221). R10.3: two deletes of
   one line agree, both are recorded, no collision is reported, the line stays deleted. So the checks read it:
   main's edit is not lost (main's own delete, written over it, replaced it first, and the applied delete agrees
   with that one; in the sandbox, that delete entered the tree upstream). The model is unchanged.
   `r10_two_deletes_of_one_line_agree_over_an_unseen_edit`, `r10_a_later_delete_agrees_with_the_winning_delete`.

29. **Open, for the owner (2026-11-05): a sync keeps a sandbox's undecided line after the parent's unfinished last
   line.** R10.4 decides it at the parent's queue, and a sync decides nothing (R3a), so until then the sandbox holds
   main's `}` and its own `n9`: with no reading as terminated, two line identities on one line of bytes, `}n9`. The
   alignment diffs bytes by git line, so a later non-append save in that window may pair the wrong records (finding
   15's first symptom; read from the code, seeds 1 to 500 show no failure). `finding_29_a_sync_keeps_a_line_after_the_parents_unfinished_line`.
30. **Open, for the owner (2026-11-05): a transaction's line after its sandbox's unfinished last line.** R10.4 is about
   a parent and a sandbox, so it does not apply inside one tree, and at commit the two identities make one line of
   bytes, as in finding 29. `finding_30_a_transactions_line_after_the_sandboxs_unfinished_line`.

31. **Open, for the owner (2026-11-05): an agreeing delete of a file a rename-over named (seed 309 of 1 to 500, every main
   random run).** Main renames d over c and then unlinks d; a sandbox unlinks d. The two deletes agree and the sandbox's
   applies (R10.3), so d's name is a version not written over the rename-over's, and `rename_overs_whole`, which knows
   nothing of R10.3, calls it half a rename-over. The tree is what R10.3 says. No check was changed (VERIFYING.md,
   "Fix passes"). `finding_31_an_agreeing_delete_of_a_rename_overs_file`.
32. **Open, for the owner (2026-11-05): a landing leaves a line unfinished before the parent's next line (seed 128 of 1 to
   500 of `rounds`, no transaction involved).** A sandbox cuts a file to one unfinished line; main edits a line that save
   deleted, so it loses whole (rule 3e); the sandbox's next save, written over its own first line, lands on its own (rule
   3c), and the parent's next line then follows it: `n6n5`, bytes nobody wrote, two line identities on one line of bytes.
   A later non-append save then pairs the wrong records, and saves bytes its calls never left (the oracle reports it),
   finding 15's first symptom. R10.4 and R10.6 say what happens to a line after an unfinished last line; none says what
   happens when the line a parent keeps later lines after becomes unfinished. `finding_32_a_landing_leaves_a_line_unfinished_before_the_parents`.
33. **Open (2026-11-05): the model does not match the owner's transaction answers.** `Client::Outside` and binding a
   transaction to a client have no rule behind them (ledger R5.4). `txn_files` also counts files open for writing, and a
   deferred call stays behind a surviving transaction at a crash; R5.3 says otherwise. Not changed, because the integration's
   API is undefined (git-model, Questions for the owner). No test records this yet.

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
- Appends to a file with no final newline are not concurrent-safe: each finishes the last line, a version of it,
  so two trees appending to such a file collide, the parent winning and the sandbox's save losing whole
  (`r8_concurrent_appends_onto_a_partial_last_line_collide`). Appends to a terminated file never collide.
- A line a sandbox appends after the parent's unfinished last line loses to it, and the save with it, whole (R10.4):
  a concurrent append is lost for a missing newline, though it stays in history.
- Replication pushes on every move of main (finding 16), not only after a collision: one push per move unless
  Replication batches moves that land before its next step, as the model's eventual `Step::Replicate` does.
- Line maps keep every deleted version and unlinked file; no trim rule.
- A sync takes the parent's decided unlink, so a sandbox's undecided edit inside that file leaves its view
  before the parent's queue records the loss. An unlink or rename-over loses whole for one unseen line. A sync
  that keeps a sandbox's undecided rename can show a parent's rename-over in part (finding 17): until the
  parent's queue decides, the sandbox, and sandboxes inside it, may show neither file.
- A transaction's answer covers its commit into its sandbox only. Committed there, it can still lose whole at
  main's queue, later; that loss is a recorded collision (and its lines leave the sandbox at the next sync),
  not part of the commit's answer.
- Until main's queue decides, a sandbox shows its own line, not main's concurrent one, and keeps building
  on it. Once a sync has brought in main's concurrent line (kept out of view), the outcome is already fixed:
  the sandbox's line will lose, and so will every save that edits that line again before main's queue decides
  (each was made on the out-of-date copy), each whole. They stay in history and a collision names each,
  but none reaches main (`r3_an_edit_over_an_undecided_line_still_collides`). This includes a
  save made after that sync, in the window after the sandbox closed its line and before main's queue decided
  it: by commit ancestry it descends from main's line, but its program could never read that line (R3c.2), and it
  loses.
- A landing waits for an open writer of every file its changes touch, even a file whose change loses (rule 9):
  the lock is fixed when the take begins, before it is known that the take writes nothing there.
- A program can hang on its own open file: a take is all or nothing, so a program that keeps file A open while
  it waits for a synced file B never gets B, if the sync that brings B also overwrites A. Linux has no such
  cross-file wait.
- While a transaction is open, a save, unlink or rename of its files by another program does not show (rule 5, R5.3), so
  that program's next read of the file misses its own close. A file with a deferred call counts as open, so a sync or
  landing of it waits for the transaction to end, as long as the program keeps it open.
- A commit that goes ahead with a program's offset writes open leaves them to apply at close to the commit's head, not
  the bytes the program read (rule 8, as any two writers in one tree: R9.16).
- A file held open for writing receives no syncs or landings while open: held open indefinitely, it drifts.
  Under P1 only the entries sharing a file or a source with the waiting one wait behind it. Under P2 a sandbox's
  open file holds back only the parent's commits from the first that touches it on, but all of those, even the
  ones touching no open file (a prefix, so no change applies out of order); a commit touching an open file holds
  back its whole change (a transaction's other files too). With no P3 (the owner's decision) a busy file can
  postpone its landing forever: overlapping writers that never all close keep the take waiting; a sandbox's landing that never comes leaves its commits for the parent to resolve. A program's
  open never waits on a waiting take, so programs never deadlock on one.
- Each queue entry keeps its file set, and a queue step scans the entries ahead of it (entries times files). The
  parent's history lists landings in the order they landed, not the order they queued.
- An orphan's write calls leave no history (the `OrphanDropped` event names them), and after a crash during its
  close its event comes twice. A file created and ended in one transaction leaves only the transaction's own
  commits; the commit into the sandbox carries none of its versions.
- The model's creatable files are a fixed pool of three identities, unborn in main's first commit (a modelling
  device: a real create mints a new identity).
- A stale truncate and rewrite replaces a synced line in the parent and, by sync, in every sibling, with
  no collision, credited to the program's writer. Both lines stay in history.
- Losing commits and their collision refs are pushed and kept forever: any trim rule must keep who changed
  what when. The remote lags: a collision ref reaches it at Replication's next push.

## Closed from the review of 2026-11-03

The last whole-model review (at `6454c47`) ranked these below P0, and they stayed open through the P0-only fix
pass. Closed 2026-11-04:

- **"Read from git" now rests on git where it can.** Each take that decides a collision records it in its own result
  commit (R5.1, R9.9; first built as a notes ref, removed 2026-11-05: the owner, "this cannot be a flat git history
  where any arbitrary checkout is a "correct" representation -- each commit is contextual to a branch/fork/remote"), a
  trailer per record (tree, file, cause, losing change, ref, lines, lost and won versions). `parent_wins` holds every
  collision against its result commit; `losers_pushed` reads the remote main's history, not the model's list. Still from
  the model's record, each checked against git elsewhere: `changes_whole` and `full_lineage` take changes and
  winners from `Landed` (which `gitledger` re-derives from git in the random runs), and the ledger's fork topology
  comes from the harness's `create_fork` calls.
- **Rules 10 and 11 are checked in the random runs.** `placed_and_unlinked` checks rule 10's placement (two inserts
  at one place, the parent's first) and rule 11's "an edit of a file the parent unlinked loses" in every take, read
  from git; `run_all` asserts that both happen across the CI seeds (seeds 1 to 64: 3 and 37 times under full
  lineage).
- **The dead rename-over arms are deleted.** A rename-over is one save, so rule 3e already makes it whole; rule 11
  now says so instead of carrying a rule of its own. Every test passes without the arms.
- **The random runs open files during takes** (above), the pattern 500 seeds missed: at seeds 1 to 64 about 300
  calls a run, 67 of them saves. The review's planted bug (a landing that locks only the files its plan writes) is
  caught by `locks`, which reads a landing's files from git (325 of seeds 1 to 500, 2026-11-05), and by its scenario
  (`r9_a_landing_holds_the_files_of_its_losing_changes`).

## The adversarial review of 2026-11-04 (at `ee29551`)

It found the model NOT SOUND: one P0, seven P1s, six P2s, from eight planted bugs. What changed:

- **P0-1, P3 deadlocks cooperating programs:** recorded as finding 24, open, with an ignored test, for the owner. Not
  redesigned here: the only P3 that cannot make a call wait on another client's close is no P3 for opens, which
  trades the deadlock back for starvation, his trade to make. He made it on 2026-11-04: no P3 (below).
- **P1-a, P3 checked against the model's own bookkeeping:** the harness now referees P3 from its own record (above:
  the entries it saw begin waiting, their files read from git, each client's opens from its own object table).
  It catches the review's planted M3 and M6 (the pull request body has the counts). That referee went with P3; its
  successor is `Oracle::no_wait_on_waiting`.
- **P1-b and P1-c, a squash's cut:** gone with squashing. The harness still fails a landing that waits while none
  of its own change's files is open; `r9_a_held_file_holds_back_only_its_saves`.
- **P1-d, the object table across a crash:** designed (git-model.md §3, §12): journaled with the calls that move
  objects, rebuilt never by identity. Not modelled beyond the model's crash keeping its table.
- **P1-e, a crash with a transaction's file open:** `r12_a_crash_saves_an_open_inside_a_transaction`, and the random
  runs crash more often then (asserted to happen at CI's seeds); it catches M4 at CI seeds.
- **P1-f, the landing lock only nominally covered:** `locks` reads every file a taking or waiting landing's changes
  touch from git and fails a lock set that misses one, so the planted bug (a landing locking only what its plan
  writes) fails an invariant at every take with a losing change, not the model's own guard at 500 seeds.
- **P1-g, P2 not modelled:** modelled after the owner's decision (below).
- **P2:** git-model.md's timing-dependent atomic-save wording (P2-a), its "checks nothing in §3 or §12" (P2-b) and
  finding 1's count (P2-c) are corrected; the pull request body is current (P2-d); open numbers never repeat
  (P2-e, git-model.md §3); rule 10's
  placement is still met only a few times per 64 seeds (P2-f).

## The owner's decisions of 2026-11-04, modelled

- **Decision 1, no P3** (finding 24): deleted. No program's open, `unlink` or `rename(2)` waits on a waiting
  landing; the 2026-11-03 lock (a take writing its files holds new write-opens back) stays. The alarm on a long wait
  went too (the owner, 2026-11-05: "no alarm is needed"). Finding 24's test passes as
  `r9_cooperating_programs_never_deadlock`; `Oracle::no_wait_on_waiting` judges every run from the harness's own
  record. Cost above.
- **Decision 2, P2, the prefix sync**: a sync delivers the parent's commits in order up to the first that touches
  a file open for writing in the sandbox and holds the rest back until it closes; each commit is one change by its
  unit (a save, a transaction's commit whole). It needs no new record: the sync's second
  parent (or the commit it fast-forwards to) is the last parent commit it delivered, and a decision in a commit it
  holds back is not in the history it delivers. `prefix_cut` checks it from git at every sync.
  The review of `d552b3d` (P0-1) found the held-back commits' decisions leaking from upstream through a record a sync
  read while squashing existed; it now reads history alone, and the checks judge a move by a collision only once it is
  delivered (`delivered`); `r9_a_prefix_sync_delivers_no_upstream_decision_before_its_commit` and
  `r9_a_prefix_sync_holds_back_the_decisions_of_the_commits_it_holds_back` reproduce both halves.
- **Older bugs now reachable, fixed**: findings 11 (and its chain, P1-1), 25 and 26, each with a scenario; seeds
  216, 253, 287 and 436 pass. Fixing 26 uncovered finding 27, resolved above.

## Open after the decisions of 2026-11-04

- **The design's P1 replay test** (replay the same calls first in, first out, in the order P1 took them) is not
  built.
- **Rule 10's placement** happens only a few times in 64 seeds (P2-f): thin evidence from the random runs.
- **P2's maximality is checked only as a necessary condition** from git (the first commit held back names an open
  file whose version differs from the sandbox's); whether the sync could have delivered it is the model's own
  merge, which the harness does not re-derive.
- **Still model memory:** `changes_whole` and `full_lineage` take changes and winners from `Landed` (which
  `gitledger` re-derives from git); a waiting take's "until" files are read from the model;
  `no_wait_on_waiting` takes a sync's held files from the model's hold (review of `d552b3d`, P2-2); `delivered`
  reads which parent commit each sync delivered from the model's sync record (each is the sync's second parent
  or fast-forward target in git).
- **Whole-commit delivery is caught only by a scenario** (P2-3): a sync that splits a transaction's commit is
  caught by `r9_a_prefix_sync_holds_back_a_transaction_whole`, not by the random runs.
- **`syncs_deliver`'s held-back excuse** covers any version taken in a held-back landing, a sibling's included;
  a leak through a sibling's landing is unproven.
- **What phase 1 must validate on a real filesystem** (git-model.md §18): `RELEASE` ordering against a reopen,
  journal durability under power loss and `ENOSPC`, the object table after a view-host restart, shared `mmap`,
  `O_TMPFILE`, stale offsets through the page cache, costs at scale, reftable under a crash, and how often real
  workloads keep a file open long enough to postpone a landing.

## Designed after the pull-request review of 2026-11-04, then modelled

The review asked for five contracts. Point 2, byte-level writes, is modelled (rules 8 and 10, the byte oracle,
findings 15 and 16). Points 1, 3 and 4 were designed in git-model.md and are now modelled in part, each with its
choices marked in the rules table:

- **Recording before return** (git-model.md §3): rule 12. Modelled: the journal as each open's calls, a crash that
  replays it as the opens' closes, once per open (`Agentfs-Open`), takes put back. Not modelled: power loss before
  an `fsync` (the model's crash keeps every journaled call), a failed save retried, refusals at the write call.
- **Objects apart from file identities** (git-model.md §7): rule 11. Modelled: creates, objects, the atomic save as
  a rewrite, orphans and their event (findings 21 and 22).
- **Progress** (git-model.md §6): rule 9. Modelled: P1 and, after the owner's decisions of 2026-11-04, P2 as the
  prefix sync and no P3. **Not modelled:** the model test that replays the calls first in, first out in the order
  P1 took them.
- **Framing** (git-model.md §14, §15): "checked for the modelled executions".

## Left out

- **Files, in part.** Creates take one of three unborn identities (a device, above); no directories, hard links,
  `O_TMPFILE` with `linkat`, or lines moved between files; no path `truncate(2)` or `mmap`. Its bytes are ASCII (so an offset is a character boundary); NUL bytes
  from a hole are modelled, other binary content is not.
- **Open files, in part.** An open file is private until close (close-to-open, the owner's decision): the
  model has no reads through an open file, and two writers' calls on one file meet at their closes, each
  applied to the head then. An offset write's position is read on the head at close, so the model's base for
  its alignment is that head, not the file as the program opened it (git-model.md §6). Appends never write over a line (checked); two offset writes to one
  line end with the last close's line, as two Linux writers at one offset end with the last write's.
- **Where the line map lives** in a real tree: the model keeps it in each file's blob (git-model.md §16).
- Binary files (the alignment diffs as text, so a NUL byte cannot hide a change, but no binary rule is
  designed; the model's bytes are ASCII); approvals and when landings run; integrator patches; restarts beyond the crash's journal replay (rule 12)
  and power loss; space limits; history truncation.
