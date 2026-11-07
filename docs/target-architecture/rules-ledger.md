# The rules ledger

Every rule that decides an outcome in agentfs's landing model, in plain words: what a tree shows, what lands,
what loses, what waits and what history records. Each rule has an ID and one status:

| Status | Means |
|---|---|
| `owner` | the owner's own words, quoted and dated |
| `approved` | proposed by an agent, then approved by the owner (his answer quoted and dated) |
| `proposed` | written by an agent, not yet approved: **the ledger check fails while any rule is `proposed`** |
| `rejected` | the owner said no: the check fails while the model still has it (the `Gone when` names) |
| `parked` | the owner's words for a design he set aside, kept for when it is reopened; not in force and not built |

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
  Open conflict: a call deferred by R5.3 is not in its tree at once (git-model, Questions for the owner).
  Tests: `r1_writes_never_wait_and_forks_sync_both_ways`
- **R1.1** · owner · An open file is private until close: other readers see only saved content (close-to-open).
  Source: "Go" (2026-11-03), accepting that recommendation.
- **R1.2** · rejected · Whatever a tree held when a save was made counts as seen. Owner, 2026-11-05: "yes, replace
  it" (by R3c.2).
- **R1.3** · approved · A program reads its own unsaved writes back through its own open file, as on Linux.
  Approved by the owner, 2026-11-05 (approval page). 
- **R1.4** · approved · A shared memory mapping behaves as on Linux, so R1.1 holds for descriptors, not mappings. All
  mappings of a file in one tree share its pages: another mapping can read unsaved stores, and each close saves what
  the kernel has flushed, which can include another program's mapped stores. No writer's bytes are lost that Linux would
  keep. Owner, 2026-11-06, accepting that recommendation with the handoff's evidence: "we can take those recommendations
  for now, and i might change my mind later".
  The kernel keeps one size and one set of pages per file, so R1.1 holds for `read` and `pread` on read-only
  descriptors. While a file is open for writing, `stat` reports the writers' size to everyone, a writable descriptor also
  reads flushed mapped stores, and `sendfile`-family calls read what a mapping reads, which can include unsaved bytes.
  Owner, 2026-11-06, choosing option A of three: "A is fine. but im still on the fence about memory mapped files, because
  they seem to be causing issues. Go with (A) for now". Revisable: option C was to refuse shared mappings.
  Tests: `a_fill_reads_every_opens_calls`, `a_stale_page_does_not_revert_a_descriptor_write`,
  `the_first_close_saves_what_the_kernel_flushed`, `a_later_close_keeps_newer_mapped_stores`,
  `a_later_close_does_not_revert_a_third_save`, `a_recovery_close_keeps_a_store_whose_handle_closed`,
  `a_recovery_close_does_not_reapply_a_saved_store`, `two_writers_in_every_combination`
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
- **R3c.1** · rejected · "Seen" is judged per line, not per commit: a line counts as seen only if the new line
  was written over it. Owner, 2026-11-05 (in chat, round 2): "this doesnt feel right"; replaced by R3c.2.
- **R3c.2** · owner · A program has seen a version only if it could read it through normal file I/O: the file was
  closed when that version arrived, so the program's next open reads it. Owner, 2026-11-05: "i think we can only say
  that a client has had a chance to "see" a landing is if the file was closed when the landing occurred. that is --
  only if the client can read the updated content through normal file I/O procedures."
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
- **R3f** · owner · Trees are never kept in step by ordering them. There are always periods when two trees do not hold
  the same commits. What agentfs guarantees is that every tree applies the same reconciliation rules, so histories
  eventually settle, consistently. Owner, 2026-11-05: "we can’t control the order that remotes synchronize. There are
  guaranteed to be periods where they don’t have the same commits. Our rules are about them sharing the same
  reconciliation rules so that the histories eventually settle, consistently."
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
  it should be able to acquire a file lock." Replaced: the commit goes ahead, and files still open save at close as
  ordinary later saves. His question, whether a transaction declares its program, is answered by R5.4: it does not.
  Gone when: `OpenFiles`
- **R5.3** · owner · While a transaction is in progress, other I/O on its files is prohibited, locked or deferred
  until it completes. Owner, 2026-11-05: "additional I/O on that file must be prohibited/locked/deferred until the
  transaction is complete. else we risk a client trying to write the transaction while also writing the the file".
  Deferred: other writes to those files are accepted and applied in order after the transaction completes. Owner, 2026-11-05: "deferred, obviously. thats what i mean by "lock" -- its about ordering the operations so that agentfs has a change to mutate the filesystem without a client fighting it".
  Its files, owner, 2026-11-05: "The changes that it adds to the transaction." A deferred call and a sync: the sync
  waits until the transaction ends (owner picked this, 2026-11-05, cost stated). A deferred call at a crash: the crash
  rolls the transaction back and applies the call (owner picked this, 2026-11-05). The model does not match these
  yet: it counts files open for writing and keeps a deferred call behind a surviving transaction.
- **R5.4** · owner · A transaction has no owner program. Owner, 2026-11-05: "There is no “who” in a transaction —
  transactions stage file changes"; "A transaction is an agentfs concept, not a property of the local/shared fs."
  Not built: the model still binds a transaction to a client and adds `Client::Outside`, which no rule supports.
  The API of the integration that creates one is not defined (his question, 2026-11-05: "I hat is the API of an
  integration that can create transactions?").

## 4. Who wrote what

- **R6** · owner · Every commit names its one writer (main, a sandbox, or agentfs); no commit mixes writers.
  Source: "Agentfs history shouod distinguish between the various containers" and "THE EDITS CAN LAND IN ONE
  COMMIT?!?" (2026-11-01). Tests: `r6_a_sync_is_a_merge_with_the_parent_second`
- **R6.1** · approved · A sync that cannot fast-forward is a merge commit credited to `agentfs`, with no edits of
  its own. Approved by the owner, 2026-11-05 (approval page). 

## 5. History settings

- **R7** · owner · Full lineage only: every sandbox commit is in main's history (squashing dropped, owner 2026-11-05:
  "drop squashing"). Transaction commits stay as they are. Every sandbox
  commit stays stored. Source: "one setting per volume" (2026-11-02); "the sandbox commits could be squashed to
  one changeset (if they are the naive simple commits) ... leave the transaction commits as they are"; "every
  sandbox commit stays stored ... that makes sense" (2026-11-03). Tests: `r7_every_sandbox_commit_lands_on_its_own_in_mains_history`
- **R7.1** · rejected · Squashing happens after judging: each save is judged on its own, only the winners are
  squashed, and a loser stays its own commit. Approved by the owner, 2026-11-05 (approval page); his note: "if my squashing optimization idea causes too many issues, we can drop it entirely. it is mostly veneer to cut down on noise in the git history.". Owner, 2026-11-05 (in chat, round 2): "drop squashing". Gone when: `Squashed`
- **R7.2** · rejected · The history setting never changes an outcome: the same calls decide the same trees and
  collisions under either setting. Owner, 2026-11-05 (in chat, round 2): "drop squashing": with one history mode there is no setting to compare. Gone when: `Squashed`
## 6. What a save records

- **R8** · owner · A save records what the program's write calls did (an append adds, a write at an offset
  replaces that range, a truncate and rewrite replaces the file) and applies them at close. agentfs infers
  nothing beyond that. Source: "I dont think that agentfs should try to do anything beyond record the actions
  through the filesystem" (2026-11-03). Tests: `r8_concurrent_appends_never_overwrite_each_other`,
  `r8_a_stale_rewrite_overwrites_the_synced_line_and_history_keeps_it`
- **R8.1** · approved · An append to a file with no final newline finishes its last line ("foo" plus "bar" is one
  line, "foobar"), a new version of that line, so two such appends collide. Owner, 2026-11-05 (in chat, round 2): approved. Tests: `r8_an_append_onto_a_partial_last_line_finishes_it`, `r8_concurrent_appends_onto_a_partial_last_line_collide`

## 7. Locks and waiting

- **R9** · owner · A sync locks every file it will overwrite: it waits until no program has the file open for
  writing, holds the files while it writes, and new write-opens wait for it to finish. Every wait ends on an event,
  never a timer. Source: "A sync should acquire a lock on every file that it aims to overwrite, yes" (2026-11-03).
  Tests: `r9_a_sync_waits_for_an_open_writer_then_applies`, `r9_a_write_open_waits_while_a_sync_holds_the_file`
- **R9.10** · owner · Locks order agentfs's own writes against clients only. Clients are never locked or
  serialized against each other (two programs on one file: last writer wins); agentfs only tracks their opens to
  know when it has the runway to sync or land. Locking must never deadlock and should starve as little as possible.
  Owner, 2026-11-05: "the lock mechanism only really applies to agentfs interactions -- we do not need to
  lock/serialize multiple clients from competing with one another. we only need to track those clients so that
  agentfs knows when it has the runway to commit/land work"; "we absolutely must avoid deadlocking, while also trying
  to minimize starvation". A sync or landing waits for a moment when all its files are closed, tried again at each close (R9.4). Owner,
  2026-11-05, choosing between that and another round of snapshot opens: "Locks, keep snapshots on file". The cost
  stays: overlapping opens can delay a landing indefinitely. Phase 1 measures how often; snapshot opens
  ([parked design](snapshot-opens.md), R9.11 to R9.15 and R9.17) are the next step if that is a real problem.
- **R9.11** · parked · Snapshot opens. A sync or landing never waits for a program's open file: it applies at once,
  as one atomic step. Each open keeps the version of the file it opened; at close its save is merged line by line
  against that version (R3c.2: it has not seen what arrived while it was open). Owner, 2026-11-05: "alright, lets
  try option 4 until it becomes a problem." Parked, 2026-11-05: "Locks, keep snapshots on file". The design failed two reviews
  ([snapshot-opens.md](snapshot-opens.md)); R9, R9.1, R9.3, R9.4, R9.5, R9.7 and R9.9 stay in force.
- **R9.12** · parked · A close works in stages. First agentfs builds the edit the program just made, against the
  version it opened, knowing nothing of what has landed since. Reconciling that edit with what has landed comes
  after. Owner, 2026-11-05: "when the client closes the file, the first stage is simply trying to build the edit that
  the client just made, either for a commit or for the impending transaction. The reconciliation with the prior
  landed commits occurs _after_ that edit has been calculated."
- **R9.13** · parked · A client's view stays on the commit from before a landing. It moves forward only after the
  landings are fully resolved, and only when the client opens a file again. Owner, 2026-11-05: "whenever a landing is
  about to occur, the client view becomes sticky to whatever commit predates the client landing. And the client view
  can only be fast-forwarded after the landing(s) have been fully resolved and also only when the client opens a file
  again."
- **R9.17** · parked · The view moves forward per file, per open. agentfs tracks each open file against the commit it
  opened; a file opened after a landing sees it, and a file already open stays on its own commit. Owner, 2026-11-05:
  "the view can only move forward for each file that has opened after the landing. so we would likely need to track
  each open file against the SHA that it opened".
- **R9.14** · parked · A file deleted or replaced by an incoming change while a program has it open: the program's
  save still counts as an edit of that file, judged by R11 (an edit of a file the parent deleted loses, kept in
  history). Owner, 2026-11-05: "(a) sounds correct to me". R11.3 covers deletes made inside the same tree only.
- **R9.15** · parked · A program's truncate and rewrite replaces the file, even if a landing changed it while it was
  open: last writer wins, both versions kept in history. Owner, 2026-11-05: "a".
- **R9.16** · owner · Two programs in one tree: the later close is final inside that tree. Only lines a sync brought
  are left for the parent to decide. Owner, 2026-11-05: "a".
  This orders descriptor writes. When a program overwrites another's `pwrite` through a shared mapping, flushes and
  closes, and the first program closes later, the mapped bytes stand (R1.4). Owner, 2026-11-06: "if the mapped bytes were
  written second in this shared fs, then i would expect them to win".
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
- **R9.5** · approved · A waiting entry waits only for its own files, and for earlier entries that share a file
  or a source with it (P1). Owner, 2026-11-05 (in chat, round 2): "i think approve". Tests: `r9_a_long_lived_main_writer_holds_back_only_its_own_file`
- **R9.6** · approved · A client's death closes its open files and saves them, as Linux's close on exit does.
  Approved by the owner, 2026-11-05 (approval page). Tests: `r9_client_death_releases_its_locks`
- **R9.7** · approved · `unlink` and `rename` of a locked file wait like a write-open.
  Approved by the owner, 2026-11-05 (approval page). Tests: `r11_an_unlink_or_rename_of_a_held_file_waits`
- **R9.8** · rejected · Each open that keeps a waiting landing waiting raises an alarm event. Owner, 2026-11-05:
  "no alarm is needed". Gone when: `Delayed`
- **R9.9** · approved · A held-back commit's decisions are held back with it, so a decision never arrives before
  the change it decided. Owner, 2026-11-05 (in chat, round 2): approved; decisions to be recorded in the commit they belong to (with R5.1). Tests: `r9_a_prefix_sync_delivers_no_upstream_decision_before_its_commit`

## 8. Lines, files and names

- **R10** · approved · At close, a save matches the program's bytes to the file's old lines once, by git's
  histogram diff. Matched lines keep their identity. From then on collisions compare line identities, never
  positions. A moved line counts as a delete plus a new line. Owner, 2026-11-05 (in chat, round 2): approved. Tests: `r10_an_insert_shifts_lines_without_changing_their_identity`
- **R10.3** · owner · Two deletes of one line agree: both are recorded as ordinary changes of their saves, the
  merge reports no collision, and the line stays deleted. Owner, 2026-11-05: "i want minimal "special cases", so
  if both deletes are recorded and then agree, my hope is that a merge reports no conflicts and life is good".
  This holds for a file a rename-over named: an agreeing delete of it is not half a rename-over (finding 31). Owner,
  2026-11-06, accepting that recommendation: "we can take those recommendations for now, and i might change my mind later".
  Tests: `r10_an_agreeing_delete_of_a_rename_overs_file_agrees`, `r10_two_deletes_of_one_line_agree_over_an_unseen_edit`
- **R10.8** · approved · A sandbox save whose unfinished last line would land in front of a line the parent keeps
  collides: the parent wins and the save loses whole, retained in history. No tree shows bytes nobody wrote, and line
  order does not change (R10.7) (finding 32). Owner, 2026-11-06, accepting that recommendation: "we can take those
  recommendations for now, and i might change my mind later".
  Tests: `r10_an_unfinished_line_before_a_kept_line_loses_whole`, `r10_an_unfinished_save_after_the_parents_last_line_lands`,
  `a_save_with_an_unfinished_line_before_a_kept_line_loses_whole`
- **R10.9** · approved · Until the parent decides, a sandbox holding an undecided unfinished last line shows that line
  and hides the parent's later lines that a sync would put after it, the mirror of R10.6. Nothing is lost: the sandbox
  gets the parent's version once decided, so R3d's decided version waits for that decision. Owner, 2026-11-06, choosing
  between this, showing the joined bytes and holding the sync: "a" (finding 33).
  Tests: `r10_a_sync_hides_the_parents_lines_after_an_undecided_unfinished_line`,
  `a_sync_hides_the_parents_later_lines_behind_an_undecided_unfinished_line`
- **R10.4** · owner · A line appended after the parent's unfinished last line (no final newline) collides with it,
  and the parent wins. agentfs never adds a newline no program wrote. Owner, 2026-11-05: "collide".
- **R10.6** · owner · Until the parent decides, a sandbox holding an undecided line does not show the parent's
  unfinished last line in front of it: the file shows the sandbox's line alone, then the parent's version once decided.
  No tree shows bytes nobody wrote. Owner, 2026-11-05: "1 a" (finding 29). This also covers a line the sandbox already
  held: it leaves the sandbox's file until the parent decides, then the parent's version arrives. Owner, 2026-11-06:
  "A is fine" (finding 35). Revisable: option b was to keep showing the older version.
  Tests: `r10_a_sync_hides_the_parents_unfinished_line_before_an_undecided_one`,
  `a_sync_hides_the_parents_unfinished_line_until_decided`, `finding_35_a_sync_hides_a_line_the_sandbox_already_held`
- **R10.7** · owner · An edited line keeps its place. When a parent appends a line after a line a sandbox edits, the
  merged file reads the edit, then the append. Owner, 2026-11-05: "B seems more correct. The magic words there are
  “edit” and “append”. From the perspective of each writer, their operations merged together naturally look like
  option b". Tests: `r10_an_edited_line_keeps_its_place_before_an_append`
- **R10.5** · owner · The same inside a transaction: a line the transaction appended after the sandbox's unfinished
  last line collides at commit, the sandbox's line wins, and the program is told (R5). Owner, 2026-11-05: "A" (finding 30).
- **R10.1** · approved · Of two inserts at one place, the parent's comes first. Approved by the owner, 2026-11-05 (approval page); he asked whether this is only inserts: yes; two edits of one line are a collision the parent wins (R3b). Tests: `r10_concurrent_inserts_at_one_place_keep_both_the_parents_first`
- **R10.2** · rejected · A partial last line that a merge puts another line after reads as ending in a newline. Owner, 2026-11-05 (in chat, round 2): "collide": a line appended after the parent's unfinished last line collides with it (R10.4). Gone when: `r10_a_line_after_a_partial_line_reads_terminated`
- **R11** · approved · A file keeps its identity through renames (like an inode). Its name and existence are one
  more line, judged like any other: an unlink loses against a parent's edit it never saw; an edit of a file the
  parent unlinked loses; a rename and an edit both apply. Approved by the owner, 2026-11-05 (approval page). Tests:
  `r11_an_unlink_against_a_concurrent_edit_the_parent_wins`, `r11_a_rename_and_a_concurrent_edit_both_apply`
- **R11.1** · owner · When two files are renamed to one path, the parent's queue decides it like any collision:
  the parent's name wins and the sandbox's save loses. Until then the sandbox shows its files as they were without
  the parent's clashing rename, so the parent's file keeps the name it has in the sandbox; on a chain, the same
  until no two files share a path. Owner, 2026-11-05: "before the landing, the filesystem should show the state of
  the files without the landing". Tests:
  `r11_two_renames_to_one_path_keep_the_sandboxs_name_until_decided`, `r11_a_chain_of_renames_to_one_path_keeps_every_previous_name`
- **R11.2** · approved · An editor's atomic save (write a new file, rename it over the old one) is a rewrite of
  the old file, so the file keeps its identity and merges per line. Approved by the owner, 2026-11-05 (approval page). Tests: `r11_an_atomic_save_keeps_the_files_identity_and_merges_per_line`
- **R11.3** · approved · Writes through a handle whose file was unlinked or replaced reach no file, and raise an
  `OrphanDropped` event. Owner, 2026-11-05 (in chat, round 2): approved. Tests: `r11_an_old_writer_after_an_atomic_save_saves_nothing`
- **R11.4** · approved · A tracked file arriving at a path an untracked file holds wins the path; the untracked
  file stays hidden and shows again if the tracked one goes. Owner, 2026-11-05 (in chat, round 2): option 1, the tracked file shows.

## 9. Crashes

- **R12** · approved · Every write call is in its tree's journal before it returns. After a crash, each open
  file is saved from the journal as its close would have saved it, exactly once, before any tree is served again.
  Approved by the owner, 2026-11-05 (approval page). Tests: `r12_a_crash_saves_every_open_from_the_journal`, `r12_a_crash_during_a_close_saves_the_open_once`, and on the real journal `a_step_killed_before_its_record_saves_at_restart`, `a_step_killed_after_its_record_is_not_saved_again`, `a_failed_save_keeps_serving_and_saves_at_the_next_step`
