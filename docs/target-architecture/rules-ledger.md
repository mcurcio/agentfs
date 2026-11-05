# The rules ledger

Every rule that decides an outcome in agentfs's landing model, in plain words: what a tree shows, what lands,
what loses, what waits and what history records. Each rule has an ID and one status:

| Status | Means |
|---|---|
| `owner` | the owner's own words, quoted and dated |
| `approved` | proposed by an agent, then approved by the owner (his answer quoted and dated) |
| `proposed` | written by an agent, not yet approved: **the ledger check fails while any rule is `proposed`** |
| `rejected` | the owner said no: the check fails while the model still has it (the `Gone when` names) |

How it is enforced (`crates/agentfs-model/tests/ledger.rs`, run in CI with the workspace tests):

- no rule may be `proposed`;
- a `rejected` rule's `Gone when` names may not appear anywhere in `crates/agentfs-model`;
- every `rule N` or `rule Nx` cited in the model's code, `MODEL.md` and `git-model.md` must be a rule here;
- every test a rule names must exist;
- every check exception in `src/check.rs` (a line marked `// excuse:`) must cite an `owner` or `approved` rule.

**Every decision here is binding only until a blocking consideration is found.** The owner, 2026-11-05: "i want
it noted that these decisions are only binding _until_ we later discover a blocking consideration. my concern is
that a future author starts building hacks into some code in order to abide by these decisions, when i would
instead want those hacks to be escalated as a code smell that demands the architecture be reopened to account for
some new case". So when keeping a rule needs a workaround (an exception, a special case, a flag, a check taught to
excuse something), do not build it. Escalate it to the owner as a reason to reopen the architecture.

Only an outcome is a rule. How the model stores a decision (ref names, the notes ref, the graft) is not, unless it
changes what a tree shows or what lands. A new rule, a new exception or a weaker check goes to the owner as a
question first ([VERIFYING.md](../../crates/agentfs-model/VERIFYING.md) §"Fix passes"). It is never added as
`proposed` and then built.

"Parent" means main for a sandbox, and the sandbox for a sandbox inside it. A *sync* brings the parent's version
into a sandbox. A *landing* brings a sandbox's save into its parent. Two lines *collide* when each was written
without seeing the other. MODEL.md's rules table holds the full wording; this page is the authority on status.

## 1. Like a Linux filesystem

- **R1** · owner · A program's close is never refused, and its save is in its own tree at once. No collision
  holds back a sync, a landing or a transaction's commit. Changes flow both ways without anyone asking.
  Source: "I generally like the metaphor of a typical Linux filesystem. I want to try that out." (2026-11-02).
  Tests: `r1_writes_never_wait_and_forks_sync_both_ways`
- **R1.1** · owner · An open file is private until close: other readers see only saved content (close-to-open).
  Source: "Go" (2026-11-03), accepting that recommendation.
- **R1.2** · approved · Whatever a tree held when a save was made counts as seen by that save.
  Approved by the owner, 2026-11-05 (approval page). 
- **R1.3** · approved · A program reads its own unsaved writes back through its own open file, as on Linux.
  Approved by the owner, 2026-11-05 (approval page). 
- **R2** · owner · No clocks. Each tree takes its queue in order, and which of two lines is newer is read from
  git's ancestry, never from a date. Source: his premise, asking how last-writer-wins works with no shared
  clock (2026-11-02). Tests: `r2_one_queue_fixes_the_landing_order`, `r2_no_clock_commit_dates_decide_nothing`

## 2. Collisions: the parent rule

- **R3a** · owner · Each collision between a sandbox and its parent is decided once, at the parent's landing
  queue. A sync never decides one: the sandbox keeps its own line until then, then takes the decision. So a
  sandbox's line never flips back. Source: "ok, that tracks" (2026-11-02).
  Tests: `r3_a_collision_is_recorded_once_at_the_parents_queue`, `r3_a_fork_never_flips_back_to_its_own_edit`
- **R3b** · owner · There, the parent's version wins. The losing version stays in history, readable on the shared
  remote, naming its writer. A save made before the parent's sync arrived loses. The same at every level of
  nesting. Source: "i think the updated main should win in a conflict" (2026-11-03); "yes, "losing" patches should
  still be part of the history. i want to know who changed what when" (2026-11-03).
  Tests: `r3_landing_collision_the_parent_wins`, `r3_a_save_before_the_sync_arrives_loses_to_the_parent`,
  `r4_nested_forks_follow_the_same_rules`
- **R3c** · owner · A line written on top of the parent's current line is no collision: it replaces it. So a
  save made after a sync wins upstream. Source: "if the program saves after the sync has landed, then arguably
  the sandbox was the last writer and its change should take effect" (2026-11-03).
  Tests: `r3_a_save_after_the_sync_wins_upstream_by_lineage`, `r3_an_edit_over_the_decided_line_replaces_it`
- **R3c.1** · proposed · *The owner asked, 2026-11-05: "you must first define "seen" if i am supposed to write rules around it"* "Seen" is judged per line, not per commit: a line counts as seen only if the new line
  was written over it. Recommend: approve. Judged per commit, an edit over a sandbox's own undecided line
  silently overwrote main's winning line (MODEL.md finding 8).
  Tests: `r3_an_edit_over_an_undecided_line_still_collides`
- **R3d** · owner · A sync carries the parent's current decided version, never a stale one, and keeps only the
  sandbox's own undecided lines (R3a). Tests: `r3_a_sync_carries_the_parents_current_version`,
  `r3_a_sync_keeps_the_undecided_saves`
- **R3e** · owner · A change lands whole or loses whole. A plain save is the whole file as written at one close.
  A transaction is all its files, and the program is told the outcome. If any line collides and the parent
  wins, nothing of the change lands. Each later save is judged on its own. Source: "I thought we had already
  decided that each commit was a transaction"; "some commits represent a transaction, and some commits represent a
  collection of changes"; "The whole file, right?" (2026-11-03).
  Tests: `r3_a_save_loses_whole_with_its_clean_lines`, `r3_a_later_save_after_a_losing_one_is_judged_on_its_own`,
  `r3_a_transaction_loses_whole_and_is_told`, `r3_a_file_rule_loss_costs_the_whole_change`
- **R3e.1** · owner · Within one landing, lines of two different changes are judged per line, so neighbouring
  lines of two changes both apply. Source: "Let’s go back to the previous model" (2026-11-03), after git's own
  merge, which treats neighbouring lines as one conflict, was tried and failed.
  Tests: `r3_neighbouring_lines_merge_per_line`
- **R3e.2** · approved · A version the parent already holds, or has already written over, is not a collision
  and costs the change nothing. Approved by the owner, 2026-11-05 (approval page). 
- **R3e.3** · rejected · A delete that agrees with the parent's delete of the same line is dropped: no change,
  no record. Owner (2026-11-05), on finding 27: "(a) agree -- why would it be dropped? it sounds like some
  optimization was injected which is causing problems". The delete is now an ordinary change of its save.
  Gone when: `lost_delete`, `r3_a_dropped_agreeing_delete_follows_the_delete_it_agreed_with`,
  `r3_a_nested_sandboxs_dropped_delete_follows_the_delete_that_lost`
- **R4** · owner · Nothing waits behind a collision: every sandbox commit is taken in order and lands or loses
  whole. Source: "Every commit can be landed independently (according to the rules of the commit vs transaction,
  etc)." (2026-11-04). Tests: `r4_nothing_waits_behind_a_collision`

## 3. Transactions

- **R5** · owner · Opt-in. Begin opens a new branch frozen at the sandbox's version. The program sees its own
  edits while the sandbox keeps syncing. Rollback, abandon or the client dying drops the branch. Commit lands it
  in the sandbox whole or not at all, and tells the program. Source: "Frozen snapshot was my intention, yes.
  That's why I said “new branch” earlier." (2026-11-02); "It’s a transaction, so a failure is an implicit
  rollback." (2026-11-02). Tests: `r5_transaction_commits_clean`, `r5_transaction_rollback_and_client_death_leave_nothing`,
  `r3_a_transaction_loses_whole_and_is_told`
- **R5.1** · approved · A transaction that lost still writes a commit with the sandbox's files, so history shows
  the decision. Approved by the owner, 2026-11-05 (approval page); his note: "its also necessary to record it with enough metadata so we know where that commit fits in the history. for example, which remote? did it "win" or "lose" a conflict? this cannot be a flat git history where any arbitrary checkout is a "correct" representation -- each commit is contextual to a branch/fork/remote". 
- **R5.2** · rejected · A transaction's commit is refused while the same program holds open a file the commit
  would write. Owner, 2026-11-05: "if a file is held by program A, and program A tries to commit a transaction, then
  it should be able to acquire a file lock." What replaces it is open (his question: does a transaction need to
  declare the program it belongs to?). Gone when: `OpenFiles`

## 4. Who wrote what

- **R6** · owner · Every commit names its one writer (main, a sandbox, or agentfs); no commit mixes writers.
  Source: "Agentfs history shouod distinguish between the various containers" and "THE EDITS CAN LAND IN ONE
  COMMIT?!?" (2026-11-01). Tests: `r6_a_sync_is_a_merge_with_the_parent_second`
- **R6.1** · approved · A sync that cannot fast-forward is a merge commit credited to `agentfs`, with no edits of
  its own. Approved by the owner, 2026-11-05 (approval page). 

## 5. History settings

- **R7** · owner · One history setting per volume: full lineage (every sandbox commit in main's history) or
  squashed (a sandbox's plain saves become one changeset). Transaction commits stay as they are. Every sandbox
  commit stays stored. Source: "one setting per volume" (2026-11-02); "the sandbox commits could be squashed to
  one changeset (if they are the naive simple commits) ... leave the transaction commits as they are"; "every
  sandbox commit stays stored ... that makes sense" (2026-11-03). Tests: `r7_squashed_saves_land_as_one_changeset`
- **R7.1** · approved · Squashing happens after judging: each save is judged on its own, only the winners are
  squashed, and a loser stays its own commit. Approved by the owner, 2026-11-05 (approval page); his note: "if my squashing optimization idea causes too many issues, we can drop it entirely. it is mostly veneer to cut down on noise in the git history.". Tests: `r7_a_squashed_take_squashes_only_the_winners`
- **R7.2** · proposed · *The owner asked, 2026-11-05: "what "history setting"?"* The history setting never changes an outcome: the same calls decide the same trees and
  collisions under either setting. Recommend: approve (it follows from R7.1). Finding 27 broke it.
  Tests: `r7_the_history_setting_decides_no_collision`, `random_interleavings_history_settings_decide_the_same`

## 6. What a save records

- **R8** · owner · A save records what the program's write calls did (an append adds, a write at an offset
  replaces that range, a truncate and rewrite replaces the file) and applies them at close. agentfs infers
  nothing beyond that. Source: "I dont think that agentfs should try to do anything beyond record the actions
  through the filesystem" (2026-11-03). Tests: `r8_concurrent_appends_never_overwrite_each_other`,
  `r8_a_stale_rewrite_overwrites_the_synced_line_and_history_keeps_it`
- **R8.1** · proposed · *The owner asked, 2026-11-05: "is there a downside to this model?"* An append to a file with no final newline finishes its last line ("foo" plus "bar" is one
  line, "foobar"), a new version of that line, so two such appends collide. Recommend: approve; it is what the
  bytes say. Tests: `r8_an_append_onto_a_partial_last_line_finishes_it`, `r8_concurrent_appends_onto_a_partial_last_line_collide`

## 7. Locks and waiting

- **R9** · owner · A sync locks every file it will overwrite: it waits until no program has the file open for
  writing, holds the files while it writes, and new write-opens wait for it to finish. Every wait ends on an event,
  never a timer. Source: "A sync should acquire a lock on every file that it aims to overwrite, yes" (2026-11-03).
  Tests: `r9_a_sync_waits_for_an_open_writer_then_applies`, `r9_a_write_open_waits_while_a_sync_holds_the_file`
- **R9.1** · owner · A landing locks the files its changes touch, the same way. Source: "freezing only the files
  that a landing touches sounds reasonable" (2026-11-02). Tests: `r9_a_landing_waits_for_the_parents_open_writer`,
  `r9_a_landing_holds_the_files_of_its_losing_changes`
- **R9.2** · owner · No program's open ever waits on a landing that is still waiting; a landing may wait
  forever, and the parent then resolves its commits. Source: "it seems low risk for them to be indefinitely delayed
  while an application is keeping the fs busy with real work. In the worst case, the landing never occurs in a
  child fork, and the parent is left to resolve the commits." (2026-11-04).
  Tests: `r9_cooperating_programs_never_deadlock`
- **R9.3** · owner · A sync delivers the parent's commits in order up to the first one that touches a file open
  in the sandbox, and holds back that one and the rest until the file closes. Source: "agreed — that sounds like
  the rule we had discussed" (2026-11-04). Tests: `r9_a_sync_delivers_up_to_the_first_change_to_an_open_file`
- **R9.4** · approved · A sync or landing waits for all its files at once, never holding one while it waits for
  another. Approved by the owner, 2026-11-05 (approval page); he asked whether a fast writer can starve a landing (open, with R9.8). 
- **R9.5** · proposed · *The owner asked, 2026-11-05: "what is a "waiting entry"?"* A waiting entry waits only for its own files, and for earlier entries that share a file
  or a source with it (P1). Recommend: approve; one long-open file then holds back only its own changes.
  Tests: `r9_a_long_lived_main_writer_holds_back_only_its_own_file`
- **R9.6** · approved · A client's death closes its open files and saves them, as Linux's close on exit does.
  Approved by the owner, 2026-11-05 (approval page). Tests: `r9_client_death_releases_its_locks`
- **R9.7** · approved · `unlink` and `rename` of a locked file wait like a write-open.
  Approved by the owner, 2026-11-05 (approval page). Tests: `r11_an_unlink_or_rename_of_a_held_file_waits`
- **R9.8** · proposed · *The owner asked, 2026-11-05: "can we do better than an alarm?"* Each open that keeps a waiting landing waiting raises an alarm event. Recommend: approve;
  it is the only signal that a landing may never happen.
  Tests: `r9_new_writers_postpone_a_waiting_take_and_raise_an_alarm`
- **R9.9** · proposed · *The owner asked, 2026-11-05: "where it came from? how can a decisions be seperated from its change?"* A held-back commit's decisions are held back with it, so a decision never arrives before
  the change it decided. Recommend: approve (it fixed a review P0).
  Tests: `r9_a_prefix_sync_delivers_no_upstream_decision_before_its_commit`

## 8. Lines, files and names

- **R10** · proposed · *The owner asked, 2026-11-05: "are we merging by lines or bytes? this one appears to conflict with R8.1"* At close, a save matches the program's bytes to the file's old lines once, by git's
  histogram diff. Matched lines keep their identity. From then on collisions compare line identities, never
  positions. A moved line counts as a delete plus a new line. Recommend: approve. It answers his "Go" to "settle
  what a line is" (2026-11-03); the costs are in MODEL.md §Costs. Tests: `r10_an_insert_shifts_lines_without_changing_their_identity`
- **R10.3** · owner · Two deletes of one line agree: both are recorded as ordinary changes of their saves, the
  merge reports no collision, and the line stays deleted. Owner, 2026-11-05: "i want minimal "special cases", so
  if both deletes are recorded and then agree, my hope is that a merge reports no conflicts and life is good".
- **R10.1** · approved · Of two inserts at one place, the parent's comes first. Approved by the owner, 2026-11-05 (approval page); he asked whether this is only inserts: yes; two edits of one line are a collision the parent wins (R3b). Tests: `r10_concurrent_inserts_at_one_place_keep_both_the_parents_first`
- **R10.2** · proposed · *The owner asked, 2026-11-05: "i dont know what this means"* A partial last line that a merge puts another line after reads as ending in a newline.
  Recommend: approve. Tests: `r10_a_line_after_a_partial_line_reads_terminated`
- **R11** · approved · A file keeps its identity through renames (like an inode). Its name and existence are one
  more line, judged like any other: an unlink loses against a parent's edit it never saw; an edit of a file the
  parent unlinked loses; a rename and an edit both apply. Approved by the owner, 2026-11-05 (approval page). Tests:
  `r11_an_unlink_against_a_concurrent_edit_the_parent_wins`, `r11_a_rename_and_a_concurrent_edit_both_apply`
- **R11.1** · proposed · *The owner asked, 2026-11-05: "is this related to landing and file locks?"* When two files are renamed to one path, the parent's keeps it. Until that is decided,
  the sandbox keeps its own name and shows the parent's file under its previous name. On a chain, the same rule
  repeats until no two files share a path. Recommend: approve. Tests:
  `r11_two_renames_to_one_path_keep_the_sandboxs_name_until_decided`, `r11_a_chain_of_renames_to_one_path_keeps_every_previous_name`
- **R11.2** · approved · An editor's atomic save (write a new file, rename it over the old one) is a rewrite of
  the old file, so the file keeps its identity and merges per line. Approved by the owner, 2026-11-05 (approval page). Tests: `r11_an_atomic_save_keeps_the_files_identity_and_merges_per_line`
- **R11.3** · proposed · *The owner asked, 2026-11-05: "is this consistent with linux?"* Writes through a handle whose file was unlinked or replaced reach no file, and raise an
  `OrphanDropped` event. Recommend: approve; on Linux they go to a file no name reaches.
  Tests: `r11_an_old_writer_after_an_atomic_save_saves_nothing`
- **R11.4** · proposed · *The owner asked, 2026-11-05: "i dont know what this means"* A tracked file arriving at a path an untracked file holds wins the path; the untracked
  file stays hidden and shows again if the tracked one goes. Recommend: approve.

## 9. Crashes

- **R12** · approved · Every write call is in its tree's journal before it returns. After a crash, each open
  file is saved from the journal as its close would have saved it, exactly once, before any tree is served again.
  Approved by the owner, 2026-11-05 (approval page). Tests: `r12_a_crash_saves_every_open_from_the_journal`, `r12_a_crash_during_a_close_saves_the_open_once`
