# Errata (append-only)

Spec, design or brief contradictions found while cutting or implementing. Newest last. Each entry:
date, who, where, what, and what was done.

Section numbers of `design.md` and `.handoff/` files cited below refer to documents that are deleted or not in the repository.

## 2026-10-17, F8 code architect (f8-interfaces)

Against `.handoff/f8-design.md` (run 6 revision 2). Resolved in `.handoff/f8-slices.md`
("Decisions taken here"); each lands as a §19.1 row marked a design choice, not the owner's words.

1. `claim(advertised, on_branch)` cannot tell `UpToDate` from `FastForward` without the head.
   Frozen as `claim(advertised, head, on_history)`.
2. `deletable(state, settings) -> Result<(), Kept>` does network I/O inside the decider and takes no
   `onDelete`, so deletion was decided in three places (DeleteVolume's `onDelete`, Kubernetes'
   `reclaimPolicy`, CreateVolume's refusal of `onDelete: delete` on local-only). Replaced by the
   pure `release::decide` (`Delete` / `Keep` / `Wait`) with `onDelete` as an input; facts gathered by
   `RemotePusher::branch_claim`. CreateVolume's local-only refusal dropped. Placed in
   `agentfs-core/src/release.rs`, not `volume.rs`.
3. "DeleteVolume pushes once more ... (else retried)": under the chart's default `Retain`,
   DeleteVolume never runs, and nothing pushes an unstaged worktree (`controller.rs`). Every agentfs
   class now has `reclaimPolicy: Delete` (admission policy widened from fork classes), DeleteVolume
   never pushes, and the node plugin's release scan (the fork publisher, generalised) pushes every
   unstaged worktree until held, raising `PushFailed` at each failure.
4. `start_branch_moved`'s clear conditions ("taking the commits in, or deleting the old branch") are
   unreachable: the patch queue cherry-picks, and the docs test refuses the delete push. Cleared by
   `agentfs start accept --tip <oid>` (create-only `refs/agentfs/<V>/accepted/<oid>`), with a new
   runbook `start-branch-moved.md`.
5. `StartPoint.refname` allowed `diverged/*` refs, which never move, so the alarm could never fire;
   and a diverged worktree's later commits reached no remote ref. `StartPoint` holds a branch only
   (`BadStartPoint::NotABranch`); the divergence save is fast-forwarded with the head (S2); the
   decider answers `Wait(Diverged)`, so the worktree stays and keeps alarming.
6. `start_from` is placed in `fork/start.rs` beside `create.rs` (sharing its tree writer), not in
   `create.rs`.
7. Brief: `.handoff/f8-slices.md` is to be written in the main checkout, which this isolated
   worktree may not write. Written to the worktree's `.handoff/f8-slices.md` instead; the
   orchestrator copies it.

## 2026-10-17, F8 S1 implementer (f8-s1)

None of these stopped the slice; each was resolved as below and is reported with it.

1. Brief: `.handoff/f8-slices.md` was not in the main checkout (erratum 7 above: not copied).
   Read from the architect's worktree (`.claude/worktrees/wf_1a3a42d9-2d6-3/.handoff/`).
2. Owned files, incomplete: renaming `node-lost.md` and retiring `--fork-scan-interval`, the
   publisher and the shared base path break links and facts outside the list:
   `docs/deployment.md`, `docs/operator-guide.md`, `docs/development.md`,
   `docs/runbooks/csi-recovery.md`, `docs/runbooks/fork-refused.md`, and design sections
   other than §11.3, §14.2, §19 (§11.5, §13's tables, §15's runbook table, §17.1's command
   list, which `reference_tests.rs` enforces, §17.2, §17.3, §18). Edited minimally (links,
   names, the retired flag). Not edited: `AGENTS.md` (its CI table names the NFS job "one
   node at a time" and lists no f8 workflow) and `fork/finish.rs`'s doc ("the node
   plugin's publisher"), owned by no F8 slice; the orchestrator should fix both.
3. DeleteVolume of a worktree whose settings cannot be read (its profile removed or broken):
   the frozen page is silent. The old test said `onDelete: retain` releases it without
   reading settings; under D1 retain waits like delete, so the remotes must be asked and
   cannot be. Chosen: `FAILED_PRECONDITION`, nothing deleted or released (the test now says
   so). A design choice.
4. `agentfs start accept --tip OID`: the binary has no runtime `git2` dependency and its
   `Cargo.toml` is not owned, so `fork::identity::full_commit` (core, owned) parses the id;
   `StartPoint::parse` uses it too (one validator).
5. The release scan's `agentfs push` holds the state lock for a moment; a NodeStage then
   made `serve-history` exit 75 (`FAILED_PRECONDITION`, kubelet retry). The service now
   starts with `--wait-lock 30s` (`launcher::SERVICE_LOCK_WAIT`). Not in the frozen page.
6. `ReleaseScan::wake` only wakes: a head a push exited 0 at stays held until the plugin
   restarts, so a remote that later loses it (its branch deleted by hand) waits for a
   restart of the plugin before DeleteVolume can complete. Clearing it at each wake would
   re-push every unstaged worktree at every DeleteVolume retry.
7. With `--mode` gone, `IdentityService::new()` takes no argument and `server::serve`
   takes the controller and node services unconditionally (both unfrozen).
8. CSI, DeleteVolume of a fork not yet published: answered `UNAVAILABLE` before the state
   lock (the finisher holds it while it publishes), through `release::decide` with the
   fork's state, as the code before F8 did; under the lock only for a published or
   abandoned fork. The frozen page's error table puts "its state lock is held" at
   `FAILED_PRECONDITION` without this exception; the forks e2e (step 4) found it.

## 2026-10-17, F8 S4 implementer (f8-s4)

1. The frozen values page says the Secret's `token` key is `user:password`. agentfs's
   credentials file is a bare token (sent as the password, with the URL's user or
   `x-access-token`) or `username=`/`password=` lines (`credentials.rs::parse`), so
   `user:password` would be refused with 401. `token` holds `username=`/`password=` lines;
   the key names, mount path and every value key are as frozen.
2. "The node plugin mounts the `token` key of the same Secret" needs `templates/node.yaml`
   (a projected credentials volume when `server.enabled`), `values.schema.json` (root
   `additionalProperties: false` refuses `server:`) and `_helpers.tpl` (the Secret's name,
   shared by node.yaml and server.yaml); none is in S4's owned files. Edited minimally; with
   the server off node.yaml renders as before, so helm-equivalence is unchanged.
3. chart-testing takes every `charts/agentfs-csi/ci/*-values.yaml` for `ct install` too, and
   helm.yml's e2e (`ci/helm-csi-install.sh`, fixed release name, no server image loaded)
   would install the server values and fail. The server values are
   `charts/agentfs-csi/ci/server.yaml`, linted by `helm lint --strict` in e2e-chart-server,
   not by `ct lint`.
4. The frozen values have no repository name: the chart serves the image's default,
   `history.git`, and NOTES.txt prints that URL.
5. The slice page was not at the path the brief gave (`.handoff/f8-slices.md` of the main
   checkout); it was read from the worktree `wf_1a3a42d9-2d6-3`.

## 2026-10-17, F8 fix agent (fix-f8)

1. Brief: `.handoff/f8-slices.md` is still not in the main checkout (errata 7, S1 1, S4 5);
   not needed for these four findings, which name their files.
2. Design §14.2 "Unpushed history" ("history no remote holds is never deleted") and
   `release::decide`'s "no history: nothing can be lost" were wrong: they weighed only the
   head, so a journal a history service killed at NodeUnstage left (records promised
   durable) was deleted. The journal's backlog is now a decider fact
   (`Waiting::Uncommitted`), and the release scan runs `agentfs flush` before pushing.
3. S1 erratum 6 is resolved, not by clearing at every wake: the scan holds a push at its
   head and the profile's remotes (a remote added later is pushed to), and DeleteVolume
   answering `Lacking` forgets that one handle (`ReleaseScan::forget`).
4. Security finding: the provisioner admission policy `agentfs-provisioner` (always
   installed, no value to turn it off) is a design choice beyond the doc correction the
   finding allowed; it needs Kubernetes 1.30's node-name claim in pod-bound tokens, which
   the chart's `kubeVersion: >=1.30` already implies.

## 2026-10-17, F8 S2 implementer (f8-s2)

None of these stopped the slice; each was resolved as below and is reported with it.

1. Brief: `.handoff/f8-slices.md` is still not in the main checkout; read from the architect's
   worktree, as S1 did.
2. Acceptance 4 (`runbooks_push_only_the_namespace` refuses every `--force-with-lease`)
   contradicts two §19.1 decisions it does not mention: "Patch submission" (a create-only
   `--force-with-lease=<ref>:`, in `docs/runbooks/patch-queue.md`, a file no F8 slice owns)
   and "Forks in history rewrites" (the fork refs deleted with a lease, by `git push`, in
   history-rewrite F3, including other worktrees' branches, never under `refs/agentfs/`).
   Resolved: the test allows only the create-only lease (empty expected value) on a
   destination of the same push literally under `refs/agentfs/`; `patch-queue.md`'s block
   names its destination literally (no other change); F3 deletes on each server with
   `git update-ref -d <ref> <oid>` (a lease) or a forge's tools (no lease), and
   `ci/e2e-runbooks.sh forks` runs it per server. §15 and a §19.1 row say so.
3. Acceptance 1 says "against the HTTP test server": the repository has no in-process HTTP
   git server (only `ci/git-http-server.sh`, CI). The moved-branch test uses a `file://`
   remote and a test-only hook between the queue push's listing and its push; the
   negotiation check it proves is transport-independent. A smart server's own
   compare-and-swap on the no-op branch update is not tested here.
4. Behaviour change the slice implies but does not state: §11.3 said the status reaches the
   remote "even while the branch cannot". With rule 2 it does not while the remote has no
   branch or another writer's: two existing tests (`applied_refs_wait_for_the_branch_push`,
   `lfs::a_failed_lfs_stage_still_pushes_the_queue_status`) now first push the branch.
5. "A lease not in the repository is fetched first": fetched into a scratch repository
   beside the push lock (alternates on the state area's objects, removed afterwards), not
   into the state area, which would take a rewrite's removed history (and its secret) back
   in at step 9. The e2e checks the state area stays clean.
6. `fork/start.rs` (S1's) `fetch_branch` made `pub(crate)` for that fetch: visibility only.
7. Not in the frozen page: `LeaseOutcome::NothingToPush` exits 1; a failed lease push leaves
   the push state alone and raises no `PushFailed` (an operator command reports directly).
8. Divergence case B for a CLI or sidecar volume: there is no CLI start-from (deferred), so
   the runbook gives a manual new-volume procedure; not exercised in CI.
9. S1's §19.1 row "Start points are branches" gave "it never moves" as a reason; with D3's
   save following the head that is no longer true, and the clause is removed.

## 2026-10-17, F8 S3 implementer (f8-s3)

None of these stopped the slice; each was resolved as below and is reported with it.

1. The frozen page never says how a fork finds its local-only source: `receive_fork_change`
   takes the source's `StateDir`, and the control socket lives in it, but neither the fork
   identity nor `csi.json` records where the source is, and `fork finish` names only the fork.
   Resolved: create writes `agentfs.fork.sourceState` (absolute, own config only, an identity
   key no include may set) when the source's fork remote is `SourceHistory`; a fork's
   `ForkRemote::SourceHistory` is "no flagged section and a recorded source state area".
   Cost: the source must stay at that path until the fork is published.
2. `init --local-only` writes `agentfs.remotes = none` in the own config, but
   `fork::create::settings_files` refuses a CLI source with settings in its own config, and a
   fork reads only the source's includes: a CLI local-only source could not be forked, and its
   fork would not see the declaration. Resolved: `agentfs.remotes` joins `OWN_KEYS`; a fork's
   mode follows its fork remote (item 1), never the declaration.
3. `history::finish_from_message` validates the fork remote trailer as a remote name, so a
   finish into the source's history needs a name no section can have: `(source history)`
   (`fork::receive::SOURCE_HISTORY`), which the parser accepts. Without it the finish record
   never parsed and publish retried forever (found by the new test).
4. Not on the frozen page: core cannot call the binary's control client, so the finisher takes
   a `fork::receive::Receiver` (`UnderLock` by default, `Finisher::set_receiver`; the CLI's asks
   the source's daemon first, op `receive_fork`), and "exit 75 when it is busy" needs
   `FinishError::SourceBusy`. `push_now::handler` takes the daemon's `StateDir`.
5. `receive_fork_change` "returns the change commit", but a fork that changed nothing has no
   change: it returns the finish commit then. A state area at the recorded path that holds
   another volume is `SourceRemoved` (no variant says "replaced"). In the finisher,
   `SourceRemoved` maps to `RefusedReason::RemoteGone` and `Diverged` to `AnotherWriter` (no new
   refusal reason, so the refused runbook needs no new section).
6. "`agentfs init` refuses a result that is `Undeclared`" breaks the documented flow "init, then
   add a remote to the own config" (README and operator-guide quick starts, about 20 test
   harnesses, 12 `ci/` scripts): a remote must now come from an include, or `--local-only`
   followed by removing the declaration. Taken literally, for an existing state area too
   (an init container of a volume that never declared fails until it declares); the docs
   quick starts now pass the remote in an include, and the harnesses drop the declaration
   when they add a remote section. The owner may prefer refusing only new state areas.
7. `none` beside a push section: with `Remotes::pushed()` empty that section vanished from
   `Settings`, so the mount preflight would no longer check its credential files. Resolved: the
   section joins `invalid_remotes` (`RemoteMisconfigured`, never pushed) beside
   `RemotesUndeclared { beside }`.
8. Slice page counts ("about 38 test files", "nine `ci/e2e-*.sh`"): the harnesses that needed a
   declaration were 13 CLI test files, the core and CSI fixtures, `ci/lib.sh` and 11 other
   scripts, and the csi-sanity profile; deploy and chart profiles already name remotes.

## 2026-10-17, F8 follow-ups fix agent (fix-f8-followups)

None of these stopped the work; each was resolved as below and is reported with it.

1. The brief asks for §19.1 rows "dated 2026-10-16"; the owner's answers ("Remotes can come and
   go. It's not a error to have zero remotes", and "Go" on the restatement) are dated
   2026-10-17 in `.handoff/f8-f12-notes.md`. The row is dated as the notes are: "Zero remotes
   (2026-10-16, superseded 2026-10-17)".
2. "No declaration" removes `agentfs.remotes`, `agentfs init --local-only`,
   `Remotes::Undeclared`, `RemotesUndeclared` and CreateVolume's refusal. A state area that
   still sets `agentfs.remotes = none` (made by S3's `init --local-only`) now carries an unread
   key: harmless, never validated (no backwards compatibility, by rule). A linked fork whose
   fork remote cannot be resolved (no flagged section and an ambiguous patch remote) was
   "undeclared"; it is now local-only, as any volume with nowhere to push.
3. Verifier finding 5 (an unlinked fork pushes its branch to the parent's remote) is not a
   code bug against the spec: 5772feb's design text says so and marks it a design choice. It
   is the owner's call and was never restated to him, so behaviour is unchanged and it is
   §19.2 open question 10. The finding's second path (an unlinked fork ignoring the source's
   `agentfs.remotes = none`) is gone with the declaration.
4. Finding 4 (the red `release_scan` test on main): reproduced deterministically by a 450 ms
   sleep after the `commit` before the failing scan (same assertion, `["lost", "retained"]`),
   so it is wall-clock time outrunning the 400 ms backoff, not 5772feb's helper refactor. Fixed
   by reading tokio's clock in `release_scan` and pausing it in the two backoff tests.

## 2026-10-17, unlinked fork remotes (unlinked-fork-remotes)

1. The brief says deleting a local-only unlinked fork "(CSI DeleteVolume or the CLI)" removes
   its history without the hold. The CLI has no delete command and no hold: a CLI volume is
   deleted by removing its state area by hand, for every volume alike. Nothing changed in the
   CLI for deletion; the docs say so. Only DeleteVolume's release decider changed
   (`Worktree::unlinked_fork`).
2. The "Fork link" §19.1 row's design choices (an unlinked fork pushes where a linked one would
   publish; create refuses it without a fork remote) are superseded by the new row "Unlinked
   fork remotes", noted in the old row rather than rewritten.
3. An unlinked fork made before this change (same day) loses its parent's remote: it becomes
   local-only, and with `onDelete: delete` (a fork class's default) DeleteVolume now removes it
   unpushed. No backwards compatibility, by rule; operators with such forks must copy their
   state areas, or recreate them on a class with `remotes`, before deleting their claims.

## 2026-10-18, lease push hardening (beads agentfs-4lb.2, agentfs-4lb.1)

Neither stopped the work; each was resolved as below.

1. Brief for 4lb.2: "pick the mechanism by what the design already records about which branches
   a worktree created". The design records no creator of a remote branch: git keeps none, and
   the push record was rejected as an authority (f8-design.md, "Also rejected"; §11.2). What it
   does record is who made each commit (the `Agentfs-*` trailers, §9), which is also the
   divergence runbook's own A/B test. `push::lease_claim` decides from those (§19.1, "The lease
   push drops no recorded history").
2. Bead 4lb.2 / HANDOFF.md: "it can replace a human branch if `agentfs.branch` names one". A
   human branch made apart from the volume shares no commit with it and was already refused
   (`Unrelated`). One that does share commits holds the volume's pushed commits with a person's
   on top, which is divergence case A by the runbook's definition and is still replaced: git
   cannot tell "a person created it from our commits" from "we created it and a person added
   commits". Not closed by this change; closing it needs a record of ref creation that does not
   exist (an owner call: new state, or limiting `--force-with-lease` to `refs/heads/agentfs/*`).

## 2026-10-18, flaky-test fix agent (fix-flaky-csi-service)

1. `csi_service.rs::the_supervisor_runs_and_recovers_a_real_history_service` (the AGENTS.md
   "known flake") fails on a slow disk at its last assertion, `the final push` (line 232):
   reproduced 6/6 in the dev container with `docker run --device-write-iops /dev/vda:3`; the
   service logs `final push: origin: still running at the push grace's end; abandoned`. The test
   asserts a best-effort outcome (§11.2: one attempt within `--push-grace`, 5 s in
   `launcher::service_spec`) as certain. 0 failures in 186 runs under CPU hogs, 24 parallel
   copies, `--cpus=1` and `dd oflag=dsync` writers: it needs slow fsyncs, not CPU load.
2. Stopping point, a product bug: the guaranteed path (§11.2 "what stays unpushed goes out on
   the next start"; §14.2 the release scan pushes until a push exits 0 at its head) does not
   hold for a local-path remote. The abandoned final push dies with the process inside
   libgit2's local-transport ref update and leaves `<remote>/refs/heads/agentfs/<volume>.lock`;
   every later push to that remote fails `refused: failed to lock file ... for writing` until
   an operator deletes the lock. With the test changed to wait for the release scan to hold
   the head, 4/6 failed this way at 3 IOPS. Fixing it needs a design call: wait past the grace
   for a local push (contradicts "never waits longer"), push local remotes from a child that
   outlives the service, or clear a stale remote ref lock (unsafe while another state area of
   the same volume, F8 moving, may push that branch). Not landed; the test change is left
   uncommitted in the worktree.
3. Follow-up, same day: the main session decided (a design choice, not the owner's words) to
   fix the product bug and land the test change with it. Built: at the push grace's end every
   push is cancelled at its negotiation (`push/stop.rs`), and a push to a local remote already
   past it is waited for up to `LOCAL_UPDATE_CAP` (10 s); design §11.2 and the §19.1 "Push
   timing" row amended. git2-rs's push progress callbacks cannot return an error, so libgit2
   offers no abort point after the negotiation: the brief's "cancel in the transfer callbacks"
   was not available, and the whole local transfer is waited for, not only its ref update.

## 2026-10-29, merge-queue code architect (agentfs-mia.4 .. mia.8 frozen pages)

1. **Keep-going cannot keep today's stale check word for word (mia.2).** The brief
   (`.handoff/mia-features.md`) keeps "inbox numbering, the high-water mark and the stale check"
   unchanged and also orders feature 2, "a conflicting patch is held whole and the rest go on".
   Today (`patch/queue.rs:191-215`, design §12 "Idempotency") an inbox entry at or below the last
   applied number that the branch did not apply is `stale`: never applied, reported, ignored. Once
   held patch `n` is passed and `n+1` lands, the high-water is `n+1` and `inbox/<n>` reads as
   stale. Resolution written into mia.2's page (a choice, revisable): the queue marks a number it
   sets aside with a local ref `refs/agentfs/<volume>/held/<n>` before it judges `n+1`; an inbox
   entry with that mark is judged again, never stale. Every entry without the mark is judged as
   today; numbering, the high-water (highest `Agentfs-Inbox` trailer), the gap rule and the trailer
   done-rule are unchanged. Same root, same bead: `recover_in_flight` tests "the branch has applied
   that number" as `f.inbox <= last_applied` (`queue.rs:669`), which a set-aside number below the
   high-water breaks; mia.2 changes that test to the trailer scan, which is what its doc comment
   already says. Needs the main session's confirmation before mia.2 is implemented.
2. **No CSI profile has a delivery webhook (mia.5).** The brief says notify under CSI goes to "the
   profile's delivery webhook". No such setting exists: the only webhook is fork delivery's
   receiver-side `fork deliver --to webhook` (`fork/deliver.rs`), and the CSI driver never
   delivers (§11.5). mia.5's page adds git-config keys (`agentfs.patchNotifyUrl`,
   `agentfs.patchNotifyCredentialFile`, `agentfs.patchNotifyTimeout`) that any volume, and so a CSI
   profile's config, can set; there is no CSI code in mia.5. Confirm the reading before mia.5.

## 2026-10-29, mia.2 implementer (agentfs-mia.2)

1. **Stopped before any code: the frozen page's precondition is unmet.** mia.2's page opens with
   "Precondition: erratum 2026-10-29 item 1 ... confirmed by the main session. If not confirmed:
   STOP." No confirmation is recorded: not in this file (item 1 still ends "Needs the main
   session's confirmation"), not in the bead (no notes), not in design §19.1/§19.2 item 11, not in
   the computed task brief. The brief `.handoff/mia-features.md` still lists "the stale check"
   among the mechanics to keep unchanged, which the page's held marks change (an inbox entry at or
   below the high-water with a `held/<n>` mark is judged again, not stale), as does the
   `recover_in_flight` applied test. Note §19.2 item 11's "Unchanged" list (commit 4002628) names
   numbering, the high-water and crash recovery but not the stale check, so the record and the
   brief already disagree. To unblock: the main session confirms item 1 (and amends the brief's
   "keep unchanged" line and §19.1 with the held-mark exemption), then re-dispatches mia.2.

## 2026-10-29, main session: confirmations
- **Item 1 (mia.2, stale check): confirmed.** "Keep going" needs it. A local `refs/agentfs/<volume>/held/<n>` mark, written before the next number is judged, exempts that number from the stale check; everything without the mark is judged as today. `recover_in_flight`'s applied test becomes the trailer check its doc comment already describes. Numbering, the high-water mark, the gap rule and the trailer rule for "applied" stay unchanged. The brief (.handoff/mia-features.md) is updated to say so.
- **Item 2 (mia.5, webhook): confirmed.** No CSI delivery webhook exists. Add `agentfs.patchNotifyUrl`, `agentfs.patchNotifyCredentialFile` and `agentfs.patchNotifyTimeout`, settable by any volume and so by a CSI profile's config; no CSI code in mia.5.

## 2026-10-29, mia.7 implementer (agentfs-mia.7, forks of forks)

1. **Stopped before any code: the roll-up's delivery cannot authenticate.** The frozen page has a
   child fork's finish deliver its change with `InboxDelivery::new(fork_remote, parent,
   Some(parent_branch), git)` (`fork/deliver.rs`). `InboxDelivery` is `patch::submit::submit` over
   a `GitRunner`, whose only implementation is `GitCli`: the `git` program, using "the
   repository's remote configuration, credential helpers and SSH setup" (`patch/submit.rs` module
   docs), i.e. a producer's own git. The finisher runs in the node plugin or daemon, and every
   remote operation agentfs itself makes authenticates through libgit2 with the remote's
   `agentfsCredentials`, `agentfsSshKey`, `agentfsKnownHosts` (`remote_auth.rs`, "shared by the
   push and the patch-queue fetch and push"). The `git` program reads none of those keys. So the
   child's submit gets no credential: it fails against any authenticated remote, including the
   CI git server that acceptance test 5 must use (`ci/git-http-server.sh`, nginx `auth_basic`;
   the `cifork` profile's fork remote sets `agentfsCredentials`). Only a `file://` or anonymous
   remote works, which tests 1-4 would pass on while the CSI case, the bead's purpose, is dead.
   Possible resolution (a choice for the architect, not made here): deliver through the
   finisher's own `RemotePusher` (libgit2, the remote's credentials and SSH), which already lists
   remote refs (`list_remote_refs`) and pushes create-only (`push_create_only`): list
   `refs/agentfs/<parent>/{inbox,applied}/*`, refuse early if the change is already listed (at
   most once; the parent queue's duplicate rule stays the guarantee), take
   `submit::next_seq`, push `refs/agentfs/<parent>/inbox/<n>` = change create-only, and on
   `AnotherWriter` list again and take the next number. That changes the page's frozen
   `fork/finish.rs` signature line (no `InboxDelivery` there; `deliver.rs` untouched). The other
   option, a `GitRunner` that hands agentfs's credentials to `git` (`GIT_ASKPASS`, an SSH
   command with the key and known hosts), adds a second credential path for one caller.
2. **A parent whose push target is hidden has no branch to submit against.** `submit` lists
   `refs/heads/<branch>` (`SubmitRequest::branch`, "without refs/heads/"); a fork created with
   `agentfs.forkRefs = hidden` keeps its history at `refs/agentfs/<fork>/history`, so a child's
   submit into it fails `NoBranch` on every try. The page does not say whether such a child is
   refused at create, published as top level, or held refused. The child cannot read its
   parent's push target from its own settings either (only `sourceIsFork = true` is recorded);
   it equals the child's own only while the shared include's `forkRefs` is unchanged.
3. **The CLI cannot create a fork of a fork as the page leaves it.** `fork create` takes the
   child's includes from `create::settings_files(<parent's state area>)`, which refuses
   (`SourceSettingsNotIncluded`) any `agentfs.*` key in the source's own config except
   `OWN_KEYS`; a fork's own config holds `agentfs.fork.source`, `.base`, `.link`, and so on. The
   CSI path is unaffected (its includes are the profile's config). Fix within the page's files:
   add the fork identity keys to `OWN_KEYS` in `fork/create.rs`; the page should say so.
To unblock: the architect re-cuts mia.7's page for item 1 (and decides item 2, and names item 3),
then re-dispatches mia.7.

### Main session, 2026-10-29: mia.7 items confirmed (engineering choices, not the owner's)
1. **Deliver through the finisher's `RemotePusher`.** One credential path (principle: every remote
   operation agentfs makes logs in through `remote_auth.rs`). No `GitRunner` with injected
   credentials. The architect re-cuts the `fork/finish.rs` line as item 1 proposes.
2. **A fork of a parent with `agentfs.forkRefs = hidden` is refused when it is created**, with an
   error naming the reason. Construction-time beats a delivery that fails on every retry. The
   architect names where create learns the parent's push target (parent state area for the CLI,
   the profile for CSI).
3. **Add the fork identity keys to `OWN_KEYS`** in `fork/create.rs`, as item 3 says.
- **Code architect, 2026-10-29: mia.7 page re-cut** (bd design of agentfs-mia.7) for the three confirmed items: roll-up through the finisher's `RemotePusher` (new `RemotePusher::list_refs`, `patch::submit::listed_as`, `PublishOutcome::Submitted`, `PublishedMark.inbox`); `CreateError::ParentHistoryHidden`, read from the parent's own state-area identity for CLI and CSI alike (not the profile); fork identity keys in `OWN_KEYS`. Finding: under item 1 the `NoBranch` reason for item 2 is gone; the refusal stands as a revisable choice.
## 2026-10-29, mia.8 implementer (agentfs-mia.8, merge queue docs)
None of these stopped the bead; each is reported with it.
1. **PR 8 is already merged** (at 5ab0a94, 2026-10-29T23:20Z), so ef53bfe (mia.7) and this bead's
   commit on `docs/merge-queue-design` reach no open PR. They need a new PR from that branch.
2. **`docs/supported-configurations.md` is on origin/main (PR 9), not on this branch**, and the
   rails forbid merging main in. Not edited here; the needed changes are in the bead's notes:
   hook `patch-arrived` becomes `patch-notify`, and its "designed, not built" rows for notify and
   apply, acceptance rules, merge or rebase, branches as patches, keep-going and barriers become
   built (live landing stays designed, not built); the queue row's "stopping at the first that
   fails" becomes keep-going.
3. Stale text outside this bead's files, left as is: `docs/patches.md:23` ("cherry-picks each
   patch"), `crates/agentfs-core/src/patch/mod.rs:4` ("a single commit whose parent ..."),
   `crates/agentfs-core/src/config.rs:915` ("forks of forks are not ...") and `:1246` ("A fork has
   no patch queue").
## 2026-10-30, merge-queue follow-up batch (agentfs-5sg, rrm, g09, ew1/4lb.10, lh8, 93v)
None stopped a slice; each was resolved as stated and is recorded in design §19.1 where lasting.
1. **agentfs-93v (1), the mia.4 page's probe P3.** The page says "P3: skip the parent-count check
   in PatchBranch::read -> test 4 (queue half) fails". Acceptance test 4 is
   `workload_attributes_do_not_steer_the_merge`; the parent-count check is guarded by acceptance
   test 5, `a_merge_inside_the_branch_is_refused` (`crates/agentfs/tests/cli_patch.rs`, the
   "Pushed with plain git, the queue refuses it" half). P3 fails test 5.
2. **agentfs-93v (2), the mia.3 build against its page.** The page has the merge run *without*
   `--no-messages` and read a driver failure from git's message lines, with exit 126/127 reported
   as `DriverNotWorking`. The build (`patch/integrate.rs`) keeps `--no-messages`; pre-checks every
   configured `merge.<name>.driver`'s first word as an executable file (`driver_program`,
   `check_drivers`); and treats only `merge-tree`'s exit 128 with `failure to merge` (with a
   driver configured) as `DriverNotWorking`. A driver that starts and exits 126 or 127 is a
   conflict, because git reads it as one. Lasting: design §12 "Applying a patch" now says so;
   §17.3 "Patch merge" already gave the reason (git 2.43 and 2.47, checked 2026-10-29).
3. **agentfs-93v (3), the mia.5 frozen page's `NotifyCursor`.** The page froze
   `NotifyCursor { delivered_status: Option<String> }`. The build (`patch/notify.rs`) adds
   `undelivered_landed: Vec<(u64, String)>` (inbox number, branch commit; omitted when empty):
   patches landed by a run whose notice failed, which a status does not keep, sent with every
   notice until one is delivered. Lasting: design §12 "Notify" now says so.
4. **agentfs-5sg, ref hiding.** The bead was right: `transfer.hideRefs` hides names, not
   objects. Probed with Apple git 2.50.1 over `file://`: protocol v0 refused an unadvertised
   object, protocol v2 fetched it by id. `serve_git_edges::hidden_refs_hide_names_not_objects`
   shows the same over serve-git's smart HTTP (Debian trixie git). The docs were corrected
   rather than v2 refused: forcing v0 closes fetch-by-id only, not the negotiation attacks
   gitnamespaces(7) "SECURITY" describes, and costs v2's ref-prefix filtering. For the owner, if
   he wants the stronger guarantee: the real fix is a separate repository, not a protocol switch.
5. **agentfs-5sg, beyond the bead.** `Settings` derives `Debug`, and a Debug dump printed
   `patch_notify_url` with its userinfo; refusing userinfo at parse closes that too. Other URL
   fields of `Settings` were not audited here.
6. **agentfs-5sg, fail closed.** An `inbox/<n>` old tip that cannot be read counts as a barrier,
   so only a pusher with `barrier` may replace it (git checks old against the ref, so this should
   not happen).
7. **agentfs-rrm, "raise from the mark".** Read literally (raise whenever the mark says the
   parent finished) it contradicts design §12's alarm table ("once") and the test
   `a_child_after_its_parent`. Resolved as pending then confirmed (`raised` flag), as for
   `Diverged` in §11.3: at least once, at most twice across a crash. `ForkPublished` and the
   publish duration point keep the same crash gap (informational, left out of scope), and
   nothing re-runs a publish at start for a fork already published.
8. **agentfs-rrm, scope.** Adding a field to `PublishedMark` breaks a struct literal in
   `crates/agentfs-csi/src/release_scan.rs` tests; one line (`parent_finished: None`) there.
9. **agentfs-g09 item 4 is not workable as written.** "Record while `agentfs run` is up": `run`
   has no way in for a record (`record` takes the state lock, exit 75; `run` serves no restores;
   `patch apply` under a daemon lands nothing; run's recorder is unused). The same commit loop
   (`service::commit_loop`, shared by `run` and `serve-history`) is driven through
   `serve-history` with `record --history-service` (`cli_checkpoint::a_daemon_commits_on_top_of_its_marker`).
10. **agentfs-g09 item 3, read as a product change, breaks principle 3.** Setting `attr.tree =
    HEAD` in the volume config would let the workload's `.gitattributes` steer merges. The product
    was already version independent (`-c attr.tree=<empty tree>` beats any config value), so
    `attr.tree=HEAD` is set only in the test fixture
    (`integrate::tests::the_heads_attributes_never_steer_a_merge`).
11. **agentfs-g09 item 1** guarded code that already existed: test only, red shown by mutation.
12. **agentfs-ew1 and agentfs-4lb.10 are one fix.** ew1's hang is 4lb.10's mechanism (a closed
    listener keeps listening through any shared fd; only `shutdown(SHUT_RDWR)` refuses). The
    shutdown sits in the stop step (`Server::shutdown` calls `Stopper::stop`), not in the accept
    loop as ew1 says; it also wakes the loop, so the wake-up connect is gone.
13. **agentfs-4lb.10 says no product path closes a listener and then needs connects refused
    in-process. Wrong:** tonic 0.14.6 `serve_internal` keeps its incoming stream through the
    graceful drain, so the history service and agentfs-csi queued connects no one answered,
    with no fork involved. Fixed by stopping the listener at the shutdown signal.
14. **agentfs-4lb.10's "one listener type" is not clippy-complete.** `tokio::net::UnixListener::bind`
    is left off the ban: `ssh_transport/tests.rs` and `tests/history_service.rs` call it, and
    `disallowed_methods` has no test-only exemption. Follow-up: ban it with a reasoned
    `#[allow]` on those two lines. agentfs-csi's `tokio-stream` dependency is now unused.
15. **agentfs-lh8, broader than the bead.** Not only uploads: every ureq read (batch, PUT,
    verify, GET bodies, the fork webhook POST) failed on `EINTR`; downloads hid it by retrying
    `Unreachable`. ureq 3.4's `TcpTransport::await_input` sets `SO_RCVTIMEO` and maps
    `Interrupted` to an I/O error; Linux never restarts such a read after a handler or a stop
    and continue (signal(7)). Who signalled the one observed failure is not proven (a container
    stop or freeze, or tokio's SIGCHLD handler in the all-features build are candidates).
    `lfs::client::tests::transfers_survive_signals` reproduces it. Design gap: nothing said
    blocking I/O retries `EINTR`; transport errors folded silently into the retried
    `unreachable` class (§11.3).
16. **Integrator seam.** The listener ban (12) failed clippy on 5sg's old-daemon fake in
    `tests/serve_git_edges.rs`; it now binds `Listener`.
17. **Integrator, environment.** `agentfs-fuse` `creds::tests::as_root_the_caller_owns_what_it_creates`
    fails in the local `agentfs-dev` container as root (mode 0644, expected 0664: the thread's
    umask is not 0). It fails the same at e9a17f7 (A/B), and no file of this batch is on its
    path; not fixed here.
18. **Brief vs. date.** The integrator brief asked for new §19.1 rows dated 2026-10-29; they are
    dated 2026-10-30, the day they were made, as the existing "Kept 2026-10-30" entry is.
## 2026-10-30, merge-queue follow-up batch, fix round (PR 14 review)
1. **agentfs-rrm's "raise from the mark" did not reach the CSI path.** Built as a `raised` flag
   in the published mark, re-raised only by a later publish. Under CSI nothing publishes a fork
   already published (the release scan skips it, DeleteVolume deletes it), so a crash between
   the mark and the alarm still lost it, while design §12 and the runbook said "never lost".
   Fixed by raising before the mark is written (the flag is gone): a crash in between leaves the
   fork finished, which every publishing path retries. Entry 7 above is superseded.
2. **Entry 14's follow-up done here.** `tokio::net::UnixListener::bind` is banned; the two test
   fakes listen through `Listener::into_async` instead of carrying an `#[allow]`.
3. **Entry 4 under-stated the object exposure.** Under protocol v0 too a pusher reads a hidden
   object whose id it knows: it pushes a commit whose tree names the object (receive-pack's
   connectivity check passes, the object is already there), then fetches the advertised inbox
   ref. Reproduced with Apple git 2.50.1 over `file://` (a pack of only the commit and tree to
   `receive-pack --stateless-rpc`). Design §19.1 "serve-git" and the serve_git.rs doc now say so.
4. **agentfs-5sg's scrub sat in one consumer.** The shared POST helper kept the raw URL in
   `PostError`, and fork delivery's webhook printed it and accepted a URL with userinfo (its
   check was a copy of the notify one from before the refusal). Now one check
   (`lfs::client::webhook_url_refused`) serves both, and `post` redacts its own errors.
   Entry 5's unaudited `Settings` URL fields remain unaudited.
5. **The review's probe of entry 13's fix was ineffective.** It replaced `stopper.stop()` with
   `let _ = &stopper;`, which still moves the `Stopper` into the shutdown future, and a
   finished future drops it, and dropping stops. Its conclusion held anyway: with the stop truly
   removed (`std::mem::forget(stopper)`) every existing test stayed green. Now
   `history_service::a_draining_service_refuses_new_connects` and
   `grpc::a_stopping_driver_refuses_connects_while_calls_finish` go red under that probe.
6. **Deferred: a missing fork base loops instead of being refused** (review finding, P3).
   Refusing `ForkBaseMissing` in `announce_uncarried` alone would patch one instance of a class:
   `Finisher::finish_once` already fails on the same corruption (`find_commit(base)`) with an
   error, not a refusal, and a history push missing the object fails as a retried branch
   failure. Local state-area corruption has no terminal outcome or alarm of its own anywhere in
   publishing or the mount-time queue (`queue_at_start` only prints). A class-level outcome
   (a refused reason or alarm, with a runbook) needs its own bead.

## 2026-10-30, agentfs-4af implementer (Hermes compose suite in CI)

Not blocking; the suite is built and green. Findings about the brief and the docs:

1. **`deploy/examples/hermes/config` cannot be used as-is.** The brief said to use the three
   rule files as-is. `config` names a placeholder remote (`https://git.example.com/...`) and a
   credentials path; its one other setting, `agentfs.nested = exclude`, is the default
   (configuration.md). Overriding the URL with a second include would make
   `remote.origin.url` multi-valued (git pushes to every value; libgit2 reads the last), and
   `url.<base>.insteadOf` would rewrite only for libgit2, not for every reader. The suite uses
   `exclude` and `attributes` as-is and the Docker example's `config/remote` for the remote.
2. **Hermes keeps its sessions in `state.db`, not `sessions/`.** design §16, deployment.md
   §10 and the `attributes` comments list `sessions/` as Hermes' session store. In upstream
   v0.22.5 (image digest `9c5561579f3f…`) sessions live in `state.db` (upstream
   `website/docs/user-guide/docker.md`, "Filesystem requirements for `state.db`"), and
   `sessions/` held nothing during the run. The rules still cover both (`*.db`,
   `sessions/**`); only the prose is out of date. Hermes also writes `response_store.db`,
   `runs_idempotency.db`, `kanban.db` and a Unix socket `gateway.sock` at the top of its home,
   all excluded by the allowlist.
3. **Bind-mounting the rule files into the state volume (as the sidecar does with `subPath`)
   left the Docker volume busy** at `docker compose down -v` on the runner (docker run
   41092510458: `unlinkat .../_data/state/git/info/attributes: device or resource busy`). The
   suite copies them at init instead, as deployment.md §10's migration does. Not investigated
   further: whether the bind survives on the host through the `/agentfs` mount's `rshared`
   propagation; it does not affect Kubernetes, where kubelet owns the mounts.
4. **The brief's topology needed two additions upstream requires.** `hermes chat --oneshot`
   hides `skill_manage` (upstream `agent/oneshot_footprint.py`), so the suite drives Hermes
   through its gateway's OpenAI-compatible API (`API_SERVER_ENABLED`, upstream
   `features/api-server.md`). Hermes' approval check flags a terminal command that redirects
   into any `.env` path (upstream `tools/approval_detection.py`, "overwrite project env/config
   via redirection") and then waits for a human, so the tool container writes `auth.json`, not
   `.env`, as its secret.

## 2026-10-30, agentfs-4f2 implementer (Hermes on every supported configuration)

1. **Product/design bug: no pod restart lands a patch in deployment.md §10's Hermes pod.**
   Hermes and docker-in-docker mount the FUSE mount at `/opt/data` through `subPath`. The
   kubelet's subPath binds (`/var/lib/kubelet/pods/<uid>/volume-subpaths/...`) stay until it
   unmounts the pod's volumes, after every container, the sidecar included, has exited. So
   when the sidecar stops at a pod deletion the FUSE session cannot end within
   `--unmount-grace`, `crates/agentfs/src/main.rs:1614` keeps `meta/attached.json` ("a
   workload may still be attached"), and the next pod's mount defers the queue
   (`main.rs:1471`, `unclean_stop`, `PatchQueueDeferred`), pod after pod. Evidence: CI run
   41102851463, job 124224481251 (e2e (Hermes on kind, sidecar)): the old pod's sidecar log
   at `kubectl delete pod` and the new pod's "patches: not applied at this start". The same
   run's fence job (emptyDir read-only at `/agentfs`, `HERMES_HOME=/agentfs/workspace`)
   lands the patch at the next pod restart, and the csi job at the next NodeStage.
   deployment.md §10 "Restarts and patches" and supported-configurations.md Example 3
   promised landing at a pod restart. Not fixed: the rule is §19.1 "Patches land after a
   clean stop" (working directories count as attached), so the choice (recommend the fence
   pod, or judge a hold only the kubelet's binds keep) is the owner's; design §19.2
   question 12 records it, the docs now say so, and the sidecar job fails on it.
2. **The Hermes rule files record Hermes' bookkeeping in `skills/`.** Upstream v0.22.5
   writes `skills/.usage.json`, `skills/.usage.json.lock`, `skills/.curator_ledger.jsonl`
   and `skills/.bundled_manifest`, and syncs its bundled skills (about 330 files) into
   `skills/`; the allowlist records all of them (the migration Job's log in the run above).
   `.usage.json` changes as skills are used, so it records churn that no one authored.
   Not changed: what Hermes' rules record is policy (deploy/examples/hermes); noted in
   deployment.md §10 "How it is tested".
3. **deployment.md §10 said an empty `terminal.docker_volumes` keeps tool containers away
   from Hermes' files. Wrong for upstream v0.22.5:** `tools/environments/docker.py` binds
   `$HERMES_HOME/skills` read-only into every tool container (`_readonly_skill_mount_args`)
   and, with `container_persistent: true` (the default), the tool container's `/root` and
   `/workspace` from `$HERMES_HOME/sandboxes/docker/<task>/` (`_mount_args`), resolved by
   the docker-in-docker daemon, so on the volume. ci/e2e-hermes.sh step 6 checks it. Prose
   fixed in deployment.md §10, design §16 and supported-configurations.md Example 3.
4. **The fence alternative needs `HERMES_WRITE_SAFE_ROOT` too.** The image sets it to
   `/opt/data`, and `agent/file_safety.py` refuses file-tool writes outside it; with
   `HERMES_HOME=/agentfs/workspace` alone Hermes' file tools could not write its home. The
   image's `main-wrapper.sh` also sets `HOME=/opt/data` whatever `HERMES_HOME` is. Read from
   upstream code; the fence job sets the variable but does not exercise the file tools.

## 2026-10-30, agentfs-vqr implementer (another pod's attached mark)

1. **agentfs-4f2 item 1 is fixed, not open.** A mount whose mount point is in its pod's own
   volume records `AGENTFS_POD_UID` in `meta/attached.json`, and a start in a different pod
   lands the queue over it (design §12, §19.1 "Patches land after a clean stop", amended;
   §19.2 question 12 closed). The docs' "the sidecar job fails on it" notes are gone.
2. **deployment.md §3 "Termination" was wrong twice.** It said a `subPath` consumer makes
   the daemon detach lazily; the unmount is not busy (the kubelet's binds are other mounts
   of the filesystem) and it is the session that outlives the unmount
   (`session_abandoned`), which the log reported as "files were still open". It also
   pointed at Hermes as §9 (it is §10). The log now says "the filesystem was still mounted
   elsewhere" when the unmount was not busy.

## 2026-11-11, module cut step 3c implementer (the root's name)

1. **The 3c design's edit of `future/sandbox-overlay-gaps.md` was wrong.** It said "the main
   mount" becomes "the root's mount". That page is about the older engine's sandbox overlay,
   where the "main mount" is the volume's mount point, not a tree. It now says "the mount
   point".
2. **The ledger harness (`tests/gitledger`) keeps its own fork naming.** The design said the
   ledger stores a `TreeNames` and names trees through it. The ledger re-derives everything
   from git on purpose, so it now takes only the root's name from the world
   (`w.names().name(TreeId::ROOT)`) and keeps its own `fork/<n>` mapping.
3. **AGENTS.md says a new docs page goes in both `docs/README.md` and `docs/SUMMARY.md`**,
   but `docs/README.md` lists no future-design page individually (it links the overview), and
   `docs_reference` checks only SUMMARY. The new `future/root-branch-name.md` is in SUMMARY
   and the target-architecture overview only, as every other future page is.
4. **Step 4's design (unbuilt, outside the repo) predates 3c.** Before it is frozen: the
   root caller is `Caller::Root(n)` and the trace prefix `r:`; its engine-side naming takes
   the root's name from `agentfs_conformance::ENGINE_ROOT`, not a literal; its conformance
   `TreeNames` type should be renamed (for example `EngineNames`) apart from the model's
   `world::TreeNames`; and its D4 ("no trace change beyond step 3") no longer holds.

## 2026-11-11, module cut step 3c fix round

1. **Item 4 above is applied**: module-cut.md's step 4 row now carries each correction, and
   says step 4's architect designs from that row, not from the earlier draft.
2. **Item 3 above stands.** AGENTS.md's "a new page goes in both" disagrees with how future
   pages are indexed; AGENTS.md is the maintainer's to change, so this round leaves it.
3. **root-branch-name.md overstated two things.** It counted "about 29 places outside tests"
   in `tree_actor/mod.rs` (there are 16 lines; `fork/deliver.rs` and agentfs-fuse's
   `tree_view.rs` name `main` only in tests), and it proposed reusing `agentfs.branch` as
   settled, though that key is the older engine's, with its own default. Both corrected; the
   key is now an open question.
4. **3c's light tier was an owner exception, not the tier's test.** The tier's list had a
   clause ("a tree's name or ref, given only through `world::TreeNames`") that blessed the
   change introducing `TreeNames`. It is removed; module-cut.md's 3c row says the full gate's
   test applied and what the light run does not show.

## 2026-11-13, module cut step 6 (implementer and fix round)

1. **`planted/run.py` hard-coded the check names it counts.** The design left run.py out of every
   slice, so a new check was classed `assert` until listed (it bit S1's `mirror_current`). Class
   fix in the fix round: run.py reads the names from the code (`named("...")` in `src/check.rs`,
   messages starting `oracle_<name>: ` or `ledger: ` in the tests).
2. **R9.18 needs a wake the design did not list.** A close or path call inside a transaction
   must wake the fork's waiting takes; without it a take held by that open waits forever and
   `locks` fires. Built in S3; corpus bug `transaction_close_wakes_nothing` plants its absence.
3. **S1 could not keep "anchors clean".** Passing collision refs from `record` to `set_head`
   changed the anchor of `collision_record_in_the_wrong_commit`, re-anchored in S1;
   `judge_skips_one_of_a_takes_changes` was re-anchored after `cargo fmt`.
4. **S2's bound, "the first differing commit is a Transaction commit", cannot be read in
   canonical-text order**: the walk can print later, unrelated commits before the
   transaction's. S2 used "the only changed commit with unchanged parents"; S3 used step-log
   diffs between the two builds.
5. **Row 6's "as the engine's `RootName` does" named nothing on main** (step 5b brings and then
   removes it). The fix round applies the design round's erratum: the `HEAD` refusal in
   `TreeNames::new`, its test cases and the row-6 clause are gone.
6. **The moved count of `random_interleavings_git_merge_base` (ignored, finding 1) went from 15
   to 16 of 64 seeds**: seed 17 is a harness panic ("a model file in git") under that rejected
   policy, only after S3. VERIFYING.md is updated.
7. **The fix brief's "tag R9.18 and R10.11 `approved`, as R10.5" rests on a misreading**: R10.5
   is tagged `owner` on main and here, as is R11.5, though both rest on a letter answer to
   agent-worded options. R9.18 and R10.11 are now `approved`; R10.5 and R11.5 are left for a
   ledger-wide tag pass.
8. **No corpus bug can isolate R10.5**: the referee takes no direction, so a transaction's
   commit and a landing are one code path; `line_follows_the_upstreams_unfinished_last_line`
   switches R10.4 off for both.

## 2026-11-13, Part B: a transaction recorded as one commit

1. **git-model §9 always merged a transaction's commit**, against R6.2 under R10.11 (a landing on the current head is
   a fast-forward). The model now fast-forwards the fork to T when the fork is still at begin, and the check
   `transactions_end_as_told` fails a merge made at begin.
2. **git-model §4 and §9 named `refs/txn-start/<n>`**, which the model never wrote. Gone: `txn/<n>` itself now holds
   begin and never moves.
3. **MODEL.md rule 12 replayed a transaction's open "into the transaction's branch"**, and the model kept a
   transaction alive across a crash, against R5.3's recorded pick ("the crash rolls the transaction back and applies
   the call", 2026-11-05) and A13. The design for this part recommended keeping survival (its Q2); the review showed
   that anchors on the code against the ledger. A crash now rolls back every open transaction and applies its deferred
   calls; `r12_a_crash_saves_an_open_inside_a_transaction` and `r5_a_crash_keeps_what_a_transaction_deferred` became
   `r12_a_crash_rolls_back_an_open_transaction` and `r5_a_crash_rolls_back_and_applies_what_it_deferred`, and the
   corpus bug `crash_drops_a_transactions_journal` (now the correct behaviour) became `crash_keeps_an_open_transaction`.
4. **The design's `Agentfs-Tree: txn/<n>` on T broke the independent landing ledger** (`tests/gitledger/mod.rs`) and
   `upstream_wins` once T is fast-forwarded into the fork and lands upstream. T names its fork; `upstream_wins` reads a
   transaction's loser by its `Agentfs-Cause`, and the ledger reads a one-parent `Transaction` commit as a fast-forward
   take.
5. **The design's "reachable commit naming `txn/<n>`" crash marker was redundant**: the ref's absence already says the
   transaction ended, and a crash now rolls back whatever still has its ref.
6. **`transactions_frozen`'s first cut required begin to stay an ancestor of the fork's head.** A sync may fast-forward
   past the fork's own sync merge (R6.1, 2026-11-09), so begin can leave the fork's history (seeds 20, 54, 61). The
   clause is gone; the ref's value is the check.

## 2026-11-13, Part B fix round (review and verification of 0f63eb6)

1. **git-model §9 gave T's `Agentfs-Open` trailers a replay purpose ("so a crash's replay never saves those opens
   again")**, against §3, which says the trailer is for the audit and decides nothing (the journal's closing record
   does). §9 now says "for the audit". The model's `saved` still reads the trailer, its simplification of §3.
2. **The design's check row "T names each staged open" was not built**: `one_writer` skips a transaction's commit.
   `transactions_end_as_told` now compares T's `Agentfs-Open` trailers with the opens its staged saves name, in order
   (a probe dropping T's opens fails 23 scenarios there).
3. **The crash rollback of every open transaction was presented as the owner's pick.** His pick (R5.3, 2026-11-05)
   covered a transaction with a deferred call; extending it to every transaction is the model's choice, now labelled
   so in §9, R5.3, R12 and git-model's Questions for the owner, with its principle-1 cost.
4. **"Recorded as one commit" vs. T plus a landing merge**: when the fork moved since begin, the fork gains T and the
   merge `[fork, T]`. R5.5 and git-model's Questions for the owner now say so, as the model's choice for the owner to
   confirm.

## 2026-11-13, step 5b S1 (engine names every tree by its identity)

1. **The design says 5b "does not touch root-branch-name.md"**, but that page's "What stands until then" said the
   engine names the root `main` and that 5b would replace it: false once S1 lands. S1 rewrites that one paragraph in
   the present tense (`root`, `refs/heads/root`, `Agentfs-Tree: root`); the page's proposal and M4's ownership are
   unchanged.
2. **The design says S1 "changes no step of the save path"**: not quite. `enqueue_moves`, now written from the
   upstream map, writes a sync's marker blob only when the tree has forks; the old root branch wrote it on every
   root save, read by nothing when no fork existed. A root save with no fork runs 15 git children, not 16
   (`tree_stages.rs` `CHILDREN_PER_SAVE`). Recording is unchanged: the same commit, refs and journal record.
3. **The design's grep gate covers only the engine files.** `world.rs:399` (the model, S2's file) still names
   `agentfs_conformance::ENGINE_ROOT`, which S1 deletes; S2 rewrites that comment with `TreeNames`.
4. **The finding-51 regression patch said "agentfs-core unchanged since 7474470"**, false after S1. Its hunk now
   applies at line 1761 (`git apply --check` clean); the recorded trace was not re-recorded (it holds model calls,
   not engine names).
5. **The design's §8 table has `rebuild_owners` read "TreeId::Root's queue"**; kept as written (landings queue only in
   the root's queue while forks fork only the root). A fork of a fork would need every upstream's queue there.

## 2026-11-13, step 5b S3 (baseline pages)

1. **The brief says S1 and S2 are landed; S2 is not** (branch head eeec466: `world::TreeNames::new(root: &str)` still
   takes and checks a configured name, and `world.rs` still cites the deleted `ENGINE_ROOT`). S3 lands only the edits
   that hold after S1: git-model.md and rules-ledger.md. MODEL.md:3–4 ("branch `refs/heads/root`") and module-cut.md
   (row 5b "Built"; "it is `root`, as in the engine") describe S2's result and wait for S2's commit.
2. **The design's §4 edits predate the plan PR's rewording** of git-model.md (the tree row, the refs table) and
   rules-ledger.md:35–36, which already say "the name it is published under is configured". The branch wins: only
   `<root>` becomes `root`; the root-branch-name links in the refs table and the ledger stay. git-model's `Agentfs-Tree`
   comment keeps the branch's "(a transaction's commit names its fork)" (0f63eb6) and drops the design's `txn/<id>`.
3. **The design's "nothing else on them changes" misses git-model §1's diagram**, whose node read
   `refs/heads/(the root's name)`: now `refs/heads/root`, the same edit as the table. system.md:78 (`Root, //
   refs/heads/<root>`) is a design page outside §4's list and is left for its own step.

## 2026-11-13, step 5b fix round (S2, review findings)

1. **S2 had not landed when S1 and S3 were reported done.** Built here: `TreeNames::new()` takes no root (a unit
   struct; `name(TreeId::ROOT)` is `MODEL_ROOT`), the reserved-name assert, `the_roots_name_changes_no_digest` and
   `tree_names_refuses_a_fork_or_transaction_name` are gone, and the callers drop the argument. MODEL.md, VERIFYING.md
   and module-cut.md (rows 3c, 4, 5b and the module text) now say the root is `root`.
2. **The design's §9 says "a plain `pending_from` test" proves the queue prefix.** With one fork it could not tell
   `tree_trailer(..) == Some(tree)` from `.is_some()` (a mutant survived). The test now holds a second fork and checks
   `pending_from(other) == false` while the first fork's landing waits; the mutant fails it.
3. **S1 generalised `enqueue_moves` through the upstream map while `take_next` and `rebuild_owners` read only the
   root's queue as landings** (two deciders). Forks are made only of the root, so recovery now refuses (`Format`) an
   upstream map entry whose upstream is not the root, or whose fork has no branch (the reviewer's unbounded-queue
   case). Forking a fork needs both readers widened with it.
4. **Compaction dropped a page write whose tree did not parse** (`is_ok_and`). It now fails before the rename, the
   journal untouched, as every `CompactError::Before` does.
5. **`tree_git::missing_objects_names_what_is_lost_of_a_tip_and_not_its_parents` is not a root-user failure.** It fails
   on the host's git 2.43.0 (`fatal: bad object` for a missing tip given to `rev-list --missing=print`) and passes 5/5
   as root in `agentfs-dev` with git 2.47.3 (`ci/git-release`). `TreeGit::missing_objects` therefore depends on a git
   newer than 2.43; not fixed in 5b.

## 2026-11-13, module cut steps 5.1-5.3 fix round (verification and review of b975dec)

1. **The 5.1 page's probes R2 and R6 name only the differential test, which 5.1.4 deletes.** Verification at b975dec
   planted both (`verdict` answers Apply where the upstream holds the same version; `displace` frees a path for any
   unlink) and both survived every remaining test and a local library replay (0 mismatched); the oracle at 3ac3bae
   fails R2 at seed 20. Two rules had lost their only guard. Unit tests now close them:
   `verdicts::the_upstream_holding_the_same_version_drops_it`,
   `referee::tests::only_a_rename_over_onto_the_name_frees_its_path`, and
   `tree::combine_keeps_its_head_and_refuses_a_version_recorded_twice_differently` (N1). The rule for later cuts:
   before an oracle or differential test is deleted, each rule it alone guards gets its own unit test.
2. **Departures from the 5.1 and 5.3 pages, behaviour-neutral, not logged when built:**
   - `placing::place` takes `(upstream, incoming, verdicts)`: the page's `base` parameter is unused and was dropped.
   - `queue::Cut::taken()` is not built: no caller reads the cut's taken step without consuming it (`into_taken`).
   - `LandedInto` and its fields were `pub(in crate::tree_actor)`, wider than the page's private struct; it is built
     only in `history/take.rs`, and is now private there as the page has it.
   - Corrupt line ancestry (missing ancestor, cycle, conflict) reads `Error::Format` where it read
     `Error::Reconcile(InvalidMap(..))`; both answer EIO. Stated on the page as a cost; recorded here as built.
   - `refs::branches` parses every `refs/heads/` name before any snapshot is read, where the old loop interleaved
     them: on a corrupt volume the first error reported can differ (both `Format`).
3. **`parse_tree` (5.2's move table puts it in `journal/fold.rs`) made `history/refs.rs` and `history/message.rs`
   import from `journal/`, while `journal/fold.rs` imports `history::upstreams`: a module cycle.** It is gone;
   `impl From<NotATree> for Error` (`Format`) beside `TreeId` lets every reader write `text.parse::<TreeId>()?`.
   `Upstreams` stays in `history/upstreams.rs` as the 5.3 page places it, so `journal` still reads `history` one way.
4. **Commit 3d4a439 (the `ci_pipes_never_end_in_an_early_exit` test and the `ci/*.sh` pipefail edits) sits in the
   5.1-5.3 range but belongs to none of the steps.** It changes no engine code; the PR description should list it.

## 2026-11-13, M1 S2 (release figures): the §8 workloads do not finish in the tracer job

1. **The design's §8 says the flat workloads are "scaled to finish in the tracer job"; on the release image they do
   not.** Image run 43500960004 (commit 6d092da, both legs cancelled at the job's 45-minute limit): `close_4k` took
   15-18 s and reported p50 75.3 ms, p99 91.4 ms on ext4 and p50 88.6 ms, p99 107.2 ms on XFS (the recording
   engine's budget is p50 3.5 ms). `create_1000` then ran for 44 minutes on each filesystem without finishing; a
   local debug build had created 482 of the 1,000 files after about 19 minutes. Every save rewrites the whole tree
   (performance.md, "Large files in the tree are slow"), so a save's cost grows with the files in the tree and the
   1,000 creates cost about the square of the count. `close_64m` never ran.
2. **The S2 page's linear extrapolations follow from the same wrong premise.** `extrapolated_100000_s` scales
   `create_1000` linearly, but the per-save cost grows with the tree, so a linear figure understates 100,000
   creates by orders of magnitude.
3. **What a re-cut has to choose (the frozen schema fixes `count: 1000`):** a smaller create count, or a deadline
   with the count reached in the report; how to extrapolate a cost that grows with the tree (or no extrapolation);
   and whether `close_64m` gets its own deadline. The commit was reverted on the branch (it held the image run red);
   the work is on `wip/m1-s2` (6d092da). The local mutation probe passed: a 50 ms sleep in `run_step` moved a debug
   build's `close_4k` p50 from 47.6 ms to 99.3 ms and flipped `warn` against a 70 ms bar, so the timed window holds
   the save.
4. **The guarantee string `committed before close returns` is not literally what the view does.** The view saves a
   file on FUSE `release`, which the kernel sends after `close(2)` returns; the save is committed before the view
   answers any later request (one session thread, requests in order), which is what the tracer's step 2 and
   `trees-figures.py`'s timed stat rely on. The report's frozen string should say that, or the design should.

## 2026-11-14, speed B3 (file maps and segments): dispatched before the slice it needs

1. **B3 was dispatched, but B2b is not built.** The frozen pages (`speed-pages.md` §0) order the slices B2a, B2b,
   B4, B3, and give B3 "Needs: B2b". On 6e4ce1e only B2a is landed: `History` (`tree_actor/history/built.rs`) and
   `Recording`'s B2b methods (`call_path.rs`) are still `todo!("B2b")`, and `tree_actor/history/builder.rs` does not
   exist.
2. **B3's page cannot be built on B2a alone.** Its own files and interfaces rest on B2b's: `BaseFile` is "the
   checkout's file as built" (B2b's private checkout and built point); `FileMap::rebase` runs "at a built event";
   segments drop "once all its records are `<= built`"; the only change to `history/builder.rs` (a B2b file) is
   streaming `FileMap::reader` into `Feed::blob_from` (a B2b type); the save record it changes,
   `Saved { len, crc32 }`, is B2b's `SaveOutcome`. Building B3 now means building B2b's builder, built point and
   checkout first, which is B2b's bead, not B3's.
3. **What resumes it:** land B2b (and B4, by the page's order), then dispatch B3 unchanged. No re-cut of B3's page is
   needed. Nothing of B3 was written.

## 2026-11-14, speed B2b (the trailing builder): the lag formula, the page's missing parts, the gate's cost

1. **The lag formula cannot be measured as written.** §4 gives lag = steps x ns_per_step + bytes x ns_per_byte,
   "both measured on the last batch". One batch's time cannot be split between two rates. The WIP fixed
   ns_per_byte at 10 ns and measured ns_per_step as the remainder; while a commit writes the tree's whole listings
   (until B4), a step's cost grows with the tree, the early small-tree batches set a low step rate, and the view
   admitted about 8,000 creates in 10 s that took the builder over 300 s (the tracer's create figure held past its
   deadline plus grace, connection aborted, image run 43560396695; reproduced locally). Built instead: each rate
   measured as if it bore the whole batch, lag = the larger of the two estimates (`history/lag.rs`). The estimate
   errs high, which only holds a call until the next built event. A re-cut should state the formula.
2. **"A waiting call holds no other request" (finding 2, docs) is not what the kernel does.** The VFS holds a
   directory's lock while a create, link, rename or unlink in it is outstanding, so a lookup or listing of that
   directory waits behind a gated create. docs/trees.md now says so.
3. **The WIP does not build all of §4.** Takes still settle and decide on the actor under its lock (no `takes.rs`,
   no `Decision`/`land`/`Refused`), and most of the acceptance table's tests and mutation probes 2, 4, 6 and 7 do not
   exist. The brief's "fallback split" for B2b is not in speed-pages.md or speed-design.md.
