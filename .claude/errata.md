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

## 2026-11-14, speed B2b rest (takes off the actor, §4's tests and probes)

1. **A sync reads its upstream's head, which §4's land rule does not check; checking it starves syncs.** A sync
   decided before an upstream save lands at the older upstream commit, which is not what an atomic take at the land
   would do. A first build refused it (`HeadMoved`); on CI (phase-1 "two-writer page cache", run 43572654936) a busy
   root refused every sync, the fork's queue grew to 70 entries, and draining them (each take planned a sync per
   entry, with git children) outran the test's 30 s. Built instead: a landed sync equals an atomic sync taken at its
   decision (it reads nothing of its own tree that changed after it, the fork's head has not moved for a
   fast-forward, and the upstream's later saves queue later entries), and a fork's entries are planned once per take
   (each is the same sync marker; P1 takes the first or none). The page's claim "equals an atomic take at the land"
   holds for its own tree only. `TakeInputs` carries the fork's `upstream`.
2. **`head_at` cannot name the head the decision saw.** A snapshot knows oids, not the step that set them. Built:
   `TakeInputs::through` (the last record when the inputs were read); the decision waits for history built through
   it, `head_at` is the inputs' head step, and `land` compares the actor's current head step with it, which refuses
   whenever the head moved after the inputs (conservatively, also for a move before the snapshot).
3. **Pruning `touched` at built events loses a refusal.** A key touched after a decision's built point but before the
   next built event is pruned before the decision lands. Built: `touched` holds keys only while the tree's queue holds
   an entry (a decision reads a queue as built, so an earlier step precedes it), and is pruned at each land to steps
   after the built point; takes are one at a time, so no decision is in flight then.
4. **A close that changes nothing lets a held key go without touching it.** `Shape::save` gives no keys for
   `Unchanged`, so an entry that a held key blocked would stay passed over. Built: every close touches its object's
   keys, whatever its outcome.
5. **`decide` returns no waiting entries**, which the wait metrics need (`ci/trees-conformance.py` reads `waits`).
   Built: `take_one` counts the built queue's entries before the decided one (or all) as waiting.
6. **The restart hypothesis is killed, and 10,000 files cannot be built in a test before B4.** Open settles, so its
   time is building the tail past the checkpoint at whole-listings cost: debug, 0.19 s for 501 files with 1 step past
   the checkpoint, 2.6 s for 600 with 100, 8.8 s for 2,100 with 100 (hypothesis: under 5 s for 10,000). The worker
   that builds 2,000 files took about 85 s (one commit's listings per step). `restart_replays_from_the_checkpoint`
   uses 500 + 100 files; re-measure after B4.
7. **No failpoint can hold the builder** (actions are kill, error, panic, sleep). `a_save_is_visible_before_it_is_built`
   holds it in the journal fsync it makes before each batch (`Durability::hold_fsync_with`), an event.
8. **Probe 1 survived the named crash test**: a rebuild after the kill makes the same commits, so refs naming them
   early is invisible after reopen. `a_step_killed_after_its_record_is_not_saved_again` now reads the refs on disk
   before the reopen.
9. **Not built from §4:** `TreeActor::{create, open}` stay public (the integration tests and the conformance library
   drive `TreeActor` directly; `TreeActor::take_next` runs the same three parts with no lock to release);
   `the_gate_holds_only_its_tree` is the lag gate's unit test, not the page's view-level test of a tree's write and
   RELEASE passing a held create.

## 2026-11-14, speed B4 (hashed sub-folders): the owned files, the builder's per-step clone, objects of other trees

1. **The owned-files list is short.** Moving object numbers into the checkpoint needs `journal/mod.rs` (the
   `Entry::Checkpoint` field lives there, not in `fold.rs`), `journal/commit_point.rs` (compaction writes it),
   `history/built.rs` (`BuiltSnapshot::objects`, since takes and the actor's open read objects as built),
   `history/take.rs`, `takes.rs` and `tree_actor/mod.rs` (the object table pointer, start-up, the format bump to
   `agentfs-trees-7`). Tests outside the list read the layout (`tree_actor.rs`, `tree_line_rules.rs`,
   `tree_pages.rs`, `support/trees.rs`), so do the conformance oracle (`invariants.rs`) and seven `ci/` scripts.
   B3 was not running, so nothing collided.
2. **The page's builder cost assumed what B2b did not do.** B2b's builder cloned the whole built tree for every
   step (names, bytes, line maps, name histories: O(files) per step), which alone would miss the creates bar. Built:
   steps apply in place on the batch's copy (`builder.rs`, more than "calls format only"); the per-batch copy and
   the per-batch object map stay O(files) per batch.
3. **`edits` takes keys, not `before`.** The frozen `edits(before, after, changed: &BTreeSet<u64>, blobs)` needs the
   tree before the step beside the tree after, which is the clone of 2. Built: `edits(after, changed:
   Option<&BTreeSet<queue::Key>>, blob)`, the keys each transition already returns (`Shape::apply`, `Shape::save`;
   a take's from its record), `None` for the whole tree (after a fast-forward only; no longer at start).
4. **`read_snapshot` of another tree's commit has no objects to read.** A landing gives a file new to the upstream
   the incoming file's object, and a sync a file new to the fork the upstream's; both came from the commit. Built:
   the writer tree's objects as built, else the identity's own number (the object its create made). It differs from
   the old reading only when the writer's tree moved that file's object after the commit.
5. **Start-up needs objects at the built log's point, past the checkpoint's.** Built: `fold::objects_through` reads
   the trees at the checkpoint's pointers and replays each step's own transition to the built point: a second whole
   snapshot read when the built point is past the checkpoint. Reopen of 600 files (100 past the checkpoint), debug:
   0.46 to 0.48 s (B2b: 2.6 s); of 10,000 (100 past): 5.9 s, over the 5 s hypothesis, spent reading one blob per
   file and name history, up to three times (checkpoint, built point, the actor's open).
6. **The conformance oracle lost the real object numbers.** Nothing in git names them now. Built: the oracle follows
   a file across a take by its identity (one that stays keeps the oracle's object, one a take brings gets a new
   one); `one_object_one_file`, read from the object table ref, is gone (the engine's `Tree::install` still refuses
   two files on one object).
7. **Acceptance naming.** `an_empty_files_first_alignment_runs_no_child` is two tests: the no-child count in
   `tree_stages.rs`, and `an_empty_old_image_aligns_as_git_does` (hunks equal git's on nine inputs) in
   `tree_reconcile.rs`. Objects per 1-file save, counted in the pack: 10 (blob, line map, seven folders, commit), not
   hypothesis 6's six; fed by mark: 2. `checkout_path` is not added: it has no consumer before B3.
8. **Cost not on the page: a snapshot read is one blob read per file.** Takes read whole snapshots (a sync plans
   over each upstream commit the fork lacks, a landing reads base and incoming): where one `metadata.json` read was
   one blob, it is now a blob per file and per name history. Not measured at size; the take tests are small. A
   reader that reads only a commit's changed folders is the follow-up if takes on large trees prove slow.

## 2026-11-14, speed B3 (file maps and segments): where the page is short, and what was built instead

1. **The owned-files list is short.** Beyond §6's list: `history/mod.rs` (`read_trees` takes each file's map
   from a closure; `checkout_path`), `history/built.rs` (the maps the builder hands the actor), `history/feed.rs`
   (`Feed::blob_from` did not exist), `tree.rs`'s `Tree::install` (a map per file from a closure), `takes.rs`
   (a take's files), `tree_actor/mod.rs` (format `agentfs-trees-8`, serve from the builder's maps, the open-files
   limit), `failpoints.rs` (new points), `ci/e2e-tree-recording-faults.sh` (renamed compaction points),
   `.github/workflows/image.yml` (the tracer job's timeout, 20 to 25 min) and AGENTS.md's figure.
2. **`Source` has no home for a take's merged bytes.** They are in the take record's head (JSON line maps), not
   in a tail. Built: a take record's tail holds the bytes of each file it leaves (`TakeRecord::tail`, `spans`),
   and the actor's and builder's maps read them there.
3. **`rebase` cannot work per piece as written.** "Pieces from steps `<= base.step`": a piece does not know its
   step, and a call's record is older than the close that saved it, so a record-numbered rule would read a later
   save's bytes from an older base. Built: a map carries the step of the save it is (`None` once it holds unsaved
   calls), and `rebase` replaces a whole saved map whose step is at or before the base's.
4. **Who rebases, and when.** The page leaves it to "a built event". The actor cannot know which checkouts are
   current (a failed copy leaves them stale) nor which files changed. Built: the builder, after each checkout,
   opens a `BaseFile` per checked-out file that still reads frames and rebases its own map; every map it changed
   or rebased goes to the actor through History (`publish_maps`, `take_maps`), which takes each at its next call
   where its saved map is the same step. An open whose map is still that save takes it too. Recovery serves the
   builder's maps, so a checkout a failed copy left stale is never read.
5. **`Saved { len, pieces }` cannot be "a hash of the piece list".** The two sides rebase at different built
   points, so their piece lists differ in what the save kept. Built: an FNV-1a digest of the length, each piece
   from the close's own frames with its offset and place, and the kept bytes as one run per gap
   (`FileMap::digest`). Cost: it checks the apply, not the kept bytes' content (the earlier saves were checked).
6. **Recovery needs the checkpoint to name pre-checkpoint frames.** A checkpoint is no longer first in the file,
   and carried or kept frames of a closed generation's page writes must stay out (R12). Built: the checkpoint
   lists the held page writes (`page_writes`); a live open's calls are found by its handle. Found by probe 3:
   carried frames could push the checkpoint's segment (named for the next record) past 64 MiB, and the next
   record's roll then tried to create a segment of the same name (`EEXIST`); fixed, with a unit test.
   "Compaction becomes: write a checkpoint frame, drop segments" also loses what the old rewrite gave: a
   restart's frames may be only in the kernel, and the rewrite's fsync made them durable without a leader fsync
   (`ci/e2e-tree-recording-faults.sh`'s `tree.wal_before_fsync` kill met the startup's instead). Built: the
   checkpoint fsyncs every segment and the directory, then raises the mark to what it covers.
7. **One descriptor per file.** `BaseFile { file }` holds one open descriptor per non-empty built file and
   tree: 50,000 for the creates figure. Not on the page. Built: the volume raises the soft open-files limit to
   the hard one at serve (as the Go runtime does); a failed open logs `tree BaseFile` and keeps the frames.
   `BaseFile.blob` is an `Option`: at start the builder knows no blob (it does not hash checkouts).
8. **`FileMap` is public.** The view (`agentfs-fuse`) reads through it with the actor unlocked, so it cannot be
   crate-private as §6 has it. `read` now returns the bytes whole (`Vec<u8>`); `file_map` and `read_open` give
   maps.
9. **Probe 3's figures counter cannot fire.** A compaction waits for the builder to catch up, and calls do not
   wake the builder, so none runs while the 1 GiB file is written. `close_1g` reports `carried_bytes` anyway; the
   test's inverted case is the detector.
10. **The space estimate is short.** "About 3 GiB for a 1 GiB close": the line map is JSON with a number per
    byte, so a 1 GiB random file's pack is about twice the file. The tracer's loop filesystem is 8 GiB, not 6.
11. **Smaller departures.** The segment-rollover crash test is in `tree_actor.rs` (`crash_points.rs` is the
    recording engine's harness); it fails the leader at `tree.segment_after_create`, between a new segment and
    its directory fsync. A compaction that fails dropping segments no longer stops the actor (nothing is
    unknown); its failpoints are `tree.compact_before_checkpoint` and `tree.compact_before_drop`. `close_1g` times
    the close and `stat` after the writes (the engine's dirty bigclose); `write_1g`'s "a 1 MiB file's writes" is
    32 files of one 1 MiB write. Page writes' bases (kernel page snapshots) stay in memory; the image is a map.

## 2026-11-14, speed fix 1 (fix-plan.md R1 to R3): where the plan is short, and what was built instead

1. **R1 changes what `lines/` holds, not what history says.** A line map is now runs of lines (identity and
   version numbering on, `dead`, `open` length for an unfinished last line) and runs of ancestors, never bytes
   (docs/trees.md, "What a commit holds"). Bytes come from `files/`. The model's digests do not read the library's
   layout and are unchanged; the conformance reader (`git_history.rs::parse_map`) and `ci/trees-conformance.py`
   expand runs over the file's bytes. The state format is `agentfs-trees-9`. Lengths are not stored per line (a
   line ends at its newline): a per-line form cannot meet "memory independent of file size" for a 1 GiB file of
   short lines (millions of entries), so the established pattern is git blame's ranges of lines.
2. **"Bounded independent of file size" holds for the builder process, not for every git child or a take.**
   Measured (one container, release): agentfs 15 to 17 MiB at 64 MiB to 1 GiB; `git fast-import` and
   `git cat-file` were made to stream (`core.bigFileThreshold=8m`, a 16 MiB pack window for `cat-file`), which
   took the children from about 3x the file to 66 MiB. Still unbounded, and documented: an edit that is not an
   append runs `git diff`, which reads both versions whole (git's xdiff), and a take expands its files' line maps
   line by line (the referee works per line) and reads the files it merges. The bar names saves; takes and edits of
   huge files would need a streaming diff and a run-level referee.
3. **Take tails: chunk records, not a multi-frame tail.** A take's merged files are written from their sources
   (`tree_reconcile::pieces`: a version's bytes are the same in every file that holds it) into `Entry::Bytes`
   records of at most 16 MiB recorded just before the take step, with no compaction between (`append_record`);
   `TakeRecord` gains `lens` and `chunks`. The take worker reads source blobs over 8 MiB through `git cat-file`
   into an unlinked file of the state area.
4. **R2: open compacts only when nothing is unbuilt.** A journal built through its last record compacts at open as
   before; otherwise the threshold is 0 and compaction runs at the first record after the builder catches up. The
   library's and the mounted replays' `crash` now settle after the reopen (they read git after every call).
5. **R3b's class is gone with R2, not fixed in the builder.** Reopen failed because `serve` required a map the
   builder had published for every named file; a linked anonymous file's map is empty and never published. Serving
   from the journal's replay removes that requirement; the builder is unchanged there. The red test now fails only
   if the replay's `Change::Link` arm is broken (probe run).
6. **R3a, mechanism:** a compaction records each step value the pointers held and the oid it was built as; a step
   whose pointers were read before it names those oids (`TreeActor::resolve_compacted`, run in `record_step`
   after the compaction). `Pointers::check_targets` still tolerates step-versus-oid (left for Fix 2).
7. **Validation tightened:** a line map's ancestry is acyclic because every version is over an earlier writer's
   (checked by event number, not by walking chains). Every map the library writes satisfies it.
8. **A `wip(trees):` commit (24ee3f5) is in the branch's history** (a checkpoint pushed to `wip/speed-fix1` before
   the slice was green); it was not rewritten.

## 2026-11-14, speed fix 2 (fix-plan.md "Fix 2"): what was built instead, residuals, and the errata's state

1. **P2-2, built instead:** compaction waits for every *step* built and the builder past the oldest segment, not for
   history built through every record. Records after the last step (calls, opens, page writes) are what the checkpoint
   holds, so its redo start stays its last record; segments are dropped only up to the builder's built point (it reads
   them). The segment that holds both dead records and calls after the builder's last batch waits for the next step
   (bounded: one segment). Once the journal is twice its due length with steps unbuilt, the lag gate holds every
   tree's new work until the builder catches up (an engineering choice, not the owner's). Steps replayed at open now
   count in the gate (Fix 1's hand-over).
2. **`check_targets` strict:** `compaction_keeps_the_fold`'s "lands on either start" premise was false after R3a:
   records after a checkpoint name the values it covers as oids, so they fold only on top of it. The test now shows
   the mixed fold is refused.
3. **Coherence 3 (frame relocation owned by the journal), residual.** A live open's frames carried forward are still
   read through the old segment's `Arc` until the open closes (the actor's `Handle.calls`, page runs, the builder's
   opens), so a carried segment's space returns only then, and a statvfs test would fail today. Plan: `FramePos` names
   (record, offset) with the save digest keyed by it (not by segment and byte), the journal keeps a relocation table
   that `drop_behind` fills, and the actor and the builder (at its next batch, through History) rewrite their
   positions from it; then a space test by statvfs. About half a day; it touches the digest, so the model-free
   conformance replays are the check.
4. **Coherence 5 (`TreeActor::{create, open, take_next}` crate-private), residual.** Open's durability is now one
   path (`TreeActor::serve` makes recovered records durable), but the integration tests (about 1,000 call sites), the
   conformance library and `tree_view` construct `TreeActor` directly. Plan: a test-support handle over `Recording`
   that locks per call, the conformance `Library` on `Recording`, then `pub(crate)`; `TreeActor::take_next` and
   `progress` go, `Recording::take_next` stays the one take path. B2b rest's item 9 stays with it.
5. **P3-4 (power-loss-trees never crosses rollover or compaction), residual.** CI-only (`power-loss.yml` after
   landing). Plan: a 70 MiB file in `ci/power-loss-trees.sh` before the marks, a mark kind for it in
   `power-loss-trees-check.sh`; the 512 MiB data image holds it. This is also the true detector of probe A (an fsync
   left out of the checkpoint); locally probe A is killed by a per-thread fsync count (`Segment::sync`).
6. **P3-6, partly:** the builder drops heads the checkpoint covers; `owners` (each direct save's commit) still grows
   with saves and is cloned and resolved whole each batch. Plan: share it as an `Arc` and resolve only the batch's
   new entries.
7. **New finding, not fixed:** the view holds about 1 GiB for the 50,000 files the `create` workload leaves (about
   20 KiB a file across the actor's and the builder's trees; `memory_1g`'s `peak_mib` 1,106 at d7666ef). Not in the
   plan; a memory-per-file figure is the first step.
8. **Suspicions settled:** `whole` lost at restart is correct (the trees are read from the built heads, which match);
   a hole in an early segment no longer reads every later segment into memory; a checkpoint's objects map passes the
   96 MiB frame limit only at millions of files (the compaction then fails before its checkpoint every time: a
   residual, not fixed).
9. **Not run locally:** `ci/check-tree-git.sh` (needs the pinned git 2.47.3); the tracer and figures (docker and a
   loop filesystem; the inotify wait was run against a simulated writer only).

Errata state, after this fix (each speed-epic entry the handoff listed as open):
- 2026-11-13 M1 S2: closed by B0 (c22a820, c828899): deadlines, no extrapolation, the guarantee string.
- 2026-11-14 B3 dispatched before B2b: closed (B2b bb0d7b6..3b1ed68, B4 c2bf696, B3 f24ed4e landed in order).
- 2026-11-14 B2b: closed; the lag formula and the kernel's directory lock are in docs/trees.md.
- 2026-11-14 B2b rest: 1-8 closed (built, documented); 9 stays with item 4 above.
- 2026-11-14 B4: 1-5 and 7 closed; 6 closed by this fix (`one_object_one_file` from the library's objects and a
  mount's inodes); 8 (a snapshot read is one blob per file) stays: a follow-up if takes on large trees prove slow.
- 2026-11-14 B3: 7 closed by d7666ef (no descriptor per file); 9 closed by this fix (`carried_bytes` dropped from the
  figures); the rest closed (built, documented).
- 2026-11-14 speed fix 1: 6 closed by this fix (`check_targets` strict); 8 stays (a pushed commit is never rewritten).

## 2026-11-14, speed fix 3: what was found, what was built, the residuals as beads to cut

1. **Patch e2e "never mounted" (run 43610665917).** The server's access log: the daemon's inbox fetch (`info/refs`,
   upload-pack, the first request through fcgiwrap) answered 200 in 30.363 s, starting the instant its 401 was
   answered; the mount waits `agentfs.patchFetchTimeout` (30 s by default) for that fetch, so it served 0.4 s after
   the script's own 30 s wait. The server-side stall ended when the daemon was killed (its fetch thread still held
   the connection), so git-http-backend or fcgiwrap waited on the client; not reproduced locally in 1,500 fetches
   and 25 mounts under CPU load (git 2.43 locally, 2.56.1 on the runner). The script now waits 60 s and dumps the
   daemon's threads, sockets and the server's git processes with kernel stacks on a fetch timeout or a missed mount.
   Also seen, not fixed: after `patchFetchTimeout` the fetch thread keeps its connection up to libgit2's 300 s I/O
   timeout, and fcgiwrap (one worker) serializes every later request behind it.
2. **Memory per file.** About 20 KiB a file was libgit2: the one `Repository` handle `TreeGit` kept held a delta-base
   cache (up to 16 MiB) for every pack it had read, and the builder adds a pack per batch. `refresh` (after each
   batch) now drops the handle. Name versions, file-map pieces and each object's paths are vectors or one set, not
   a B-tree map per file. Release, local: 3.4 to 3.8 KiB a file (VmRSS growth, 5,000 to 20,000 files). The rest:
   the builder clones its whole state per batch (about 0.7 KiB a file, measured by taking instead of cloning), each
   path held about four times in each projection. The `memory_create` bar is 2 KiB, so CI warns until those go.
3. **Coherence 3** built as planned: `FramePos` is (record, offset) with the segment it reads; `Relocations` lives in
   `Durability`; the actor moves its positions after `drop_behind`, the builder before each batch; the save digest
   hashes (record, offset). A view's open file keeps its segment until it closes.
4. **P3-4** built: `ci/power-loss-trees.sh` holds a file open across a 70 MiB rollover, a compaction and a carry,
   mark `HC`. The workload ran locally against a view (carried 4 MiB, dropped 2 segments, history right); the
   dm-log-writes replay was NOT RUN (CI-only, `power-loss.yml` after landing).
5. **Beads to cut** (this agent may not create beads): each names its files and done condition.
   - Coherence 5: `TreeActor::{create, open, take_next}` crate-private. Files: `crates/agentfs-core/src/tree_actor/mod.rs`,
     a test-support handle over `Recording`, `crates/agentfs-core/tests/*.rs` (about 1,000 call sites),
     `crates/agentfs-conformance` (its `Library` on `Recording`), `crates/agentfs-fuse/src/tree_view.rs`. Done: the
     three are `pub(crate)`, `Recording::take_next` is the one take path, all tests green.
   - Coherence 6: mounted replay never sees a lagging builder. Files: `crates/agentfs-conformance`,
     `crates/agentfs-core/src/tree_actor/history/`. Done: a mounted replay where the builder trails the actor by
     batches (a failpoint holding the builder), invariants read from git, green.
   - Coherence 7: the builder's step duplicates tree transitions. Files: `history/builder.rs` (`BuildState::step`,
     `apply`), `tree_actor/tree.rs` (`Shape::apply`), `tree_actor/mod.rs`. Done: one transition per change that both
     projections run; conformance replays unchanged.
   - Builder `owners` growth (P3-6): `owners` shared as an `Arc`, only each batch's new entries resolved. Files:
     `history/builder.rs` (`built_owners`), `history/mod.rs`. Done: a batch's cost independent of earlier saves.
   - Checkpoint size at millions of files: a checkpoint's objects map passes the 96 MiB frame limit, and compaction
     then fails before its checkpoint every time. Files: `journal/commit_point.rs`, `journal/mod.rs`. Done: a
     checkpoint of a tree with 2,000,000 objects is written and reopened.
   - B4 8: a take on a large tree reads one blob per file. Files: `tree_actor/takes.rs`, `tree_git.rs`. Done: a take
     figure on a 100,000-file tree in `ci/trees-figures.py` within its bar.
   - Memory per file to 2 KiB: no per-batch clone of the builder's state (apply in place, or persistent maps), paths
     shared. Files: `history/builder.rs`, `tree_actor/tree.rs`. Done: `memory_create` under 2 KiB on CI.
   - Pack count: the builder adds a pack per batch and nothing repacks, so every lookup scans more packs. Files:
     `history/builder.rs`, `tree_git.rs`. Done: pack count logarithmic in batches (geometric repack), with the
     tracer's history checks green.

## 2026-11-14, M1 S4-S6 re-cut (code architect): where the old pages broke on the engine

Re-cut pages: `.claude/handoff/m1-s4-recut.md`, `m1-s5-recut.md`, `m1-s6-recut.md`; they override `m1-pages.md`'s S4-S6.
1. **C4 without `rebind` stops phase 1.** The old S4 ends every restored client at boot and keeps a fork's writer claim
   for life, so the mounted conformance replay's crash (kill, remount the same fork, write), `ci/e2e-trees.sh` and
   `ci/e2e-tree-recording-faults.sh` would get `ForkTaken`. Re-cut: S4 builds clients.md's `rebind` (with M1's pod
   context); the boot order is rebind or attach, then `reconcile(live)`. A clean unmount still spends the claim.
2. **The old S5's after-take hook cannot reach the services.** It runs on the view's worker thread; the services hold
   `Rc` (S3's `DomainEvents`). Re-cut: `ViewHost` notifies `HostEvent` through a `Send + Sync` callback into the
   composition root's channel; SIGTERM joins that channel in S6.
3. **Opening a view waited for history.** `serve_with_metrics` calls `head()`, which settles; a failed builder would
   refuse the mount (principle 5). Re-cut: `ViewHost::open` checks the actor's saved trees.
4. **`ClientStore::end`'s doc promises reserved room** that no page builds; `ClientError` and `TreeError` have no
   variant for a stopped actor. Re-cut: the doc names `Stopped`, `NotDurable`; both errors gain `Stopped` with one
   `From<StoreError>` each.

## 2026-11-14, M1 S3 (store: volume events as commits beside their trees): where the page is short

Page: `.claude/handoff/m1-s3-recut.md`. None blocks the slice; each is built as stated.
1. **The probe "gate row steps → `a_failed_builder_stops_no_event`" cannot be killed by that test.** The lag gate
   admits every call while the builder has failed (principle 5, `LagGate::admits`), so a store write that first
   calls `Recording::admit` passes it (probe run: all seven `tree_store` tests green). Killing it needs a builder
   that lags without failing and a gate limit of zero (`History::set_lag_limits` is crate-private), and then the
   probe blocks rather than fails; no test was added for it. The other four probes are killed.
2. **`Trees::create` carries no pod context**, but `ForkStore::create` takes `at`: `TreesService::create` passes an
   empty `PodContext`, so `ForkCreated` has no pod trailers. The launcher's pod context needs a contract change
   (a frozen interface: an erratum and a re-cut, not a local edit).
3. **`TreeError` has no `Stopped`.** S3 maps `StoreError::Stopped` and `Damaged` to `TreeError::Io(Other(EIO))`,
   `NotDurable(k)` to `Io(k)`, `Refused` to `UpstreamUnknown`; S5's page adds `Stopped` (`From<StoreError>`).
4. **"`pending_from` ... set by `publish`" misses takes:** a landing's take enqueues syncs too. Each queue entry's
   source is the step's tree, set in the one place every step is recorded (`TreeActor::record`), replayed after
   the built point, and read at open from a built landing's `Agentfs-Tree` (a sync marker: its queue's upstream).
5. **`fork_with(upstream)` with a fork upstream** would record a step recovery refuses (forks of forks are not
   built; `Upstreams::check_recovered`). It answers `NotFound` before recording; Trees answers `NestedNotBuilt`
   before that.
6. **A row ref's `Step(n)` for an earlier step** is never written (`record` names a chain's own step only); the
   builder answers `Disagrees` for one rather than resolving it.
7. **Files outside the owned list, each a compile need:** `tree_actor/tree.rs` (`Shape::apply` arm for
   `Change::Rows`), `tree_actor/call_path.rs` (`progress` uses the new `queued()`; the old `queued(tree)` is
   `queue_holds`), `history/feed.rs` (`BuildRef`), and the `Cargo.toml` of `agentfs`, `agentfs-conformance` and
   `agentfs-core` (dev) for `agentfs-trees`.
8. **`an_event_is_durable_before_create_returns`** checks the journal's durable mark against the last record with
   the builder failing before its own fsync: the process-wide `journal_fsync` count is shared by parallel tests.

## 2026-11-14, M1 S4 (clients and views): where the re-cut page is short

Page: `.claude/handoff/m1-s4-recut.md`. None blocks the slice; each is built as stated.
1. **A mount's own steps move every failpoint count.** `trees mount` now records an attach (or rebind) per view
   before it serves: a journal record, an fsync, a step and a builder batch each. The failpoint scripts counted from
   the client's first call: `ci/e2e-tree-recording-faults.sh` (kill at the first hit) and `ci/e2e-tree-enospc.sh`
   (`tree.before_step` and `tree.build_after_built_log` stalls, `tree.wal_before_fsync=error`) would fire in the
   mount's attach. Neither is on the page's file list. Built: a remount after a clean unmount mounts the root alone
   (one step, so exactly one builder batch), the counts move by one (`@2`, the wal records `@3`), and the script
   waits until the attach's event commit is in Git before the client's call (`tree_git_wait_root_attached`, new in
   `ci/tree-git-test-lib.sh`): otherwise the attach and the client's first record can share one batch, and a builder
   point's second hit comes later or never. The faults script's remount leaves the sandbox out (its writer is spent).
2. **`detach` and `reconcile` carry no pod context**, but `ClientStore::end` takes `at`: the service passes an empty
   one, so `AttachmentEnded` and `LastDetached` have no pod trailers (as S3 item 2 for `ForkCreated`). `Attached`
   and `Rebound` carry the client's.
3. **The Clients row is `TreeClients` itself**, its entries (`Audited<ClientRow>`, the claim) each stamped by the one
   helper (`store::stamp`); no outer `Audited` (the port's `load` answers `Vec<TreeClients>`).
4. **The CLI cannot tell a restored client's access** from `attached` (ids only). Built: `ClientsService::resumable`
   (inherent, not on the trait): the latest restored, unbound client of a launcher, tree, label and access.
5. **`detach` after a restart of a client ended in an earlier run** answers `UnknownClient`: rows are hard-deleted at
   an end, so only this run's ends are known (`ended`); within a run it is idempotent.
6. **Read-only views mount read-only** (`MountOption::RO`) besides the session refusing writes (`EROFS` either way).
7. **Files outside the owned list**, each a compile or test need: `crates/agentfs/Cargo.toml` (agentfs-clients),
   `crates/agentfs-core/Cargo.toml` (dev: agentfs-clients), `crates/agentfs/src/domain_events.rs` (`next` is used
   now: its `expect(dead_code)` went), `Cargo.lock`, and the three `ci/` files of item 1.
8. **Not built here:** `Draining` (S6's `on_volume_draining`), `on_fork_dropped` and `release_claim` (S5).

## 2026-11-14, M1 S5 (fork lifecycle): where the re-cut page is short

Page: `.claude/handoff/m1-s5-recut.md`. None blocks the slice; each is built as stated.
1. **The probe "skip `pending_from`" survives at either site alone.** The page puts the check in two places: Trees'
   `settle` filter and the actor's `drop_fork` (`Busy`). Each masks the other, so no single-site mutation fails
   `an_ended_fork_drops...`. Built: that test also calls `ForkStore::drop_fork` directly while the landing is queued
   (expects `Refused`), which kills the actor-side probe; the Trees unit test `an_ended_fork_drops_once_nothing_holds_it`
   now sets the pending landing and the child one at a time, which kills the settle-side probe.
2. **`ForkEnded` and `ForkDropped` carry no pod trailers**: `Trees::end`, `end_keyed` and `settle` carry no pod
   context, so the service passes an empty one (as S3 item 2). `ClaimReleased` carries `service_at()`, as the page says.
3. **`release_claim` of a fork with no claim writes nothing** (a fork never written, or already released): the page
   says "one step"; an empty step for nothing to release would only add a commit.
4. **An unkeyed fork now ends and drops at its clean unmount**, which `ci/power-loss-trees-check.sh` (reads
   `refs/heads/$FORK` at the final replay, after the unmount) and `ci/e2e-tree-recording-faults.sh` (step counts of the
   next mount) cannot take. Both mount a keyed fork (`--fork-key power-loss`, `--fork-key faults`), which stays open:
   the harnesses keep their meaning. The other scripts mount an unkeyed fork; `ci/e2e-trees.sh` gains the end, drop,
   exit 75, ended key and unkeyed end checks.
5. **`EntryGone` at the land of a decision on a dropped fork** is built (`TreeActor::land`) but not tested: it needs a
   decision taken unlocked and a drop interleaved before its land, which no test can order without a hook.
6. **Files outside the owned list**, each a compile or doc need: `tree_actor/tree.rs` (`Shape::apply` arm for
   `Change::Drop`), `crates/agentfs/src/shared.rs` (an `expect` reason naming S5), `docs/library/{core,filesystem}.md`,
   `docs/architecture.md`, `docs/target-architecture/future/one-engine.md` (the CLI's flag name).
7. **`--fork-read-only` excludes `--fork-writer`** (the page's `|`): a read-only fork client takes the default label.
   `trees end` restores Clients too, so its `ForkDropped` releases the claim at once.
8. **No state format bump** for `Change::Drop`: a journal of format 10 without one reads as before; nothing is released.
9. **Not built here:** the model's two `drop` traces stay excluded from the library and mounted replays (the replay's
   mapping, not this page's).

## 2026-11-14, M1 S6 (volume lifecycle): where the re-cut page is short

Page: `.claude/handoff/m1-s6-recut.md`. None blocks the slice; each is built as stated.
1. **A mount's stage moves every failpoint count again.** After a clean exit the volume is `Released`, so the next
   mount records `VolumeStaged` before its attach: two steps, in one builder batch or two, so a builder point's
   count would race. `ci/e2e-tree-recording-faults.sh` and `ci/e2e-tree-enospc.sh`'s rename runs (not on the page's
   file list) now kill a mount once its attach (or save) is built and wait for the lease (`flock actor.lock`): the
   volume stays `Staged` (a stage writes nothing) and the client attached (the next mount rebinds it), so the
   counted mount records one step, as in S4. The enospc fsync run, a first mount, counts two fsyncs: `@3`.
2. **The take gate ended the take worker.** With `AGENTFS_TAKE_GATE`, the worker returned at the gate's end, so a
   queue left at the end of a mounted replay never emptied and the stop drain never released: the mount would hang
   at `finish`. Built: after the gate ends the worker takes as it does without one (`tree_view.rs`, one line, and
   `failpoints.rs`'s doc; neither on the page's list). Nothing is compared after `finish`.
3. **The last view's end is a stop.** The page names SIGTERM and SIGINT only; `trees mount` used to exit when every
   view was unmounted, and every `ci/` script relies on it. Built: the last view's end unstages too (a convention
   decision), so unmounting every view drains and exits 0 once released.
4. **A stopped actor never releases.** With the actor stopped after a recording failure, `unstage` fails, so a loop
   waiting for `VolumeReleased` would hang (`ci/e2e-tree-enospc.sh`'s failed-fsync run expects exit 1). Built: the
   loop also ends once the actor has stopped and no view is left (`wait` reports the stop, exit 1); a stop while
   stopped closes the views first.
5. **`VolumeError` has no `Stopped`** (as S3 item 3 for `TreeError`): a store that cannot record maps to
   `NotDurable(Other(EIO))` in `agentfs-volume`'s service. `release_verdict` judges an unreadable row as `Staged`.
6. **`unstage` and `rejudge` carry no pod context**, but `VolumeStore::record` takes `at`: `VolumeService` keeps
   `stage`'s `at` (the history service's) for the volume's later events. `VolumeService::area()` is new (inherent),
   so the root reaches the `Recording` `stage` opened.
7. **The SIGTERM test mounts the root alone.** With a fork, a sync queued at the stop holds the release past the
   detach, so the probe "skip `Attached`" survived it; with no fork, no queue entry stands between the stop and the
   release, and the test kills that probe.
8. **Docs: the domain pages are trimmed only in part.** `volume.md` keeps only its unbuilt parts; `clients.md`,
   `views.md` and `system.md` say at the top what is built and where, but their built sketches are not yet removed
   (deferred to the docs step at M1's end).

## 2026-11-14, M1 end review fix round: the contract re-cut and where the spec was short

Review findings: verifier F1-F2, durability F1-F5, coherence F1-F14. Applied at the class level; each fixed finding has
a test that a mutation probe of its fix turns red.
1. **The spec's boot re-judgement missed Trees' end of an unattached fork** (durability F1). `m1-pages.md` says "at
   boot each consumer re-judges from asked facts", but an unkeyed fork ended only on a live `LastDetached`. Built:
   `TreesService::settle(clients: &impl AttachedLookup)` is level-triggered for both moves (an open unkeyed fork no
   client is attached to ends, by `agentfs`; then every ended fork the store lets go drops); `on_last_detached` is
   gone and the root settles at `LastDetached`. Clients is passed per call, not held, so Trees never asks an
   unrestored Clients slot (whose answer would be "nobody attached"). A fork must be attached in the run that creates
   it before the next settle: true of `trees mount` (create, attach, then reconcile and settle), by ordering only.
2. **The writer claim's release had no re-judgement** (durability F2): `Clients::reconcile` now also releases the claim
   of every fork Trees reports `Dropped` (store.rs's "as a boot's re-judgement needs" is now true).
3. **Contract re-cut (coherence F1, F7, F8; durability F3, F5; closes S3 item 2, S4 items 2 and 5, S5 items 1 and 2,
   S6 item 5, and applies S6 item 8).** `Trees::create`, `end`, `end_keyed` and `Clients::detach`, `reconcile` carry
   the launcher's `PodContext`; `reconcile` answers `Result` (the root logs `tree ReconcileFailed`). One home for an
   agentfs event's pod context: the store fills in `service_at()` (ports take `at: Option<&PodContext>`, `None` by
   agentfs; `drop_fork(fork)` and `release_claim(fork)` take no principal, always agentfs). `QueueFacts` is
   `queued() -> Result<_, StoreError>` alone: `pending_from` and `has_child` are gone from the port, and the actor's
   `drop_fork` is the one judge of a drop (Trees reads `Refused` as "not yet"). `VolumeStore::load` and `queued`
   answer `Stopped` before `open`; `release_verdict` judges an unreadable row or queue `Staged`. `VolumeError::Stopped`
   with `From<StoreError>`; `StoreError::Refused` maps to `ClientError::UnknownClient` and `TreeError::ForkUnknown`;
   one `store::io_kind` for every adapter. `Clients::detach`'s doc says "idempotent within a run".
4. **The Gate outcome sentence was stale** (durability F4): "each event's commit is written from it before the call
   that caused it returns". As built, the journal record is durable before return and the event commit trails with the
   step's batch. Corrected in `m1-design.md`'s Gate outcome.
5. **`ViewCaughtUp` had no owner** (coherence F12): recorded as deferred in `m1-design.md` §10 and system.md §2.3; the
   view host's `HostEvent::Landed` stands in for it until the take worker moves into agentfs-core (residual 4 below).
6. **"The actor knows no domain" was false** (coherence F10): the actor names the chains (`Chain::{Volume, Trees,
   Clients}`) but not their content; library/core.md and m1-design's module table now say so. Making chains opaque
   to the actor was not chosen: refs.rs validates their ref forms, and an opaque key would move that check to each
   store.
7. **Volume's `unstage` still carries no pod context**: it records with `stage`'s (the launcher's, the same process
   in every composition built); a contract change to `Volume::unstage` was not needed for any finding.
8. **`trees end` does not reconcile**: reconciling there would end a killed mount's restored client, which the next
   mount rebinds (C4). It settles through `end_keyed` as before.

## 2026-11-14, M1 residuals (end review findings larger than the fix round)

Each is to become a bead; none is open as one yet.
1. **A library-level volume coordinator** (coherence F3, with F6 and the suspicions "`Domains::stopped()` reads a lock
   acquisition", "`AttachedLookup` on an unrestored Clients slot answers nobody"). Files: a new module in agentfs-core
   (or a small crate) owning the boot order (stage, restore, rebind-or-attach, reconcile, settle), the event routing,
   the drain loop on one event channel and one shutdown path; `crates/agentfs/src/trees_cmd.rs` and agentfs-csi parse
   flags and call it; `crates/agentfs-core/tests/tree_store.rs` drives the module instead of its own `dispatch`,
   `domains` and `SharedTrees`; Clients owns the rebind-or-attach choice. Done when the binary holds no lifecycle
   decision, every exit after `stage` runs the drain or a release (F6: the e2e's "remount to write: exit 1" ends
   `Released`, not `Staged`), "stopped" is a fact the actor answers, and the core tests reach the real dispatcher.
2. **`stage` judges the volume name; the root never names `tree_actor`** (coherence F4, and the suspicion "`trees end`
   writes rows to a released volume without staging"). Files: `agentfs-contracts/src/volume.rs` (`stage` takes the
   expected `VolumeName`), `agentfs-volume/src/{service,ensure}.rs` (one rule with `decide_ensure`),
   `crates/agentfs/src/trees_cmd.rs` (`--volume` guard, `trees end` and `init`'s recovery through `VolumeService`).
   Done when `trees_cmd.rs` imports nothing from `agentfs_core::tree_actor` and `trees end` stages and releases.
3. **The recording-fault harness counts the mount's own steps** (coherence F11). Files: `crates/agentfs-core/src/
   failpoints.rs` (arm a failpoint scope from an event, for example once the mount reports serving, or keyed by the
   client's call), `ci/e2e-tree-recording-faults.sh`, `ci/e2e-tree-enospc.sh`, `ci/tree-git-test-lib.sh`
   (`tree_git_wait_root_attached` goes). Done when no script kills a mount on purpose to fix a count and a new
   mount-time step moves no count.
4. **The take worker and its landed event move into agentfs-core** (coherence F9, F12). Files:
   `crates/agentfs-fuse/src/tree_view.rs` (`take_worker`, `HostEvent::Landed`), the history service side of
   agentfs-core (the worker, publishing `TreeMoved` to the coordinator's channel), residual 1's coordinator. Done when
   a process with no view host (`trees end`, a CSI node) drops a fork whose landing was queued and releases a volume,
   and `ViewHost` only subscribes to invalidate pages.
5. **Smaller suspicions, each checked and not fixed here:** `Clients::rebind` checks neither `draining` nor the fork's
   phase (unreachable today: rebind runs only at boot, before a drain, and an ended key refuses before it);
   `ClientsService::end` unbinds each session before the store step, so a failed step leaves the client indexed with
   no session; a dirty mmap page written back after `unbind` gets `EROFS` and is lost without `msync`; `VolumeReleased`
   ignores a pending close after a failed `close_writer` (safe under `Keep`, wrong under a future `Delete`); rows
   are visible in the actor before `durable_all` (safe by threading, not structure); `remove_stale_checkouts` refuses
   an open on an unremovable checkout (principle 5 at boot); `ViewHost::wait` after the release is bounded only by
   Kubernetes' grace period.

## 2026-11-18, M2 S2 (the model's rules for directories, metadata and links): where the brief was short

1. **A case the rules do not decide: an upstream's decided entry under a directory the fork's undecided change
   removed or moved.** A fork runs `rmdir d` (or renames `d` away); before its upstream's queue takes that change,
   the upstream makes `d/x`, and a sync reaches the fork first. R14 says a sync keeps the fork's undecided directory
   change, so the fork has no `d`, and the upstream's `d/x` is decided, so the fork takes it: invariant D1 breaks.
   The landing itself is decided (`directory-not-empty`, the fork loses whole), so the question is only what the fork
   shows in between. Options: (a) as R11.1 does for two renames to one path, the fork shows its tree without the
   upstream's clashing entry until the upstream decides (recommended: it is the existing rule's reading); (b) the
   sync shows `d` again at once with the upstream's entry, undoing the fork's undecided change before the decision.
   Not invented: the model stops loudly there (`entries::sync`, "not modelled"), the scenarios order their steps so
   the landing is decided first, and MODEL.md's "Left out" says so. An owner question, not blocking S2.
2. **The cause word for an entry onto a path where the upstream has an entry of the other kind** (a file created
   where the upstream made a directory, or the reverse). The approved summary says such a move loses whole; the brief
   named three causes. The model records it as `directory-not-empty`, marked a choice in MODEL.md and git-model.md.
3. **The owner's answer of 2026-11-18 was already in OWNER-GUIDANCE.md**, under "One engine" with the sweep answer.
   S2 moved the rules answer to "Saves, landings and shared mappings", as the brief asks, and left the sweep answer
   where it was: one home each, no copy.
4. **`R13` already appears in git-model.md §16**, as an old requirement id inside a quote of 2026-11-02 (with `R9`
   beside it, also reused). The ledger check reads only `rule N` citations and ledger ids, so nothing clashes in a
   check; R13 to R15 were taken as the brief's numbers.

## 2026-11-18, M2 S1 (directories, modes and links in the tree views): where the brief was short

Carried on in every case; none stopped the slice.

1. **A take records its directories by path only, so a directory it adds has no mkdir number.** The brief says a
   directory's number is its mkdir record's and "a take keeps the source tree's numbers", but `TakeRecord.dirs` is
   `Vec<(String, Option<Meta>)>`, and the decision runs on built history, which holds no numbers. Reusing the source's
   mkdir number can clash: a tree that renamed a synced directory keeps its number, and a later take adds the
   source's directory of the old name. S1 numbers a directory a take adds `1 << 63 | step << 20 | index`
   (`tree::taken_dir_number`): unique, the same at the call, the replay and the builder, and apart from every record
   number. S3 may give `TakeRecord.dirs` the numbers instead.
2. **An anonymous open's metadata needs a home in the checkpoint.** `Entry::AnonymousOpen` gains `meta`, but a
   compaction that keeps a live anonymous open lists it as an `OpenRecord`, which had no metadata; a link after the
   checkpoint would replay without it. `OpenRecord` gains `meta: Option<Meta>` (anonymous opens only).
3. **A fast-forward landing takes the fork's commit whole.** "A take adds every directory the target lacks and
   removes none" cannot hold for a landing that fast-forwards: the tree's head becomes the fork's own commit, so a
   directory the fork removed would be in the actor's state and missing from the head commit and from a restart.
   S1's fast-forward takes the commit's directories exactly (`decide_landing`); a merge landing and a sync follow the
   brief's rule.
4. **The view needs a file no name reaches.** `node_at` is NotFound for an anonymous or unlinked-while-open file,
   which `getattr` must still answer (`O_TMPFILE`, an unlinked mapped file's `fstat`). S1 adds `open_node(tree,
   object)`, mirrored on `SessionGuard`.
5. **Lock keys and a sync's fast-forward test read metadata and directories.** `referee::touches` (a step's lock
   keys) and `take::shows_the_same` compared lines and name heads only, so a chmod-only change took no lock and a sync
   could fast-forward over a directory difference. Both now compare metadata and directories too: the interim rule's
   minimum, which S3 replaces with the referee's.
6. **"Each test file one const Attrs helper"**: the integration tests share `tests/support/trees.rs`, so the one
   `ATTRS` lives there (one home), and each unit-test module imports the actor's `test_support::ATTRS`.

## 2026-11-18, M2 S4 (owners, links and times in the view; `--owner` removed): where the brief was short

Carried on in every case; none stopped the slice.

1. **`crates/agentfs-contracts/src/views.rs` still names `acts_as`.** `MountOptions`'s doc comment says the presented
   owner is "the binding's grant's `acts_as`". The brief says to grep `acts_as` and update every consumer, but its
   owned paths leave this file out, so S4 did not edit it. The comment should now say each entry is presented with
   its recorded owner. S5 (or whoever owns `views.rs`) fixes the one line.
2. **The non-root cases need the e2e's work directory reachable.** `ci/e2e-trees.sh` mounts under `mktemp -d`
   (mode 0700, root's), which uid 10000 cannot traverse; S4 makes it 0755 before mounting.
3. **"touch makes no commit" needs a built history to compare.** History is built behind the calls, so
   `ci/e2e-trees.sh` checks the head after a kill and the next `trees init`'s build, against the head it already
   recorded, then mounts again to check the times survived. No wait on a timer.

## 2026-11-18, M2 S3 (forks land directories and metadata by rule): where the brief was short

Carried on in every case; none stopped the slice.

1. **The refusal test cannot live in `tests/tree_dir_landing.rs`.** The brief asks each test there to show that
   a take refused for a consulted directory key is decided again, but a refusal needs a step between a take's
   decision and its land, and the public API runs both in one `take_next`. The one refusal test is a unit test
   in `tree_actor/takes.rs` (`a_take_refused_for_a_consulted_directory_key_is_decided_again`), beside the
   existing `FilesChanged`, `Held` and `HeadMoved` ones; the integration file holds the model's 17 scenarios.
2. **The brief lists 16 scenarios; S2 wrote 17.** `r15_a_link_is_a_file_whose_bytes_are_its_target` is the
   seventeenth, and `tree_dir_landing.rs` has it too.
3. **The library's collision record is shorter than the model's.** The model records a rule 13 or 14 loss with
   the change's and the upstream's values (`lost`, `won`); the brief fixes the library's JSON as
   `{"cause":..,"path":..}`, so the library names only the cause and the entry. The model's metadata layout
   (`meta.<identity>`, `dir.<path>`) is its own choice; the library keeps the `lines/` and `dirs/` layout S1 built.
4. **A sync past an undecided directory removal goes beyond the model.** When the fork's undecided `rmdir` or
   directory rename removed a directory under which the upstream has an entry the fork never had, the model
   panics ("not modelled", MODEL.md "Left out"). The library hides that entry until the upstream decides, as
   rule 11 hides a clashing name, so D1 holds; no scenario covers it.

## 2026-11-18, M2 S5 (conformance replays directories, metadata and links; M2 docs close): where the brief was short

Carried on in every case; none stopped the slice.

1. **A mount cannot make the model's metadata change in one call.** The brief has the mounted replay make
   `chmod(2)` plus `chown(2)` for one `set-meta`. The model's metadata is one value, but Linux sets a mode and an
   owner in two system calls, and each is its own step and change: a fork's chmod then chown is two queue entries,
   so the trace's one take lands half of it (`r13_a_chmod_lands_over_the_value_it_saw` showed `0755:0:0` against
   the trace's `0755:1000:1000`), and two trees writing one new value disagree on the half-way value
   (`r13_the_same_new_value_agrees` recorded a `metadata` collision the model has not). Likewise `mkdir(2)` drops
   the set-group-ID bit (`r14_an_empty_directory_is_kept`'s `2775`) and a directory or link owned by another user
   needs an `lchown(2)`. `mountable` now leaves out a trace with such a call; 5 of the 17 rules' traces replay on
   mounts, all 17 on the library. This is real behaviour, not an engine bug: a program that runs chmod and chown
   makes two changes. Whether the model should say so (a `set-meta` of mode or owner, not both) is the owner's call.
2. **Directories are not paths.** The brief says `paths()`/`read()` report directories. `paths()` and `read()`
   feed the byte invariants and the convergence check, which compare files; `TreeCalls` gains `entries()` (every
   entry with its metadata) instead, one question per method.
3. **The traces are not regressions.** "Record the new scenarios' traces into the conformance regressions, as
   VERIFYING.md describes": `tests/regressions/` holds engine-found regressions, each with a `.patch` that removes
   its fix. The 17 traces are the model's own, kept in `tests/rules/` with `tests/rules.rs`, which replays each on
   the model and the library and requires both recordings to equal the trace.
4. **A whole loss can name an entry as its winner.** The library writes a rule 13 or 14 loss's other versions as
   `{"lost":{"Name":..},"winner":{"Entry":{"path":"e/"}},"kind":"Whole"}`; `docs/library/core.md` says only that
   they lose `Whole`. The conformance reader takes `Entry` as a version.
5. **The digest check needs the pinned git on the system path.** `ci/model-digest.sh` refuses a `git` other than
   `ci/git-release` in `/usr/bin` too, and this container has no Docker. It ran in a private mount namespace with
   git 2.47.3 bound over `/usr/bin/git`: 199 runs match; its one complaint, scenarios over 1.5 times their 5 s
   budget, is the same (19 s) on the base commit.

## 2026-11-18, M2 fix round: where the findings were short, and M2 residuals

1. **"Make the library stop loudly as the model does" cannot fail every sync step.** The library syncs the upstream's
   commits one at a time (P2), and `undecided` is judged at each commit, so an intermediate commit (the upstream's
   `q/x` before its landing decided the fork's move of `q`) meets the unruled case even when the head has decided it:
   `r14_moving_a_directory_the_upstream_filled_loses_whole` failed so on the library. The model judges only the state
   a sync shows. So each step records the unruled path (`referee::Synced::unruled`) and only the step a sync would
   show fails (`Error::Unruled`, logged, retried at the next event). The owner question of M2 S2 erratum 1 is still
   open and must be put to him by the main session, labelled "absolute requirement", with its background.
2. **The set-meta granularity question (M2 S5 erratum 1) is still the owner's.** docs/library/core.md now states the
   two-change behaviour of `chmod` plus `chown`; whether the model should make set-meta mode-or-owner is unasked.
3. **No M2 new-terms table exists in the repo** (finding B-F13). `Attrs` is now defined against `Meta` in
   docs/library/core.md and `FileOwner`'s doc no longer points up a layer.
4. **CI ran the root-only checks as the runner user.** `trees-posix.py` (view, touch, touched) and the mounted replay
   now refuse a non-root euid at start, and CI runs them under `sudo`; the views' server still runs unprivileged in
   `ci/e2e-trees.sh`. A non-root server could not be tried locally: this container's `/dev/fuse` is root's alone.

M2 residuals (each to become a bead):

- **Metadata for a file no name reaches** (B-F4). Files: `tree_actor/recording/mod.rs` (`set_meta`, `set_times`,
  `anonymous_open` taking the parent directory), `journal` (a step keyed by object), `agentfs-fuse/src/tree_view.rs`
  (`set_attrs`, `tmpfile`). Done: `O_TMPFILE` + `fchmod` + `linkat` keeps the mode, an `O_TMPFILE` in a set-group-ID
  directory takes its group, a test of each; the "Limits" bullet in docs/trees.md removed.
- **Blobs known after a restart** (B-F6). Files: `tree_actor/mod.rs` (the `read_trees` map at open),
  `history/mod.rs`, `history/format.rs` (one walk of `files/`). Done: each `FileBytes` built from a checkout carries
  its `files/` blob, so a directory rename after a restart feeds no bytes to fast-import; a test that shows it.
- **An anonymous open's times across a replay** (B-F8). Files: `journal/fold.rs` (`add_anonymous(.., 0)`),
  `tree.rs` (`restore_locals`). Done: a live `O_TMPFILE` across a compaction and reopen keeps its creation time.
- **Power loss over the M2 step kinds** (B-F11). Files: `ci/power-loss-trees.sh`. Done: mkdir, chmod, touch, an
  FSYNCDIR, a cut, and the modes, owners and times checked after replay (runs on main after landing).
- **A stable readdir cookie** (B-F12). Files: `agentfs-fuse/src/tree_view.rs` (`readdir`), `Shape::children`.
  Done: an unlink between two readdir batches skips no untouched entry; a test of it.

## 2026-11-19, M3 S1 (operator rules and the untracked area): where the brief was short

1. **The engine guard answers `Error::Name` for a `.git` name, not `Error::Untracked`.** `recording::name` (RepoPath)
   refuses every git spelling of `.git` first, and `crates/agentfs-core/tests/tree_actor.rs`
   (`single_actor_and_invalid_names_fail_before_recording`, not this slice's file) asserts `Error::Name` for `.git`
   and `d/.git`. The guard runs after `name()`, so the engine answers `EINVAL` there and `EXDEV` for every other
   untracked placement; the view never calls the engine for a `.git` name (it routes it to the area itself).
2. **The rules error is lost on the CLI's path.** `trees mount` opens the volume through `VolumeService::stage`, whose
   `StateArea` port (`state_area::volume_error`) maps every other actor error to `VolumeError::NotDurable(Other(errno))`,
   so the user sees "the volume's state could not be made durable: Other(22)". The actor now logs
   `tree RulesRefused: info/attributes line N: ...` before it fails, which names the line (the badrules e2e checks
   that). The proper fix is a `VolumeError` variant carrying the reason (agentfs-contracts `volume.rs` and
   `state_area.rs`, neither owned by S1).
3. **A fork of a fork is not reachable.** `TreeActor::fork_with` refuses an upstream other than the root, so the owner's
   requirement (4) ("a fork of a fork copies its own upstream's area") holds by construction (`copy_area(upstream,
   fork)`) but has no test.
4. **"A kill mid-copy" is tested by the state a kill leaves, not by a kill.** A failpoint inside the copy needs a name in
   `failpoints::POINTS` (`crates/agentfs-core/src/failpoints.rs`, not owned). `tree_rules.rs` plants a cut copy under
   `.staging/` and a published copy no fork owns, and checks the open removes both.
5. **`agentfs-rules` also depends on `gix-parallel`** (feature `parallel`): without it `gix-attributes`' names are `Rc`
   and a `RuleSet` is not `Send + Sync`, which the shared `Arc<RuleSet>` needs. Feature unification hid this in the
   workspace build but not in `cargo test -p agentfs-rules`.
6. **The brief's `fork FORKMOUNT` e2e mode ("shows none of the root's untracked files") contradicts the owner's
   2026-11-19 answer.** It is split: `early-fork` checks a fork made before the root's untracked files has none of
   them; `fork` checks a fork made after them starts with a copy, and that later writes stay on their side.
7. **`docs/target-architecture/future/one-engine.md` is not in S1's owned paths**, so its M3 row is left to S3.
8. **A dropped fork's area goes with its checkout, so only a fork the builder had checked out loses its area at once.**
   One dropped before it was ever built keeps its area until the next open's sweep.

## 2026-11-19, M3 S2 (whole-file replace, untrack at open): where the brief was short

1. **A new failpoint needs its name in `failpoints::POINTS`** (`crates/agentfs-core/src/failpoints.rs`, not in S2's
   owned paths): `every_hit_is_listed` and the spec parser refuse an unlisted name. S2 adds
   `tree.replace_before_step` and `tree.untrack_after_place` there, two lines and nothing else.
2. **A replace cannot tell "unchanged" before its chunk records are written**: `src` is read once. Its chunk records
   go first (after any due compaction, and none between them and the step); a replace that changes neither bytes
   nor metadata then answers `Saved::Unchanged` and leaves its chunk records with no step, which the next take or
   replace step discards as it discards a cut-short one. The cost is those records' journal bytes until compaction.
3. **A replace over a file with a live write-open runs the page cache's save hook** (`pages_saved`, `pages.rs` not
   owned): the file's image is dropped and the page writes made before the replace count as superseded, as a mapped
   write to a file renamed over is lost on Linux; the opens' own calls still save over it at their close. The
   arrival-order size the view reports for that open file (`inode_size`) is not reset until its last open closes.

## 2026-11-19, M3 S3 (moves across the halves, close M3): where the brief was short

1. **"One of ten lines changed keeps nine line identities" is not what the engine does.** A changed line keeps its
   identity and gets a new version (`lines/`' runs carry both). `ci/trees-untracked.py save` checks that all ten
   identities are kept and that nine (identity, version) pairs are unchanged.
2. **The allowlist save cannot live in `view` and `history`.** Exclude `/*` with `!/config.yaml` would leave out every
   name the main volume's checks record. It is a mode of its own, `save BIN DIR`, on its own volume, as `untrack` and
   `badrules` are; `view` and `history` carry the other new checks (`/logs/` added to the main volume's exclude).
3. **Mutation probe 2 is caught by the engine's rule guard, not by the history check.** An import by `first_link` of
   the temporary name answers `EXDEV` (`Error::Untracked`, the engine refusing a name the rules place untracked), so
   `view`'s rename of `draft.log` fails before any history is read. The history check's power against that mutation
   is unproven; the guard makes it unreachable. Probe 1 likewise fails at `view`'s `draft.log` rename, before the
   `save` mode's `os.replace`.
4. **An import onto a tracked link is not specified.** `replace_file` answers `Error::Invalid` there; the view answers
   `EXDEV` (`mv` then copies), and `EISDIR` onto a tracked directory, as rename(2) does.
5. **`docs/target-architecture/future/module-cut.md` still says step 7 runs "beside its M8"** (not an owned path); after
   the 2026-11-19 order it runs after the baseline, which now follows M8.

## 2026-11-19, M3 fix round: what was applied, and M3 residuals

Applied (this entry closes them): M3 S1 item 2 (a rules refusal is `VolumeError::RulesRefused`, naming its line; the
`tree RulesRefused` stderr line is gone), S1 item 4 and S2's failpoint reason (`tree.fork_copy_file` and
`tree.fork_after_copy`, with kill and error tests in `tree_rules.rs`), S1 item 8 (`drop_fork` removes the area when
the drop is recorded), S3 item 5 (`module-cut.md` says step 7 follows the baseline, which follows M8).

Where the findings were wrong:

1. **"Unreachable in practice" was wrong for the inode ranges.** A directory a take adds has bit 63 set in its
   number (`tree::taken_dir_number`), so its inode fell in the untracked range (`1 << 63 | n`) and the view would have
   served it from the area. Untracked inodes are now `[1 << 62, 1 << 63)` and `tracked_inode` answers `EOVERFLOW` for
   a number that would meet them.
2. **"Name calls never under the actor's lock" was the docs' error, not the code's.** The lock makes a name's half
   one decision against the tracked half (an `O_EXCL` create, a rename across the halves, a merged listing). The
   docs now say which calls take it; the code is unchanged.

M3 residuals (each to become a bead; none blocks M7):

1. **The owner's test "a SQLite database copied while being written opens and passes integrity_check" is not built.**
   No launcher makes a fork while its upstream's view serves (`trees mount` forks before mounting), so a copy under
   live writes has no real flow. Files: M8's fork claim path in `crates/agentfs-csi`, a test-only way to make a fork
   on a live view host, `ci/trees-untracked.py`. Done when a fork made while a WAL-mode writer commits through the
   root view copies a database that passes `integrity_check` with at least the rows committed before the fork.
   docs/trees.md "How it is verified" lists it as untested.
2. **A fork's copy holds the actor's lock for the whole copy** (`fork_with` runs `copy_area` under `&mut self`): every
   call on the volume, tracked recording included, waits while an area is copied, one fsync per file where reflinks
   are not available. The owner asked to hold the upstream's untracked writes, not the volume. Owner question for
   M8 (general guidance): take the copy outside the actor's lock, holding only the upstream's area, then record the
   step; or accept the stall as a cost of fork claims on a live volume. Files: `tree_actor/mod.rs` (`fork_with`),
   `tree_actor/untracked.rs`. Done when the answer is recorded and built, with a test that a tracked write
   completes during a long copy (or the cost in docs/trees.md if accepted).
3. **Readdir cookies of the untracked half are indexes into a listing re-read per batch** (`tree_view.rs`,
   `listing`): an entry made or removed between batches skips or repeats others. Same class as M2 residual B-F12;
   fix both halves with one stable cookie (by name or hash) under that bead, covering the area half in its test.
   The area listing is also re-read for every batch under the actor's lock (`ls -f` of 20,000 untracked entries
   took 1.6 s against 0.012 s directly); page it in the same change.
4. **A fork of a fork copies its own upstream's area (owner requirement 4) has no test**: forks of forks are not
   built (`fork_with` refuses them; docs/trees.md says so). Done when forks of forks are built, with that test.
5. **Readers still reach raw descriptors.** Every change to an area goes through `AreaWrite`, and the view's untracked
   writes take it by signature, but an untracked file's open descriptor is a plain `OwnedFd`: a future write path
   that skips `untracked::write` compiles. Done when the view's untracked opens are a core type whose write,
   truncate and allocate methods take the hold (`tree_view.rs` `AreaOpen`, `tree_actor/untracked.rs`).
6. **Untracked entry numbers are not bounded at allocation** (`tree_view/untracked.rs`, `Inodes::next`): past
   `1 << 62` lookups an entry's inode would leave its range. Unreachable (4.6e18 lookups); the bound would make it
   impossible by construction.

## 2026-11-19, M7 S1 (library for launchers): where the brief was short

1. **The stacking test cannot make its stale mount by aborting the connection in-process.** The brief's root-only
   test aborts the first view's FUSE connection through `/sys/fs/fuse/connections/<dev>/abort` and opens a second
   `ViewHost` on the same state. Neither holds: fuser's `Mount` drop (run when the view's session joins) reads the
   aborted device's `POLLERR` as still mounted and unmounts the mount point, so no stale mount is left; and the first
   host's actor keeps the state area's lock in that process, so a second `Recording::open` is `Busy`. Done instead:
   `tree_view::tests::leave_stale_view` runs the view in a worker process (the test binary itself, by env var),
   waits for its "ready" line (an event, not a timer) and kills it, which is the crash being recovered. The
   assertions are the brief's.
2. **`operator_settings`' module doc names docs/trees.md, "Operator settings"**, as the frozen page says, but that
   section does not exist yet: docs/trees.md is S2's. S1 documents the API in docs/library/core.md ("Operator
   settings"); S2 must add the docs/trees.md section the module doc names.
3. **`write_includes` must run after `ensure`.** `tree_actor::inspect` calls a directory holding anything but
   `lock` or the create marker, without `format`, `Other`: a `<state>/config` written before the create makes
   `ensure` answer `OtherFormat`. S2's `init --include` writes the includes after `ensure` (the state area exists
   then).

## 2026-11-19, M7 S2 (the binary moves): where the brief was short

1. **A found `init` is never byte-unchanged.** Its recovery open appends to the journal (`journal/<seg>.seg`
   differs after any found `init`, with or without `--owner`), so "`--owner` on a found volume changes no file"
   cannot hold. `cli_init.rs` checks what must not change instead: the `root` ref and `root:dirs/root`.
2. **The includes are written after the recovery open, under the volume's lock**, not between `ensure` and the
   open: a busy volume then exits 75 with nothing written. Probe 5 (write before `ensure`) still fails the
   another-name test.
3. **Files outside the owned list that the slice breaks.** `docs/development.md` linked the deleted
   `tests/mount.rs` and `tests/crash_mount.rs`, failing `docs_reference` locally: S2 rewrote those two bullets
   (and one table row) minimally. Not edited, for S3/S4/S5: `ci/readme-quickstart.sh` (source mode still checks
   `final push: origin: pushed` and `/data/remote.git`, so ci.yml's fuse job fails on the new README quick start);
   ci.yml's fuse job still runs a passthrough matrix of three identical legs and sets
   `AGENTFS_TEST_PASSTHROUGH`/`AGENTFS_TEST_UNPRIV_USER`, which launcher_mount ignores;
   `docs/operator-guide.md` and `docs/supported-configurations.md` still name `mount --history-service` and the
   recording mount; `agentfs-contracts`' `VolumeError::NotCreated` message still says `agentfs trees init`
   (the launcher answers with its own message first); the sandbox overlay is still in `agentfs-fuse` (no
   binary serves it).
4. **The gate is red on the last measured figures.** docs/performance.md's last release run made 7,529 (ext4)
   and 7,743 (XFS) creates of the bar's 50,000, so with `create` gated the image workflow's trees-tracer jobs fail
   until the builder's cost per save drops. That is the gate doing its job; it is the milestone's to clear.
5. **`--seal-mountpoint` seals every view's bare mount point** (the fork's too), not the root's alone: the frozen
   page's "(root only, as today)" read as "needs root"; a fork bound by subPath has the same gap.

## 2026-11-20, M7 S4 (retire the recording launcher's CI): where the brief was short

- **e2e-csi-move step (h) needed step (g).** The brief said to delete step (g) (a CLI recording volume pushing,
  diverged) and renumber. Step (h) (a start class on a `refs/agentfs/<V1>/diverged/*` ref refused as "not a branch")
  read the ref (g)'s divergence saved. Done: (g) is gone; (h) becomes (g) and writes that ref on the git server with
  `git update-ref` (as root, through `sgit`), so the refusal is still checked; (i) becomes (h). Untested locally (kind).
- **No alert list in docs/telemetry.md.** The alert table is in docs/deployment.md (S3's). telemetry.md's "Alerts"
  section gained a paragraph on `AgentfsNotBuilt`; deployment.md's table still lacks the row (S3 or S5).
- **AgentfsTelemetryExportFailing cannot be caused by a tree mount.** It exports metrics only (no spans, no logs), so
  the only export that could fail would carry the count. e2e-alerts lists `agentfs_telemetry_export_failures_total`
  in NOT_CAUSED with that reason.
- **References outside S4's paths to deleted jobs or files:** AGENTS.md's workflow table (`power-loss.yml`,
  `bench.yml`, and ci.yml's covered list) for S5; `ci/bench-budget.json`'s comment names `ci/bench-report.py` (kept
  file, not owned); docs/runbooks/stuck-pushes.md names the `e2e-shared-remotes` job; crates/agentfs-core/src/
  ssh_transport/tests.rs names the `e2e-ssh` job; ci/git-http-server.sh documents `GIT_SERVER_LFS` for
  ci/lfs-server.sh and `lfs_faults`, so lfs-server.sh and the LFS helpers in ci/lib.sh were kept (now used by no
  script). The runbooks' `<!-- ci: ... -->` markers stay, run by no script now.

## 2026-11-20, M7 S3 (sidecar, Docker and Hermes on the tree engine): where the brief was wrong or short

- **The migration Job's `cp -a` can record a nested repository's files.** Rules decide when a name is made, and
  "inside a nested repository" needs the `.git` entry to exist first. GNU `cp -a` copies a directory's entries in
  inode order: a repository whose files predate its `.git` (files first, then `git init`) has them recorded. Seen
  locally with the real binary: `skills/cloned/SKILL.md` reached history. Done: deployment.md §10's block (and the
  upgrading runbook's move) copies each `.git` first (`find . -name .git -prune -print0 | xargs -0 -r cp -a
  --parents -t <mount>`), then `cp -a .`; with it the file stays untracked. The rest of the block is as the brief
  wrote it.
- **F3 is half right.** Locally, a mount stopped with SIGTERM while a bind of its view (a `subPath` stand-in) holds
  it drains, unmounts the view (the mount point turns bare), answers the bind's writes with `EROFS`, and stays alive
  until the bind goes. So the restart after the kill mounts the bare mount point; it does not stack (nothing is
  stale at the mount point). The consumer's bind then fails `ENOTCONN` and its liveness probe restarts it, as with
  the recording engine. A sidecar-only SIGTERM never ends on its own (no kubelet grace applies): it ends at a kill.
  deployment.md §7 and the kind and Hermes kind tests say and check that.
- **Possible stuck Terminating (not tested).** A view stacked over a stale layer detaches it only when the view
  ends; with a `subPath` consumer a pod stop ends in the kubelet's kill, so the dead layer stays under the emptyDir
  for the kubelet's teardown, which may not remove it. The old kind step "deleting the recovered pod leaves nothing
  mounted" is gone (it was the recording engine's exit path); deployment.md §7 lists this as a limit. For M8/M9.
- **Files outside the owned paths, needed by the slice:** `ci/helm-equivalence.sh` and
  `ci/helm/sidecar-sandbox-overlay-values.yaml` (the deleted overlay's comparison), `ci/readme-quickstart.sh`'s
  docker checks (the README Docker block runs on the tree engine now; its source mode is untouched), and
  `crates/agentfs/Cargo.toml` + `Cargo.lock` (`agentfs-rules` as a dev-dependency for deploy_examples.rs).

## 2026-11-20, M7 S5 (docs close): where the brief was short or the code disagrees

- **Links to the deleted overlay runbook outside S5's paths.** `docs/library/filesystem.md` and `docs/telemetry.md`
  linked `runbooks/sandbox-overlay.md`; the link check fails without repointing them. Done: both now point to
  guarantees.md's "The sandbox overlay", committed with S5 although not in its owned paths.
- **`trees mount` still named outside S5's paths:** `docs/architecture.md` (Technology choices, "Signals" row) and
  `docs/target-architecture/future/tree-view-telemetry.md`. Not changed. In code, `crates/agentfs-contracts/src/
  volume.rs`'s `NoVolume` error text still says ``agentfs trees init --volume NAME`` (the launcher prints its own
  message, so it is reached only through the library).
- **Dead recording code after the single-process mount went:** `agentfs_core::patch::attached`'s `UncleanStop`,
  `StaleMount`, `mark` and `meta/attached.json` are produced by no binary path now (only `serve-history`'s
  `FrontsRunning`/`WorkloadAttached` remain). The patch-queue runbook's `unclean_stop` and `stale_mount` rows and
  their Docker and sidecar steps were removed; the code is for M9 or a cleanup bead.
- **guarantees.md cited a deleted test.** `overlay_refusals` (an integration test in the binary's mount tests) went
  with S2; the "directory not empty" leak is now marked not tested.
- **AGENTS.md's bench/power-loss rule (decided 2026-10-15) had no workflows left** after S4. Replaced by the one
  rule that still applies: a release figure other than `create` missing its bar is a warning to follow up.

## 2026-11-20, M7 fix round: where the review was wrong, and M7 residuals

- **F4's hang does not happen.** With `command.terminate()` removed from the Stop arm, `mount -- sh -c 'cd ROOT;
  trap ... TERM; sleep & wait'` still exits 7 with nothing mounted (run locally, as root): the loop ends at the
  volume's release (`domains.released`) without waiting for the busy view to end, and the terminate after the loop
  then stops the command. The Stop-arm terminate only sends the signal earlier, at the drain's start, which no
  outcome a test can observe without a race distinguishes. `launcher_mount.rs` (f) now checks the documented
  behaviour (SIGTERM sent on, exit 7, nothing left mounted); it does not kill that mutant.
- **F5's probe scenario is refused earlier now.** `--fork-mount <state>/inside` is refused by `prepare` (F11) before
  anything is staged, so it no longer reaches a failed view open. The test of "a refused view means the command never
  runs" (`launcher_mount.rs` (g)) uses a view only the view host refuses: the fork's mount point holding
  `--metrics-output`. It kills the `failed.is_none() || true` mutant (the marker is made).
- **F9 corrected.** `csi_service.rs` and `crash_daemon.rs` drive the history service through `record
  --history-service`; no Rust test runs the FUSE `agentfs front` now. The front's FUSE path is covered only by CI's
  CSI e2e jobs (image e2e-csi, f8, hermes csi). docs/guarantees.md says the recording FUSE front killed mid-write is
  not tested by the crash oracle.
- **S2 item 4 is superseded.** The trees-tracer gate passes in CI: create reached 50,000 (image run 44649022393).
- **C2 decided:** the docs workflow's README Docker job builds the image from the commit and runs the README block
  against it (`AGENTFS_IMAGE=agentfs:ci`); docs/development.md and AGENTS.md's workflow table say so. The published
  image is gated by image.yml's e2e jobs before publish.
- **C5 for a keyed fork:** `mount`'s prepare cannot know a keyed fork's tree before staging, so it accepts a stale
  `agentfs:<volume>:...` there and the view host makes the exact check at open (`stale::stackable`). A root-only test
  of a fork's stale mount at the root's path through the view host is not written (the stale worker mounts the root
  only); the rule is tested by `stale::tests::only_the_exact_source_is_stackable` and, through the binary, by
  `launcher_mount.rs` (h) (an unkeyed fork never stacks over a crashed one).

M7 residuals (each to become a bead; none blocks M8):

1. **Delete the recording mount's dead stacking and the sandbox overlay.** `agentfs-fuse`'s `overlay` module,
   `MountConfig::stack_over`, `OverlayConfig::stack_over`, `preflight_stacked` and the overlay half of `mount.rs`
   (`tmp_wipe_error` and the rest) have no caller since S2 (`front.rs` passes `None`); `agentfs_core::patch::attached`'s
   `UncleanStop`, `StaleMount`, `mark` and `meta/attached.json` are produced by no binary path. Files:
   `crates/agentfs-fuse/src/{overlay.rs,overlay/,mount.rs}`, `crates/agentfs-core/src/patch/attached.rs`, their docs
   in docs/library/filesystem.md and docs/architecture.md. Done when no symbol is unreachable from a binary or test of
   a live path and `cargo test --workspace` passes. Due at M9 at the latest.
2. **The view's memory per file (F15).** memory_create missed its bar on ext4 and XFS (4.33 and 4.59 KiB per file
   against 2 KiB; about 250 MiB RSS for 50k files; a 100k-file tree nears the sidecar's 512Mi limit). Files:
   `crates/agentfs-fuse/src/tree_view.rs` handles and the actor's per-file state in `crates/agentfs-core/src/tree_actor/`.
   Done when `ci/e2e-trees-tracer.sh --figures` meets `ci/trees-budget.json`'s memory bar on both filesystems.
3. **A Rust test of `agentfs front` on FUSE (F9).** One root-gated test in `crates/agentfs/tests/` mounting `front`
   against `serve-history` through `volume::init_with`, so the front's FUSE path has a local test until M9 deletes
   it. Done when `ci/fuse-tests.sh` runs it.

## 2026-11-20, M8 S1 (`agentfs host`): where the brief was wrong or short

1. **P3's mechanism is wrong.** The brief says replying `closed` before `HostEvent::ViewEnded` fails test (i)
   "because A is still a mount at the reply". It is not: `ViewHost::close` detaches the mount lazily
   (`umount2(MNT_DETACH)`), so the mount point leaves the namespace before `close` returns, whatever the reply
   waits for. What `closed` after `ViewEnded` adds is that the view's files are released and its client detached.
   Test (i) now holds a file open across the close and then opens another view at the same place, and (l) opens
   the fork with a new label; under P3 these fail in about two runs of three, not every run. A test that kills P3
   every time needs an event that says the host has handled the close while a file is still open (the mount table
   changing, read through `poll` on `/proc/self/mountinfo`); none is in reach without unsafe code or a new
   dependency.
2. **`crates/agentfs/Cargo.toml` was not an owned path.** The brief asks for rustix's socket type of stdin, which
   needs rustix's `net` feature in the binary's crate. S1 turned it on (one line, no new crate, Cargo.lock
   unchanged).
3. **Fork numbers are journal event numbers, not 1 and 2.** Test (c) says `Forked{fork 1}` and "the next automatic
   create gets fork 2". A fork's number is the journal event that made it. Test (c) checks the refused approval
   wrote nothing by running the same steps, without it, on a second volume and comparing the numbers.
4. **Test (j)'s landing wait.** launcher_mount.rs has no landing wait to copy. Test (j) uses the drain: Volume
   releases only with every queue empty, so the host's exit 0 at EOF is the landing's event. It then checks the
   root's history and reads the file through a new host's view of the root.
5. **Refusal kinds the frozen page has no row for.** `ClientError::NotDurable`, `TreeError::Io` and
   `TreeError::NotPermitted` reply `stopped`. `ClientError::NotPermitted`, `UnknownClient` and `NotItsLauncher`
   reply `mount_failed`. `ClientError::TreeUnknown` replies `key_ended`, and `TreeError::UpstreamUnknown` and
   `ForkUnknown` reply `key_unknown`. Each refusal's message carries the real error.
6. **Residual risks for S2.** An open whose mount fails detaches the client it bound, as `mount` does. For a fork's
   writer that ends the fork's one writer, so a NodePublish retried after a transient mount failure gets
   `fork_taken`. The include-under-a-view check of the operator settings cannot run at boot because `host` knows no
   view then; it is not checked at open either.

## 2026-11-20, M8 S3b (f8 jobs and the forks job): where the brief was short

1. **`ci/k8s-csi/move/values.yaml` is stale and owned by no slice.** ci/e2e-csi-move.sh installed the chart with
   it. It passes `--release-scan-interval=5s`, a flag agentfs-csi no longer has (S2 removed the release scan, so the
   node plugin would exit at its first argument parse), and its `agentfs-move` class sets `onDelete: delete`, which
   CreateVolume now refuses. S3b's owned paths do not include it, so the move script now writes its own values to a
   temporary file and the old file is unused. docs/helm.md still links it: the docs slice (S5) or the milestone's
   end should delete the file and that link together.

## 2026-11-20, M8 S2 (`agentfs-csi` on tree volumes, root claims): where the brief was wrong or short

1. **CreateVolume's order.** The brief reads `csi.json` before inspecting the state for a recording-format volume. A
   recording driver's `csi.json` does not parse as a `ClaimRecord`, so that order answers `INTERNAL` (a parse error),
   never the recording-format `ALREADY_EXISTS`. CreateVolume inspects the state first, as DeleteVolume and NodePublish
   do.
2. **csi-sanity's `[host ` line.** A host writes nothing to its stderr when no setting is not built, so a profile with
   no keys gives no `[host ` line. ci/csi-sanity/profiles/sanity.config sets `pushInterval`; the script checks each
   host's `not built:` line.
3. **Audit fields.** The frozen `ClaimRecord` and `PublishedRecord` carry no created or changed time or actor, which
   `rules:global` asks of every persisted record. Built as frozen; a later re-cut decides.
4. **Additions to the frozen page, all additive.** `Layout::published_dir()`; `mounter::detach_agentfs` (the frozen
   `Mounter` has only `detach` and `state`, and a host that crashed twice leaves two stale layers);
   `launcher::recent()`; `controller::FORKS_NOT_BUILT`. `validate_handle` keeps its `LayoutError` ("unchanged").
5. **Texts the brief kept that are no longer true.** `volume::MULTI_NODE` says pods on other nodes read the history
   "from the remote it pushes to"; push is not built. docs/configuration.md's `fork`, `link` and `remotes` rows still
   describe the recording driver's forks (S4 owns them).
6. **Seen, not S2's.** A host whose state area is removed under it (a failing root test's `TempDir` drop, recursing
   through the live mount) stays in its drain after EOF with no builder thread, and never exits. The test's orphan was
   killed by hand. Files: `crates/agentfs/src/launcher/serving.rs`, the tree actor's builder.

## 2026-11-20, M8 S4 (fork claims): where the brief was wrong or short

1. **ci/fuse-tests.sh pins one csi_trees root test.** It requires `test result: ok. 1 passed`; a second `#[ignore]`
   root test (the brief's fork scenario) fails that line. ci/ is not S4's path, so both root scenarios now run under one
   ignored entry point, `views_at_the_kubelet_target`. A later change to the script could match "0 failed" instead.
2. **P4's mechanism is shared.** With `in_use` counting only the claim's own records, the source's DeleteVolume is still
   refused, by `Supervisor::release`'s own guard (a host runs), with a shorter message. The root test now pins the full
   in-use text naming the fork's target, which P4 fails.
3. **P3 first failed on a host count, not on the store.** The test now checks first that the ended key is not made
   again (CreateVolume of the same claim answers `ALREADY_EXISTS`), which P3 fails because the fork stays open.
4. **"The next number".** Fork numbers are journal event numbers (S1's errata 3), so the approval test checks that the
   refused claim added no ref under `refs/agentfs/trees/` and that the next automatic claim adds one.
5. **`end_fork` also takes `key_ended` as Ok**, beside `ended` and `key_unknown`: a retried DeleteVolume of a fork
   already ended must succeed whatever the host answers.
6. **Exits were counted, not waited for.** csi_trees counted `exit host` events right after a call returned; the tap
   logs them from its own task, so the count could run first. Each is now waited for (`Driver::until`) before it is
   counted, S2's tests included.
7. **S2's frozen addition `controller::FORKS_NOT_BUILT` is gone**, replaced by `FORK_NEEDS_SOURCE`; nothing else
   used it.

## 2026-11-20, M8 S5 (docs): where the brief was wrong or short

1. **Stale CSI text in pages the code slices own.** docs/configuration.md still says every recording command runs
   "the recording engine, which the CSI driver runs" (the `agentfs` commands section), that the CSI driver sets
   `--push-grace 5s` against a 30 s unstage grace, that the CSI node plugin passes `serve-history
   --workload-attached`, and that only the CSI node plugin starts `front`. docs/architecture.md's crate table says
   `agentfs-csi` "makes the node's worktrees and supervises `agentfs serve-history`, `agentfs mount` and the release
   scan's `agentfs flush` and `agentfs push` children". S5 may not edit either page; the milestone's end should.
2. **`ci/k8s-csi/move/values.yaml` (S3b's item 1) is still in the tree.** S5 removed docs/helm.md's link to it; the
   file is under ci/, not an S5 path, so deleting it is left to the milestone's end.
3. **Pages the brief did not list also said the CSI driver runs the recording engine**: docs/operator-guide.md,
   docs/development.md, docs/README.md, docs/runbooks/recording-failures.md, worktree-heuristic.md and
   reconcile-failed.md. S5 corrected them (owned under docs/**). The scoped runbooks keep their "Under CSI" command
   blocks; one sentence each says those steps apply to no volume today, as the brief asked (no rewrite).
4. **`deploy/csi` sets `OTEL_SDK_DISABLED=true`.** The brief says each host exports `agentfs.not_built` through the
   plugin's `OTEL_*` variables; true, but off by default in the shipped manifest. docs/csi.md says so.
5. **Landing order with a sibling in the same checkout.** S5's `git push HEAD` twice carried S4's local commits that
   sat under S5's (58b7857, faa1eb4 on the first push), because siblings commit to one branch in one checkout.

## 2026-11-20, M8 fix round: what was applied, where the review was wrong, and M8 residuals

Two reviews (A: F1..F10, B: F1..F9) and the CI report on f34bfa6. Applied (5670701, bac84b1, 3913f35 and the
commit carrying this entry):

- **A-F1 / B-F1 (e2e-csi fstype).** The product was wrong, not the script: the vendored fuser's pure mount dropped
  the `Subtype` option and mounted type `fuse`. It now mounts `fuse.<subtype>` as libfuse does
  (vendor/fuser/AGENTFS-PATCH.md); launcher_host (h) asserts the mountinfo type. The script's check stays strict.
- **A-F2 (mdbook `<pv>`).** Code spans in docs/runbooks/moving.md no longer wrap; mdbook 0.5.4 builds with no warning.
- **A-F3, A-F4, B-F3 (plugin restart).** Chosen: a host stopped (EOF, SIGTERM, SIGINT, a failed reply write) with a
  view open exits 3 at once with no drain, each mount left stale and its client attached, as a crash leaves them; a
  host with no view open drains as before. Every stop the supervisor asks for has no view open, so DeleteVolume's
  release verdict (exit 0) is unchanged; only the plugin's shutdown or death meets views. Tests: launcher_host (m)
  (a fork claim's writer rebound after EOF, both mounts stacked) and csi_trees (the fork scenario: kill -9 of the
  host, then `Driver::restart`, then NodePublish of the same target and a write through the fork's view). Both fail
  with the old drain (probed). Docs: csi.md sections 8, 12, 14; csi-recovery.md; filesystem.md; trees.md;
  library/kubernetes.md.
- **A-F5.** launcher_host (n): a host killed with a view open, a new host given EOF before any open: exit 0. With
  the drain's reconcile removed the test hangs (probed; the host never exits).
- **A-F7 / B-F4 (part).** A drain logs `tree DrainBlocked` with the attached clients of other launchers; csi.md
  section 12 states the limit; csi-recovery.md "A host never exits" gives the remedy.
- **A-F8, A-F9, B-F2.** architecture.md's crate row and configuration.md's front, recording-commands, `--push-grace`,
  `--workload-attached` and `fork finish --timeout` rows rewritten; ci/k8s-csi/move/values.yaml deleted. Errata S3b 1
  and S5 1, 2 are applied.
- **A-F10.** Covered by the csi_trees fork scenario above (kill -9 under a published fork claim).
- **B-F5.** "keyed forks" removed from the reconcile description (filesystem.md, trees.md); the supervisor logs
  "client(s) no view used were ended".
- **B-F6 (documented part).** csi-recovery.md, "A pod that will not mount", names the stall and how to find the
  holder.
- **B-F8.** `Domains::end_keyed` serves `host` and `fork end`; `Serving::open` decides and logs a stacked open once.
- **B-F9.** Chosen: the launcher's 1 s bound per pipe stays (a grandchild may hold the pipes; an unbounded wait would
  never report the exit) and is stated as the one timer beside the backoff (library/kubernetes.md, supervisor.rs).

M8 residuals (each to become a bead):

1. **A-F6: `closed` only after `ViewEnded` has no deterministic test.** An ordering witness needs the host to report
   that it has handled a `close` while a file is still open; no request answers that. Files:
   `crates/agentfs-contracts/src/host.rs` (a `status` request, or `close` answered at detach with a later
   `view_ended` line), `crates/agentfs/src/launcher/host.rs`, `crates/agentfs/tests/launcher_host.rs`. Done when a
   test fails every run with `close` answered at once. Until then the invariant is known untested.
2. **B-F6: one `close` stalls every request of the volume.** Files: `crates/agentfs/src/launcher/host.rs` (defer
   only requests about the same `at`, or answer `close` at detach and report the end separately),
   `crates/agentfs-csi/src/host_client.rs`, docs/library/filesystem.md. Done when a NodePublish of another claim of
   the volume completes while a held file keeps one view's close open (a csi_trees root test).
3. **B-F4: the release verdict reaches the parent only as an exit code.** Files: `crates/agentfs-contracts/src/host.rs`
   (a `stop` request answered `released` or `waiting` with Volume's blockers), `crates/agentfs/src/launcher/host.rs`,
   `crates/agentfs-csi/src/supervisor.rs` (`release`, `stop_host`: answer `FAILED_PRECONDITION` naming the blocker,
   never hold the controller's lock unbounded; a cancelled call leaves no untracked host). Done when DeleteVolume of a
   volume with a foreign client answers at once, naming it, and a root test proves it.
4. **B-F3 (alarm part): a replay the host refuses is dropped with a log line only.** agentfs-csi has no telemetry of
   its own. Files: `crates/agentfs-csi/src/supervisor.rs` (`replay`), a gauge in the plugin or the host. Done when a
   refused replay raises an alarm the example alert rules fire on, and the record is kept until NodeUnpublish.
5. **B-F7: telemetry is off in the shipped CSI manifests** (`OTEL_SDK_DISABLED=true` in deploy/csi/node.yaml and the
   chart), while the sidecar exports to a placeholder collector. Not changed here: a collector address is the
   operator's, and each host flushes its exporter at exit, so an unreachable default would delay every host stop on
   kind. The goal's row in one-engine.md says the log line is always on and the gauge needs the operator's collector.
   For the owner (general guidance): keep the alarm opt-in under CSI, or ship a default endpoint as the sidecar does.

## 2026-11-20, memory-and-stalls run 1: spec errata from the slices

### 2026-11-20, agentfs-ln6.1 (close tracer): where the frozen page is wrong

1. **Blocking: `LenRule::Fixed(u64)` cannot satisfy A2.** The page asks for `on(saved_len)` to fail with `TooLarge` exactly when folding `Call::len_after` over the calls from `saved_len` fails. An `Append` before a `Truncate` can overflow (`len.checked_add(n)`), and whether it does depends on the saved length. `Fixed(s)` has no room for that bound. Counter-example: calls `[Append{len: u64::MAX}, Truncate(0)]`. The fold from 0 is `Ok(0)`; the fold from 1 is `Err(TooLarge)`; the rule is `Fixed(0)` for both. Without the fix, `call` accepts a call that today's re-fold refuses (after another handle's save grows the file), and the close's replay then fails with `TooLarge`. Proposed re-cut: `Fixed { len: u64, grown: u64 }`, where `grown` is the `add` of the calls before the first truncate and `on(L)` fails iff `L + grown` overflows. Keep A2 as written. The probe "Truncate composes as Shift" still applies.
2. **A4 and A6 in `tests/tree_close.rs` cannot read "a `#[cfg(test)]` counter in readback.rs".** An integration test compiles the crate without `cfg(test)`. Either move the reader-count assertion into a unit test (`call_path.rs` or `recording/mod.rs`), with `tree_close.rs` keeping the public `actor_lock` and `bytes_compared` deltas, or expose the count publicly (for example as a `Stages` field, which is another lane's file).
3. **`ActorGuard { since: Instant }`:** `stages.rs` records a run only through a `Timer` it starts (its fields are private, and there is no add-a-run function). The guard must hold a `Timer`. The field is private, so this is cosmetic.
4. **`close_frag`'s `lock_hold_max_ms` from `stages.actor_lock.max_nanos`** is the maximum over the whole process. In a full figures run it includes the create workload's holds and any compaction's. Only `--only close_frag` measures the close itself. State this in the budget text, or have the figure run alone.
5. **A6:** "100k-call open closed on the fast path" holds only for overwrites of a small range plus one length change. 100,000 appends make about 100,000 pieces, which is above `SMALL_PIECES` (32), so the close is never fast. The test must say which.

### 2026-11-20, agentfs-ln6.2 (host protocol v2): where the frozen page and the review were wrong or short

1. **Frozen signatures changed by the review's amendments**:
   - `lock_claim` is `fn lock_claim(&self, handle) -> Result<OwnedMutexGuard<()>, Error>`. It tries the lock and returns ABORTED on conflict, not an async wait (amendment 2, the CSI rule for rejecting secondary calls).
   - `idle` is `fn idle(&self, using: &Using, volume) -> bool`. It must take the caller's guard: otherwise deciding idle and removing the host could not be one critical section (amendment 1).
   - `HostClient::start` returns `Result<_, StartError { Gone(HostGone), Version { host, plugin } }>` (amendment 5).
   - New `HostClient::close()` (amendment 1).
   - `HostDriver.in_flight` is computed from `closing` and `waiting`, not stored (amendment 7).
   - `OpenView` gains `holds: Resources`, which is where a Closing gets the fork key (amendment 7).
   - `unpublish` takes `self: &Arc<Self>`.

   The page should be re-cut to match.
2. **`using` changed meaning (amendment 6).** It now holds only the opens in flight, not "claims with a record or an open in flight". `idle` = `using` empty and no record on disk, so the records stay the one source of truth. Sites that now decide through `idle()`: restart, release, stop_if_idle. The plugin-start reconcile goes through stop_if_idle.
3. **Docs not changed in this commit, because they are not owned paths**:
   - docs/library/filesystem.md:701-707 still says "Protocol, version 1 ... exactly one reply per request, in order" and shows `{"v":1,...}`.
   - docs/library/kubernetes.md:124 still says "Each volume's calls run one at a time under a per-volume" lock.
   - docs/csi.md should state the ABORTED answer to a secondary NodePublish or NodeUnpublish.

   AGENTS.md requires library docs to travel with the code. The split to ln6.7 needs either that commit to land next, or an owner exception recorded in OWNER-GUIDANCE.md. I recorded neither.
4. **Design page not edited (memory-and-stalls.md is not owned):**
   - §5 needs the review's alternative written up: refuse a request that overlaps a closing view with a retryable kind, and keep no waiting queue. Why per-resource waits are better: the parent cannot tell when to retry; every refusal costs a round trip and a backoff; the order between a close and a re-open of the same mount would come from the parent's retry timing, not the host. Two costs to state: the host keeps waiting requests in memory, and none can be cancelled (there is no `$/cancelRequest`).
   - §6 row 6 is wrong. Labels come from the target, so a re-publish at a new target shares no client with the closing view. On the plugin side the second call of the same claim is answered ABORTED, not queued. A fork claim's re-open ends in ForkTaken.
5. **Fork waits kept (finding 10 rebutted).** Requests that name the same fork are answered in v1 order. Dropping the `fork` field would itself need a frozen-interface erratum. C3 asserts that fork_end waits.
6. **serde limit.** `#[serde(deny_unknown_fields)]` on the internally tagged `Request` cannot reject extra fields on `reconcile`, which has none. This is pinned in the test comment.
7. **Residuals 1 and 2 are covered by this commit.** M8 residual 1 (A-F6) is covered by launcher_host (o) with the probe "answer close at once -> fails". Residual 2 (B-F6) is covered by csi_trees `a_held_close_stalls_only_its_claim`. The commit marking them applied in .claude/errata.md is the main loop's: errata.md is not an owned path.
8. **Still open: a waiting host request has no cancel.** A request queued behind a close that never ends waits forever at the host. The plugin side is bounded: one task per claim, retries get ABORTED. A fork_create or fork_end that waits holds the volume's start lock for the whole call, which would stall that volume's starts and stops. It can only wait while a view of that fork is closing, which in practice the controller's claim_in_use refusal prevents. This is close to B-F4 residual 3.

### agentfs-ln6.3

agentfs-ln6.3 frozen page, findings (none stopped the slice; nothing written to .claude/errata.md):

1. `History::owners(&self) -> &Owners` cannot be written as specified. History keeps its state behind a Mutex, so it cannot hand out a reference. Nothing calls it, and an unused function would fail clippy -D warnings, so it was not added. BuiltSnapshot::owners() -> &Owners was done as specified. If History needs this question later, it should return an Owners clone (O(1)).

2. The D1 probe as written does not prove the test can fail. Making `trees` (or `opens`) a BTreeMap again leaves the ratio at about 1.46, because each Tree's own maps are now persistent and cloning the outer map of a few trees is cheap. Making Tree.bytes a BTreeMap still passes too (ratio 1.6). 50 creates make only about 3 batches, so the fixed per-create cost (about 8.6 MB) hides a single map that deep-clones again. A probe that shows the class needs Shape or Tree maps back as BTreeMaps (the original code gives a ratio of 4.27), or a test that settles after each create.

3. A batch is still proportional to the volume in one place. `objects: imbl::OrdMap<TreeId, Arc<Objects>>`, with `Objects = BTreeMap<u64, u64>`, is rebuilt by `objects_of(&shape)` for every tree a batch moves, which walks every name. This is most of the remaining growth in D1 (about 4 MB more at 20,000 files than at 2,000). Making it proportional to the batch means changing the `Objects` type, which journal/fold.rs, journal/commit_point.rs and history/format.rs::read_snapshot also use; none of those files are mine. It is left for whoever owns them (a later bead could update the map per step from the keys each step returns).

4. Environment: worktrees -2, -4, -5 and the main checkout all have branch `work` checked out. When one lane commits, the others' HEADs move, and their indexes then show the other lane's changes as staged reverts. When I committed, my index held a staged reverse of 683bb83 (seven host and CSI files). My commit used explicit paths and contains only my 7 files, but any commit without paths from one of these worktrees would revert a sibling's work.

### agentfs-ln6.5 (memory-and-stalls §4.5): an uncounted repack spawn under tree_actor/ is forbidden by a source test

The frozen page puts the repack task in `crates/agentfs-core/src/tree_actor/history/repack.rs` and makes its `git repack` child "uncounted (not on the actor's path)". It also lists only repack.rs, built.rs, history/mod.rs and tree_actor/mod.rs as owned.

But `crates/agentfs-core/tests/tree_stages.rs::one_spawn_site` fails on any `.spawn()`, `.output()` or `.status()` in the production code of every file under `src/tree_actor/`. Its message is "starts a process ... outside git_cli::counted", and `git_cli::counted` is the only spawn allowed.

So the bead cannot land as written:
- **Spawning through `git_cli::counted`** breaks "uncounted". A background repack would also make the process-wide count of git children, which tree_stages.rs checks per operation, nondeterministic.
- **Spawning directly in repack.rs** fails `one_spawn_site`.
- **`crate::repack::repack`** is excluded by the page. It is also unusable: it does nothing when there are no loose objects, and the tree history is packs only.

Re-cut: add one of these to the bead's owned paths.
- (a) `crates/agentfs-core/tests/tree_stages.rs`: exempt `tree_actor/history/repack.rs` from `one_spawn_site`, saying why: the repack task is not on the actor's path.
- (b) `crates/agentfs-core/src/git_cli.rs`: a second named spawn site for uncounted background work, plus the guard's count changed from 1 to 2.

Also for the re-cut:
- **E5:** `ci/trees-figures.py` has no `long_history` workload on claude/kind-fermi-vvxywi at abc1ab6. Lane B's harness hasn't landed, so E5 cannot be measured yet.
- **E2/E4 test hold:** git writes the new packs and deletes the old ones inside one process, so the hold "between git's pack write and delete" needs a test-only git wrapper per git directory. It runs the repack without `-d`, waits on a fifo, then runs the full command. The page should say so.
- **Killing:** `join` should kill the child's whole process group. Killing only `git repack` leaves its `pack-objects` child running, holding stderr open and writing `.tmp-*` files.

## 2026-11-20, memory-and-stalls run 2: spec errata from the slices

### 2026-11-20, agentfs-ln6.1 (close tracer, landed 1b94d8b): where the page or the code was short

1. **The A1 probe cannot fail.** The page's probe is "digest_since counts record > first instead of >=". It is an equivalent mutant. `first` is the open's own record, which is an Open, AnonymousOpen or Create step with no tail, so no frame ever has record == handle. Measured: with `>` in place, A1 passes at all 63 seeds. A probe that does fail: count every frame as the close's own. A1 then fails at seed 1, step 4. The page should name a probe that can fail.

2. **A pre-existing builder disagreement, not caused by this slice.** History build "Disagrees" fires on some histories with page writes and two opens of one file.
   - A/B on 1160def (a release of the base tree, an integration test using only the public API): seed 11 fails at record 60 there, as here. With a 3 ms pause before each close, seeds 11, 19 and 26 fail on 1160def. It depends on when the builder's batches run.
   - Hypothesis, not verified: FileMap::digest decides which pieces are the close's own by `pos.key()` alone, ignoring a piece's `at`. A page-write run's remainder that an earlier save left in the saved map has the same key as the piece this close applies. It then counts as "own" when the saved map is on frames, and as kept when the map was rebased onto the checkout. The actor and the builder rebase at different times, so their digests differ.
   - A compatible fix would carry each own piece's length, which changes the frozen own_keys/digest signatures. This needs its own bead and the model-verification skill.
   - A1 therefore does not assert that settle() succeeds. It collects builder failures and prints them with --nocapture. Every close is still checked against the oracle.

3. **`ActorGuard` is not exported from tree_actor/mod.rs**, and that file is not this lane's. session.rs cannot name the type, so SessionGuard holds it boxed as `Box<dyn DerefMut<Target = TreeActor> + 'a>`. That costs one small allocation per FUSE request. The page's field type `actor: ActorGuard<'a>` needs `pub use call_path::ActorGuard` added to tree_actor/mod.rs.

4. **close_frag's 50 ms lock-hold bar is missed because of compaction, not the close.** On a release build, ext4, with `--only close_frag`, actor_lock max_nanos was 136.3 ms and stages.compaction max_nanos was 136.2 ms. The close itself went from 110.4 s to 0.086 s. write_1m's longest hold is compaction too: 834 ms. Section 4.1 does not bound compaction, which copies live frames forward while holding the lock, so this bar will keep warning until a slice does.

5. **write_1m's peak memory got worse: 670 MiB before, 774 MiB after** (bar 256, warns either way). The likely cause is the Large `imbl::OrdMap`: a million appends at the end leave B-tree leaves about half full. This slice removed the call list, which offsets part of it. "Merging sequential write pieces" is listed as deferred on the design page.

6. **The fast path only partly replaces the view path.** When the base did not move and the file has no page writes, CloseWork::decide never takes a CallReader either. So the A4 reader-count probe ("disable the fast path, then a reader is taken") is not caught by a reader count. It is caught by tests/tree_close.rs's lock-hold count, which goes from 1 to 2. Amendment 2's reader-count unit test still checks A6 (no reader for 100,000 calls) and that a moved base takes a reader.

7. **Docs were not updated, because they are not owned paths:**
   - docs/trees.md ("File maps", the close).
   - docs/performance.md (the new figures, `--self-test-warn` and step 5).
   - docs/library/core.md (`Recording::close_with`, ActorGuard, `TreeSession::close`).

   AGENTS.md says docs travel with the code. The memory-and-stalls.md design page also stays for now.

8. **pages_build_image now reads the journal under the lock** to rebuild a file's page-cache image. Each open with calls costs one walk of the segments' frame heads. That replaces walking an in-memory call list, so the first page fill after each save may stall longer than before. Not measured.

### 2026-11-20, agentfs-ln6.5 run 2: the repack beside fast-import breaks Feed::finish (blocking)

The frozen page has the repack task run `git repack --geometric=2 -d` "beside `git fast-import` ... never under the builder's lease and never stopping it". `crates/agentfs-core/src/tree_actor/history/feed.rs::Feed::finish` assumes fast-import is the only writer in `objects/pack`:
- it lists the directory before and after;
- it skips only `tmp_*` names;
- it opens and fsyncs every other new name, and an ENOENT fails the batch.

A concurrent repack's `.tmp-<pid>-pack-<hash>.{pack,idx,rev}` files appear in that difference and are renamed away before the open. The batch then fails with `BuildError::Io(2)`, and `settle()` returns `History(Failed(Io(2)))`.

Measured on wip/agentfs-ln6.5 (f279d6b):
- A probe that logged instead of failing saw several vanished `.tmp-*` names per 300-batch run.
- `cargo test -p agentfs-core` fails `tests/tree_actor.rs::append_finishes_a_partial_line_and_close_replays_on_the_current_head` this way.
- The slice's own E4 failed 2 times in 20 runs.

The same race can also delete fast-import's own new `pack-*` before its fsync: a repack can roll that pack in and delete it before the open. That case was not observed.

Re-cut: add `crates/agentfs-core/src/tree_actor/history/feed.rs` (Feed::finish only) to ln6.5's owned paths, with this rule:
- Skip `.tmp-*` names: they are a repack's files in flight, not fast-import's.
- Treat ENOENT on a new `pack-*` name as rolled in by a repack. git renames its new pack into place before it deletes old packs, and `core.fsync=objects,derived-metadata` fsyncs the new pack's contents. The builder's own `objects/pack` directory fsync, which follows, makes the rename durable.
- Add a test: batches with the real task running, each settled, with no failed batch. (A scratch 300-batch stress shows the failure today.)

The alternative is a mutual-exclusion between a batch's fast-import-to-fsync window and a repack. It needs builder.rs and contradicts "never stopping it".

Also for the re-cut:
- (1) The page's `child: Arc<Mutex<Option<Child>>>` holds the stop flag too: `Mutex<Slot { child, stopped }>`.
- (2) The repacker starts in `serve` right after the volume is constructed, not inside `start_history`. A later recovery failure then stops it through Drop.
- (3) E2's probe "Odb with refresh disabled" cannot be built through git2-rs. libgit2 refreshes the pack list on a miss.
- (4) A daemon SIGKILL leaves an orphaned repack. Its own process group means nothing kills it. It is harmless: it is cut short or finishes, and keeps every object. PR_SET_PDEATHSIG would end it.

## 2026-11-20, memory-and-stalls: the repack beside live readers (fixed in 80c92d2)

1. **The design's section 4.5 was unsafe.** It ran `git repack --geometric=2 -d` in the store's own `objects/pack` beside live readers. git and libgit2 find a pack by listing the directory, then opening the index lazily, and list again only once on a miss. A reader that listed before the install and opened after the delete reported an object as missing (CI on 6cd7edf: four conformance hits). No object was lost: strace reproduction, `git cat-file -e` succeeds afterwards. Fixed in 80c92d2: each run repacks in a scratch object directory (hard links plus `GIT_ALTERNATE_OBJECT_DIRECTORIES`), links the new pack in with its index last, and deletes rolled-up packs only at a run that starts on a later built event. This is the deferral git's own incremental repack (`multi-pack-index repack`, then `expire`) uses. The repack child gets `PR_SET_PDEATHSIG`.
2. **E2 of the ln6.5 page could not fail** for a reader between listing and opening. Replaced by `a_pack_a_reader_listed_outlives_the_run_that_rolls_it_up`.
3. **The run-2 re-cut's "skip `.tmp-*`" rule in `Feed::finish` is moot.** No repack writes in `objects/pack` any more.
4. **Open.** `docs/architecture.md` still says the repack runs in the git directory (the docs step owns it). `clean_packs` in `history/mod.rs` still fails a reopen on `ENOENT`; tolerating it would be more robust (residual).

## 2026-11-20, memory-and-stalls run 3: errata from the slices

### 2026-11-20, agentfs-ln6.4 (one copy of each fact, overlay; landed 2f6b2b9): where the page was short or wrong (none stopped the slice)

1. **F1 cannot pass as measured, whatever the overlay saves.** The bar reads RssAnon once history is built. glibc keeps freed heap resident.
   - The builder lags far behind `create`: 20 s of building after 11 s of creates at 20k files, 66 s after 30 s at 50k. During the lag the overlay holds every unbuilt change. So the peak equals the old permanent copy, and RssAnon never comes down from it.
   - Measured on a release build, local ext4: before 2.61 to 2.70, after 2.60 to 2.71 KiB/file (20k and 50k).
   - After `gdb -ex 'call malloc_trim(0)'`, live anonymous memory went from 52.8 to 40.0 MB at 20k and from 131.6 to 93.5 MB at 50k, about 1.87 KiB/file.
   - Meeting the bar needs one of: malloc_trim (deferred on the design page), a builder that keeps up, or a bar read some other way. That is the owner's call.

2. **The frozen write API gives no step to most overlay writes.** Shape::save, drop_object, add_anonymous and Files insert/get_mut/remove carry no step. These are lane B's call sites, so I did not change their signatures.
   - Such entries are noted at UNSEALED (u64::MAX).
   - I added `SavedTree::seal(last)`, which the actor runs before `rebuilt` with its last record number. This is safe because every change noted at no step happened at or before that record.

3. **`rebuilt` is O(entries changed since the last built event), not O(entries dropped).**
   - Names, Dirs and Objects are built as built map plus changed keys re-applied, so each read is one lookup and range reads (children, descendants) stay simple.
   - Files is the page's two-layer form.
   - Dirs needed a guard: a directory renamed in the overlay may already be numbered at its new path. F3 found this.

4. **Files::get_mut and Files::iter have no caller outside tests.** With -D warnings they would fail clippy as dead code, so they carry `cfg_attr(not(test), allow(dead_code))`.

5. **The actor also rebuilds in after_step, not only in on_built.** A library user has no take worker calling `progress`, and without this its changed layer would grow without bound.

6. **Docs not updated:** docs/trees.md and docs/library/core.md (overlay, built.log on disk only) are not owned paths.

7. **Environment:** a long TMPDIR (the scratchpad path) breaks `tests/history_service.rs` with "path must be shorter than SUN_LEN". Run with TMPDIR=/tmp. The disk filled to 100% once during the run, which broke `tree_batch_alloc` until space was freed.

### 2026-11-20, agentfs-ln6.6 (memory-and-stalls §4.7, restart): the attribution contradicts the frozen page (blocking)

Measured at 2f6b2b9 on a release build. The state was 100,000 `fig-create-N` files made through a FUSE mount by the `create` workload's write. History was built, the view stopped with SIGTERM, and the store held 818 packs (history.git 2.0 GB). On the restart I sampled /proc RssAnon and RssFile every 100 ms and marked each start_history and serve phase with probes. massif ran on a 25k-file state.

Today's restart at 100k: VmHWM 1,087 to 1,266 MB, mount 37 to 70 s (shared 4-CPU box). Attribution of the 1,087 MB peak:

| Contributor | Size |
|---|---|
| libgit2 pack and index mmaps | about 661 MB, 61% (570 `.pack` + 95 `.idx` over 809 index files), touched by the first full read of the head's trees and never unmapped under libgit2's default mwindow limits |
| checkpoint branch of start_history (`built > folded_built`, the usual case after a workload) | +189 MB anon, from reading the whole tree at the checkpoint (read_trees, read_snapshot then Tree::install); replay and objects_of add under 1 MB |
| read_trees at the built point | +55 MB anon |
| history::owners | +47 MB kept and about 60 MB transient (`git log` of every direct save held whole, plus a 16 MiB read_to_end buffer) |
| copy_all | 0 MB at the peak, but 14 to 45 s of the mount |

What-ifs (no code landed):
- libgit2 `set_mwindow_size(32 MiB)` + `set_mwindow_mapped_limit(64 MiB)` + `set_mwindow_file_limit(32)`: VmHWM 517 MB.
- Store repacked to one pack: VmHWM 517 MB, mount 27.5 s (copy_all 19 s of it).
- Both together: 517 MB, mount 19.3 s.

Where the page is wrong:
1. **The frozen interface cannot express today's semantics.** `read_tree(repo, tree, head) -> Result<Tree>` lacks the journal's object numbers (`Objects`; git-model §3: the journal's, never history's) and the bytes-map source (checkout path, `bases`, built point). Building a Tree from the head alone needs a second renumbering pass, which is the forbidden intermediate copy. Re-cut: `read_tree(repo, tree, head, objects: &Objects, map: impl FnMut(u64, &str) -> Result<Arc<FileBytes>>) -> Result<Tree>`, filling the persistent maps straight from the leaves with no `referee::Snapshot`. format.rs must join the owned paths; it is under history/ already.
2. **The design misses the largest contributors.** (a) 61% is file-backed pack pages. The repack lane (§4.5) or libgit2 mwindow limits fix it. The limits are process-global and belong in `crate::history::init_libgit2` (history.rs, not owned by this lane). (b) The checkpoint branch reads the whole tree a second time. A re-cut should read it once, for example by carrying `objects` and `locals` as of the built point in the built log or the checkpoint, or by replaying on the one read. (c) owners holds every direct-save message at once; it should stream (tree_git.rs commit_messages, not owned). The marker lever saves mount time (copy_all), not memory.
3. **The marker as specified is unsafe on power loss.** TreeGit::checkout_entry writes each file with `fs::write` + rename and never fsyncs it. A marker fsynced last can then outlive lost or empty checkout files, and a restart would serve wrong bytes from the base maps (principle 1). The re-cut must make the checkout's files durable before `mark_checkout`: one `syncfs` on the state area per checkout, or an fsync per written file in tree_git.rs. Otherwise markers should be written only at a clean stop, after a syncfs.
4. **G1's bar is superseded by the amendment.** The page holds `hwm_mib <= 256` at about 50k files; the amendment holds 512 MiB VmHWM at 100k. Even with every lever above, 100k measures about 517 MB before the read_tree and owners changes. The 256 MiB at 50k bar is not reachable by this page's levers alone.

Suggested re-cut owned paths: add history.rs (init_libgit2 mwindow limits) and tree_git.rs (commit_messages streaming; checkout fsync or syncfs). The marker should live outside `trees/<tree>/` (for example `trees/<tree>.files`, which remove_checkout and remove_stale_checkouts must also delete).

## 2026-11-20, agentfs-ln6.8: the builder's false "Disagrees" on page writes (fixed)

1. **Run 2's ln6.1 item 2 hypothesis is confirmed.** `FileMap::digest` named a close's own pieces by `FramePos::key()` (record, offset of the frame position) through `own_keys`. A page write's run part that a close applies has the same key as the run an earlier close saved, so that earlier save's remainder counted as the close's own while the saved map was on frames (the actor), and as kept once it was rebased onto the checkout (the builder). Probe at seed 11 step 60: the actor's map held `[149..160) frame(58,0)+3 own=true`, the builder's `[149..164) base`; the bytes were equal.
2. **Fixed by making the digest a function of the saved length and the close's calls alone** (`FileMap::close_digest`): it lays the calls on a map whose bytes are all "as before" and names each visible piece by file offset, length, record and frame offset. `own_keys` and `FileMap::digest(&own)` are gone. For every history outside this bug the value is unchanged (the quick path's `digest_since` equals it: `digest_since_equals_the_close_digest_of_the_calls`), so journals recorded before replay as before; a journal holding a save of exactly this shape would still fail its check once.
3. **A1 now fails when a build fails.** `two_opens` panics on a failed `settle()` and settles at the end of each history; seeds 11, 19 and 26 (26 with a 3 ms pause before each close) are named regression tests.
4. **Not done: page writes in the model's and the conformance generators.** The model has no page cache (MODEL.md "Left out": no `mmap`; VERIFYING.md: "R1.4 mappings: no mmap call"). Adding it is a model extension, not a generator tweak: a new `TreeCalls` call and trace line, R1.4's semantics in the model (the per-page bases a page write diffs against, `saved_through`, which close saves a store, recovery's `calls_for_close`), a mounted replay that drives one exact page write (mmap store plus `msync` on a whole page), and new regression-seed cover and digests. It needs its own bead under the model-verification procedure.

## 2026-11-20, memory-and-stalls run 4: errata

### 2026-11-20 agentfs-ln6.11: invariant 3 contradicts the overlay (implementer stopped)

The frozen page of agentfs-ln6.11 says that if the builder publishes no map of a flagged (tree, object) at that step, the actor keeps serving the bytes the workload saved. The code does the opposite:
- builder.rs:737 puts the replay into the builder's trees (`t.bytes.insert(open.object, saved_map(map, seq))`). Those trees are handed out by `built_event` (builder.rs:1471).
- In tree_actor/mod.rs:1599-1619, `on_built` runs `rebuild()`, which calls `saved.rebuilt(built, through)`.
- tree.rs:740-743 replaces the actor's bytes with the builder's and drops the actor's own changes at or before `through`.
- tree_actor/mod.rs:772: `start_history` reads every file from its checkout ("Each file reads from its checkout as built"). Invariant 7 leaves the kept frames out of the fold, so nothing restores the saved bytes after a restart.

Result: once the builder passes a flagged step, and after every restart, the workload reads history's replay. Today's fatal `Disagrees` turns into a silent change of a file the workload can see.

Decision needed (amendment 1 of the protocol review, an absolute-requirement question for the owner):
- (a) Serve the replay, with the cost stated: "the file may silently change to history's bytes".
- (b) Serve the saved bytes, through a named mechanism that holds across `rebuild()` and `start_history`.
- Or keep the step fatal for that file only.

Amendments 2-10 of the same review are also open:
- tests where the replay's bytes differ from the actor's;
- verify run with the same binary is deterministic, so it confirms the mismatch and then drops the evidence;
- the notes lock is held across an fsync on compaction's call path;
- flags should be keyed by (step, commit);
- the notes ref is a primary record, yet architecture.md, git-model.md and runbook section 5 call the refs a copy;
- a note that fails to parse blocks the mount;
- the owner's words are mixed with design choices in OWNER-GUIDANCE.md:150 and the bead;
- the page has no Costs section;
- the exit-code contract differs between main.rs's doc and invariant 10.

The page needs a revision and a re-cut before this bead can be built.

## 2026-11-21, memory-and-stalls run 5: errata

### agentfs-ln6.12

agentfs-ln6.12 engine (run 4 resume), implementer findings:
1. OWNED omits crates/agentfs-core/src/tree_actor/journal/fold.rs. Plan item B removes the only caller of `Replay::shapes`, so leaving the function in place fails `clippy -D warnings` (dead_code). Commit 018cd1d deletes `Replay::shapes` there, 14 lines and nothing else. A sibling lane editing fold.rs may need to rebase over that.
2. Mutation probe P3 is wrong about what catches it. `git log` lists newest first, so the record at EOF is the oldest reachable commit, not the newest direct save. With P3 applied, tests/tree_actor.rs stays green (51/51). Only the unit test (c), `each_commit_message_is_streamed_once_and_whole`, fails.
3. The page's figure of "+189 MB anon at 100k" for the double read is not what I measured. On release builds after a SIGKILL with built 400 records past the checkpoint, the sampled peak RssAnon fell from 370-386 MiB to 328-333 MiB (about 40-55 MiB), and mount_s from 46-49 s to 38-39 s.
4. The page's arena figure "2.52 to 1.95 KiB/file" does not match this session's measurement of 2.67 to 1.64 at 50,000 files. docs/architecture.md now records the measured figures.

### agentfs-ln6.13

None appended: nothing in the page stopped the slice. Non-blocking findings for the main loop to record if it wants: (1) the page's `LagGate::overlay`, written as admits-before vs admits-after, costs O(unbuilt steps) on every release of the actor's lock and made the release create figure about 25% slower (63 s to 78-84 s on ext4); the slice recomputes admission only when a report crosses the limit. (2) The page's burst test needs a take worker calling `progress` at built events (as the view's take_worker does), or `admit` never returns once the tree is held, because only lock releases report the overlay. (3) With the default 32 MiB the create workload's overlay never reaches the limit and the RssAnon peak is unchanged (about 81 MiB); at 8m the peak falls only to 77.5 MiB, so the overlay is a small share of RSS. (4) docs/library/core.md's classify table calls `includeIf.<c>.path` "none: structure", but `classify` answers `Other` (left as is).

## 2026-11-21, agentfs-ln6.7 (memory-and-stalls docs): residuals closed, and where the docs were behind

1. **M8 residual 2 (B-F6) is done**, landed in 683bb83 (host protocol v2): a request waits only while a pending close
   holds a resource it names, and the plugin locks per claim. csi_trees `a_held_close_stalls_only_its_claim` is the
   done condition. M8 residual 1 (A-F6) is covered by launcher_host (o) in the same commit. Documented in
   docs/library/filesystem.md ("The view host command"), docs/library/kubernetes.md ("The supervisor") and
   docs/runbooks/csi-recovery.md ("A pod that will not mount").
2. **Docs the slices left behind, now current:** docs/library/filesystem.md still described protocol v1 ("exactly
   one reply per request, in order"); docs/library/kubernetes.md still said each volume's calls run one at a time;
   docs/runbooks/csi-recovery.md said one held close stalls every claim; docs/trees.md's "File maps" described the
   close applying every call under the lock and the actor taking rebased maps at its next call; docs/architecture.md
   said the repack runs in the state area's git directory; docs/trees.md's limits named one gauge.
3. **Design page deleted** (docs/target-architecture/future/memory-and-stalls.md). Its deferrals that are not
   documented behaviour stay open, for a bead if wanted: untracked fsync on the view's FUSE thread (never measured);
   trimming `owners` and `heads` at a checkpoint; merging sequential write pieces (now a stated limit in
   docs/trees.md); compaction under the actor lock (a stated limit); the restart checkout marker of the page's
   restart section (not built: the failpoint `tree.checkout_before_marker` is registered with no marker after it);
   `clean_packs` failing a reopen on `ENOENT`; a waiting host request has no cancel (stated in
   docs/library/filesystem.md).

## 2026-11-21, P0: the repack task deletes a live pack and loses history (open)

The forks e2e failure on 2412162 (passed in the push run, failed in the PR run) is a product bug. Root cause, reproduced locally with the image's git 2.47.3 in a library stress test (forks, landings, drops and `wake_waiters()` beside the task, 60 loops): 6 of 20 runs lost objects at 2412162, some leaving the store empty; 0 of 10 with the repack skipped; 0 of 20 at d43b95b (before ef4ad49).

Mechanism (`crates/agentfs-core/src/tree_actor/history/repack.rs`): pack names are content hashes, so a later roll-up can produce the same name as a pack an earlier run set aside. The scratch run still sees set-aside packs through the store alternate (`:484`), so the output name flips A, B, A. `link_pack` treats `AlreadyExists` as installed (`:362`), and the set-aside bookkeeping is by name (`:547`, `:245`, `:233`, `:445`), so `delete()` removes the live copy. Runs with no new objects (the epoch also moves on `failed_event` and `wake_waiters`) make repeats likely; ef4ad49 deletes set-aside packs sooner, raising the rate. Evidence: the job log's `git show root:` "does not exist", `merge-base` "Not a valid commit name 969a77b", `fast-import` "Not a valid commit: b900c82". Patch of the stress test: /tmp/claude-0/forks-rca/rca-test.patch (not committed).

This is the fourth defect in the bespoke repack mechanism (Feed race, reader race 80c92d2, set-aside growth ef4ad49, live-pack deletion). Per the owner's rules that is a wrong premise, not an instance to patch: options go to the owner.

## 2026-11-21, PR #40 fix round: the P0 repack entry closed, and where the findings and docs were wrong

1. **The P0 above (the repack deleting a live pack) is fixed** by the owner's option B. The bespoke scratch-directory task is replaced by git's incremental repack (`crates/agentfs-core/src/tree_actor/history/repack.rs`): `multi-pack-index expire` at a run on a built event after the last roll-up, then a geometric roll-up, then `multi-pack-index write --preferred-pack`. Which pack goes is git's judgement from the index, so a roll-up that writes a pack the store already holds keeps it (`a_roll_up_that_writes_a_pack_the_store_holds_keeps_it`; the same packs rolled up twice give the same name, checked with git 2.43). The RCA stress test is committed as `forks_beside_the_task_keep_every_object` (40 loops): 0 of 12 runs failed, against 2 of 10 on the old task (reviewer's run).
2. **Where option B as worded was short.** `git multi-pack-index repack --batch-size` does not name the pack it writes, and `expire` deletes the superseded packs only if the index prefers the new pack (git breaks ties between packs of the same second by name, so without `--preferred-pack` a superseded pack can stay referenced). The roll-up is therefore written by `git pack-objects --stdin-packs`, the call `git repack --geometric` makes, which prints the new pack's name. Recorded in OWNER-GUIDANCE's bracket note; the owner may prefer the literal command.
3. **A second writer in `objects/pack`.** `multi-pack-index write` makes `multi-pack-index.lock`; `Feed::finish` failed a batch with ENOENT when that name went before its fsync (seen once in the full suite). `Feed::finish` now tolerates any new name gone before its fsync. A stale lock goes at start in `remove_stale_locks`.
4. **The overlay that never shrank (high)** is fixed in `TreeActor::rebuild`: when entries are left past the built point and the last record is no step, the actor asks the builder to build through it (`History::want_through`). Test: `the_overlay_goes_when_the_last_record_is_no_step`, deterministic through a test-only hook at the builder's batch start and read (`History::on_batch_read`); without the fix it reproduces the reviewer's `10240 B, built through 81, the write at 82`.
5. **A write to an open being closed** is refused (`NotFound`, as a second close is) in `Recording::call`, rather than journalled and left out of the save.
6. **`record()` on a restarted host that refuses the view** now removes the record and fails the publish with the refusal. Not fixed, deferred: the root cause, a restart's reconcile ending the client of an open still in `using`. A fork claim caught in that window now fails loudly (`FAILED_PRECONDITION`) on every retry, where it got OK with a stale mount.
7. **Docs that were wrong.** docs/trees.md said `tree HistoryStalled` is logged once; the code logs each turn to `Stalled`, at most once per built event under a burst, and the doc now says so (no hysteresis added). docs/performance.md named the journal's compaction as `close_frag`'s longest lock hold; the reviewer's perf trace puts it at the rebuild dropping the 200,000-piece map (`Files::rebuilt`), and the row now says so with the reviewer's figures. Moving that drop off the lock is not done. CHANGELOG said host protocol v1; docs/csi.md's NodePublish and NodeUnpublish rows lacked `ABORTED` and the version refusal.
8. **Left as found, reported.** Commit 683bb83's message says a v1 parent is refused at hello, which is wrong (a v1 plugin never checked `v`). History is not rewritten. The node-wide `using` lock with disk reads under it (design §4.6 asked for a volume-scoped set) is unchanged. `Disagrees` still stops all history; that is the owner's call. The unverified alarm clears only at a restart, as the runbook says. The restart checkout marker (design §4.7) is not built: the misleading comment and the unused failpoint `tree.checkout_before_marker` are removed.
9. **Found by CI on 0021dad (phase-1 power loss, ext4 and xfs, `git fsck` at a flush checkpoint).** `git multi-pack-index expire` unlinks the packs it drops before it writes the index that leaves them out (git's `midx.c`, `expire_midx_packs`), so a power cut or a kill between the two leaves an index naming packs that are gone; `git fsck --full` fails ("failed to load pack in position 0") and so would later index writes. The start now removes such an index (`repair`, reading the index's `PNAM` chunk), and a stop lets an expiry finish rather than killing it. Test: `an_index_naming_a_deleted_pack_goes_at_the_next_open` (fails without the repair with the same fsck message); the repack tests now run `git fsck --full --strict` after each stop. The power-loss job itself needs dm-log-writes and runs only in CI.

## 2026-11-21, PR #40 close-out: process lessons

1. **A fix agent rebuilt a mechanism without its reviewed design.** Run 7's end fix agent, briefed to fix CI, replaced the repack (0021dad, 91ad04f) before the reviewed design page existed and before the owner's three answers. The final verify then found 7 defects that the design had already covered (failed-step index drop, free-space floor, stray sweep, built-event counter, deferral test). Lesson: a fix brief names its scope; a finding that needs a redesign returns to the main loop.
2. **Briefs pushed root-only tests.** An agent running as root wrote a test that opened git's read-only packs for write; CI's non-root runners failed it (fixed in b217c2f). Lesson: test briefs say CI runs unprivileged.
3. **Container restarts lost two runs.** Unfinished worktree work was saved to `wip/` branches (cf22d8e, 705e05a) and resumed; nothing was lost. Check-ins now save worktrees before relaunching.
4. **Worktree builds filled the disk four times.** Each worktree builds its own target (20-37 GB). Check-ins now free finished worktrees.

Open residuals: agentfs-ln6.9 (compaction holds the actor lock), ln6.10 (write_1m peak memory), ln6.15 (edges-file fsync), ln6.16 (.pack without .idx), ln6.17 (pin or log git's version).

## 2026-11-22, agentfs-ln6.9 (latency slice 2, lane A): blocked twice on gaps in the page; re-cut for its tests and failpoints

**The block (second attempt, at b60dcc4).** The implementer stopped before building, on gaps in the amended page (1832478), not in the design. The page says open does not compact. But `ci/e2e-tree-recording-faults.sh:144-165` kills `agentfs init` at `tree.compact_before_checkpoint` and `tree.compact_before_drop`, which only an opening compaction reaches. A small volume is never due, so after the slice neither point fires, and `phase-1.yml`'s "kill at recording boundaries" step would turn red. The script was outside lane A's paths. The report found five more gaps:
- The three compaction failpoints had no settled fate.
- `tests/tree_dirs.rs:609` relied on a reopen compacting, and passed only because a small volume never rolls.
- Many in-process tests relied on "the first call after the builder caught up compacts" (`tests/tree_actor.rs` `fill_journal` and its users, `:1255`, `:1793-1937`; `tests/tree_stages.rs:373-394`). The only event the page offered to wait on was `AGENTFS_COMPACT_GATE`, a process-wide variable, which would park the compactor of every volume that other tests in the same binary open in parallel.
- `ci/measure-trees.py:160` counted only `*.seg` as journal bytes.
- `Relocations::dropped` and `FramePos::relocated` compared segments by `first`, and carry files all have `first` 0.

The re-cut found two more:
- `src/store.rs:548` and the in-crate tests that call `v.compact()` (`tree_actor/mod.rs`, `journal/readback.rs`) relied on a direct or opening compaction.
- The error-path tests (`NotPublished`, `NotDropped`, `Stopped`) used the failpoints being deleted.

**The re-cut (docs/target-architecture/future/latency-upkeep.md, "Slice 2"; the design is unchanged).**
1. **Owned paths.** Lane A's row now lists every file slice 2 touches. Lanes B and C have landed. The page has a table of each test and script that assumed a compaction at open or at a call, with its line at b60dcc4 and what it becomes.
2. **Failpoints.** The three are deleted from `POINTS` and `journal/mod.rs`. They never had rows in `docs/configuration.md`. The compaction gate is the one mechanism that parks, kills and fails a compaction:
   - A kill is a process killed while parked: the e2e script's new loop of five gated mounts, one per gap. The script's opening loop keeps `tree.after_ref_rebuild` alone.
   - A copy of the state area taken while parked is 2.1 and 2.14.
   - An error is a gate's `Err` at a gap, classed by `Gap::fault`: `Covered` and `Written` give `NotPublished`, `Renamed` gives `Stopped`, and `DirSynced` and `BeforeUnlink` give `NotDropped`.
3. **Events.** The gate is a per-volume `Upkeep` option, passed to `{TreeActor, Recording}::{create_with, open_with}` and `TreeStateArea::with_upkeep`. The library reads no environment variable. Only `launcher::upkeep()` reads `AGENTFS_COMPACT_GATE`, in failpoints builds, and wraps a `FifoGate`. In-process tests use a `ChannelGate`. History gains a public published event, `History::checkpointed()` and `wait_checkpointed(after)`, a generation that tests and tooling wait past. It moves History's epoch, so the metrics file is rewritten at each publish. `Stages::checkpoints` counts it, exported as `agentfs.journal.checkpoints`. Tests make a volume due without 64 MiB through `Upkeep`: `Upkeep::gated` is one frame per segment and a compaction at every built event, and a small `compact_from` or `segment_bytes` works without a gate. These are library options, not keys, so only `AGENTFS_COMPACT_GATE` goes into `docs/configuration.md`. `compact_once` now returns `None` only when B has not moved past the last checkpoint, so a gated small volume publishes at every built event.
4. **Carry files.** Segments compare by path (`Segment::same_file`). `ci/measure-trees.py` and the tests' `journal_len` and `journal_blocks` count `carry-*` files. The journal's figures move to `Durability::{len, removed, carried}`, because the compactor, not the actor, drops and carries.
5. **The partial patch** (lag.rs without the overdue stop, stages.rs with `Hold::PageImage` and `assert_unlocked`, the telemetry test, the pages.rs line) applies cleanly to b60dcc4, and the implementer applies it first. It is at `/tmp/claude-0/-home-user-agentfs/d9d7fd5b-08fa-53a9-9d39-8e3a2341a15d/scratchpad/slice2-partial.patch`. It needs `History::compaction_overdue`, its caller in `commit_point.rs` and `tree_stages.rs:433` removed in the same commit. If the file is lost, the page describes the same edits.

**Not run:** nothing was built or tested for the re-cut. It is a reading of the code at b60dcc4: the call sites of `TreeActor::open` and `Recording::open`, `failpoints.rs`, the scripts, and the tests named above.

## 2026-11-22, agentfs-ln6.9 (latency slice 2, lane A): third attempt stopped before any edit; the brief's owned paths cannot carry the slice

- **What stopped it.** The workflow brief for this attempt lists its owned paths as only `docs/target-architecture/future/latency-upkeep.md` and `.claude/errata.md`, with "touch nothing else; other lanes work concurrently on other files of the same branch". The frozen page at be2ec7e ("Owned paths are disjoint" and "Slice 2") gives lane A some forty paths across `agentfs-core`, `agentfs`, `agentfs-fuse`, `agentfs-conformance`, `ci/` and `docs/`. Its own rule is that "A lane that needs a file outside its list writes an erratum and stops". Slice 2 removes `History::compaction_overdue`, moves compaction to history, adds `Upkeep` and the compaction gate, and rewrites the tests and scripts in the page's table. None of that can be done in the two paths the brief allows.
- **Where the spec is wrong.** The page is consistent. The brief is not: its owned-paths line looks like the docs-only list of the re-cut step, carried into the implementation step. Whether another lane is now editing any of lane A's files is unknown from here. If one is, the page's statement that lane A's list is "slice 2's alone" no longer holds.
- **What is needed.** Re-launch the implementer with the page's full lane A list as its owned paths, after checking that no concurrent lane holds any of them. The partial patch (`slice2-partial.patch` in the session scratchpad) was not applied, and no code changed.

## 2026-11-22, agentfs-ln6.9 (latency slice 2, lane A): fourth attempt blocked; the switch at a built event leaves a journal recovery cannot fold

- **What stops it.** The page's switch rule ("Settled here", the switch record): at each built event, under its lock, `TreeActor::switch_built` turns every pointer whose value is a step at or below *History's built point* into its oid, with no journal record. Every later record then names that oid as the pointer's `old`. The fold (`journal/fold.rs`, `Pointers::check_targets`) compares each `old` strictly with the value the records before it give, which is still `Target::Step(s)`. When `s` is after the checkpoint the fold starts from (the usual case: History's built point runs ahead of the last published checkpoint, and with the default upkeep a small volume never publishes one), the first record after the switch fails with `PointerMoved`. Step heads (C4) cover only steps at or below a checkpoint's `built`, so they do not help.
- **Evidence (built at the wip commit, not landed).** With the page's rule, `cargo test -p agentfs-core --lib --features failpoints`: `recording::tests::{a_close_decides_as_the_oracle_does, seed_11_builds_every_save, seeds_11_and_19_build_every_save_with_a_pause_before_each_close}` fail with `PointerMoved { name: "refs/heads/root" }` when they fold the live journal; `history::repack::tests::forks_beside_the_task_keep_every_object` fails to **reopen a cleanly shut-down volume** with the same error; `tests::compaction_keeps_the_fold` and `tests::a_restart_after_a_kill_reads_each_tree_once` fail with `PointerMoved` on a queue entry, the second inside the compactor's own `fold_through` (`NotPublished`). That is a durability contradiction: a volume stops opening after a switch until a checkpoint past the switch is published, and a crash in that window loses the volume.
- **A fix that worked locally (an experiment on the wip branch, two edits, marked `EXPERIMENT`):**
  1. `switch_built` switches only steps at or below the **last published checkpoint's** `built` (`History::checkpointed().built`), not History's built point. Those are exactly the pointers the checkpoint keeps a head for, and the head's oid is `Checkpoint::pointers[p]`.
  2. The fold treats a change whose `old` is `Oid(x)` as naming `Step(s)` when the pointer's folded value is `Step(s)`, the checkpoint it started from has `step_heads[p] == s`, and `pointers[p] == x`. The step and the oid are then the same value, and the fold can check it.
  With both, the library's 987 tests pass (`cargo test -p agentfs-core --lib --features failpoints`, local, debug). The switch-record rule (History keeps `(s, at)` until a published checkpoint's `built` reaches `at`) and C3/C4 stand as written. The cost: a step is switched only after a checkpoint covers it, so heads live one compaction longer.
- **What the re-cut needs to say:** the switch rule's bound (the published checkpoint's `built`, or another that keeps every record after any surviving checkpoint foldable) and the fold's equivalence of a head's step and its oid, with a test: a volume reopened after a switch and before the next checkpoint (none is due on a small default volume, so any test that writes, settles, writes again and reopens covers it). P2.4 and P2.11 should be rechecked against it.
- **Smaller decisions made on the way (test and internal API only):**
  - `Journal::open` stays `(dir, built) -> (Journal, frames)` under `#[cfg(test)]` and the serving path calls `Journal::open_with(dir, built, segment_bytes) -> (Journal, frames, Published)`, because `recording/mod.rs` (lane C's, not lane A's) calls `Journal::open(dir, built)` in three tests. `Journal::create` does not take `segment_bytes`: it writes no frame and returns no journal.
  - `Pointers::to_targets` in `tree_actor/pointers.rs` (not in lane A's list) lost its only caller, the deleted in-lock compaction; it is deleted so `-D warnings` holds. `BuiltSnapshot::all_objects` likewise.
  - `Published` carries two private fields beyond the page's (`unlink`, the covered segments whose live frames have a copy; `carried_bytes`), because a covered segment whose live frames outweigh its dead bytes is kept, as today, and `unlink_covered` must not unlink it. `Carried` carries the same `unlink` list into `Sealed`.
  - `Durability::segments()` returns the table and then the carry files, so `CallReader::of_segments` in the builder (lane C's) still finds a carried frame; `Durability::table()` is the table alone, for the coverage rule.
  - `Durability::len()` is O(segments + held unlinked files), not O(1): each `Segment` keeps its own length.
  - `compaction_progresses_under_constant_writes_to_one_file` checks the linked files' bytes on disk, not `Durability::len()`: the old map that held the unlinked segment is dropped on the deferred-drops thread (slice 1), and no event says when.
  - `compaction_keeps_the_fold` no longer asserts that a record after the checkpoint fails to fold on the journal before it: C4 makes the opposite the rule. The fold's pointers are compared with the actor's up to a switched step.
- **State of the wip branch `wip/agentfs-ln6.9`:** the journal (checkpoint file, carry files, recovery with the C7 rules), `JournalCompaction`, the compactor thread, `History` (published event, switches, `at_built`), `Upkeep` through `TreeActor`, `Recording` and `TreeStateArea`, the gate types, `FifoGate`, the telemetry counter and the in-crate tests are written. Not written: the compactor's acceptance tests 2.1–2.14 (`history/compactor/tests.rs` is a stub), the integration tests in the table, `launcher::upkeep()` and the binary's plumbing, the conformance legs, the CI scripts, the docs, and the 2.9 figures. Not run: integration tests, clippy, conformance, power-loss, figures.

## 2026-11-22, agentfs-ln6.9 (latency slice 2, lane A): re-cut for the owner's answer "B"; the switch is a journal record

- **The owner's answer.** Asked on 2026-11-22 as general guidance, with three options (A: switch only what the last published checkpoint covers and teach the fold that a head's step and its oid are one value; B: write the switch into the journal as its own record; C: park slices 2 and 6): "B". It is recorded in `OWNER-GUIDANCE.md` under Performance. The page (`docs/target-architecture/future/latency-upkeep.md`, "Slice 2") is re-cut for it. No code was written.
- **The new rule.** The actor's pointers change only by an appended record: a step's, or a switch's. `Entry::Switch { pointers: Vec<Switched> }` with `Switched { name, step, oid }` says that pointer `name`, whose value was `Target::Step(step)`, is `Target::Oid(oid)` from that record on. `TreeActor::switch_built` appends one such record, under the lock, for every pointer whose step History has built: in `record_with`, after a step's record and before `History::appended_step`; and in `on_built`, which then asks `History::want_through` of that record. The fold replays it with `check_targets` and `apply_targets`. It has no fsync of its own: every record that depends on it comes after it, a power cut loses a suffix of the journal (D1), and a checkpoint that keeps no head for the pointer is published only once the switch record is durable (C1).
- **Deleted from the page, because they existed only for the unrecorded switch.** `History::switched`, History's `switched` map and its keep-until-published rule, `AtBuilt::switched`, the "unless switched" clause of C4 and of `compactor::checkpoint_at`, the second half of test 2.11 with P2.11a and P2.11b, and the two `EXPERIMENT` edits of `wip/agentfs-ln6.9` (the bound on the published checkpoint in `switch_built`; the fold's rewrite of an `old` from a head's oid to its step).
- **Kept, with the reason on the page.** `step_heads`: the switch record for step s always comes after the built point that built s, so a checkpoint at that built point folds the pointer as `Step(s)`, and until the switch record a save, a fork and the switch record itself name `Step(s)`. `Published::oldest_head()` and the builder's trim of its heads and of `built.log` at `checkpointed_through()`: a restart resolves a head's step from the built log, and the builder resolves a fork's `Step(s)` from its heads.
- **What the re-cut found on the way.**
  - The wip branch calls `switch_built` from `caught_up`, which runs after a step has already woken the builder. With a record to append there, a batch could start between a step and its switch record, and a test that settles between steps would see two batches where it counts one (the `tree.build_*=kill@N` tests). The page puts the append before the builder is told. Not run: the implementer confirms it on those tests.
  - A switch record that `on_built` appends has no step behind it, and the builder builds only what it is asked to. `drain` in `ci/trees-figures.py` and the power-loss driver (`ci/power-loss-trees.sh:147-148`) wait for the built point to reach the last record appended, and would never see it on a quiet mount. So `on_built` asks the builder to build through the record. The cost is one batch with no step when a volume goes quiet.
  - A switch record takes a record number, so record, object, open, fork and event numbers now depend on when the actor saw a built event. Nothing in the engine or the model's replay relies on more than their order. A test that asserts a literal number after a build has to read it from the call's result. A `grep` found no such test, but the test run is the judge.
  - A new `Entry` variant needs an arm in `history/builder.rs`, which was lane C's file and not in lane A's list. Lane C has landed, so the page gives lane A that one arm and one comment there, and the deletion of `Pointers::to_targets` in `pointers.rs` that the wip branch already made.
  - P2.1's stated failure (`Format` in `resolve_pointers`) rested on the unrecorded switch. With the switch recorded the old checkpoint needs no trimmed value in that scenario, so 2.1 now checks C3 directly while the compactor is parked, and P2.1 fails there.
  - The brief said the wip branch carries two new errata entries. It carries one, the fourth attempt's, carried over above this entry. The third attempt's entry was already on the branch (e2ad4dc).
- **The fourth attempt's smaller decisions stand** (`Journal::open_with`, the private fields of `Published` and `Carried`, `Durability::segments` and `table`, `Durability::len` not O(1), the two deleted helpers, the two changed tests). The page lists them under its re-cut responses, and where they differ from a signature it froze earlier, they are the signature.
- **New acceptance tests.** 2.15: a volume reopens after a switch and before the next checkpoint, after a clean shutdown and after a kill, with the switch appended by `on_built`, by a step, and not at all for a branch already at its own unbuilt step. 2.16: the same for a row chain.
- **Not run.** Nothing was built or run for this re-cut: it is a reading of e2ad4dc and cf66512. The docs link test (`cargo test -p agentfs-core --test docs_reference`) was not run locally; the re-cut adds no link and no heading. The implementer starts from `wip/agentfs-ln6.9`.

## 2026-11-22, agentfs-ln6.9 (latency slice 2, lane A): fifth attempt landed; where the page and its review were wrong or silent

The slice is built: compaction on history's compactor, the switch as a journal record, the overdue stop gone, state format 14. The frozen page (`docs/target-architecture/future/latency-upkeep.md`) is not in lane A's paths, so nothing below is edited into it: each point is recorded here and the code and tests carry it.

- **The review's six amendments (brief of the fifth attempt).**
  1. *A step keeps its own time.* Applied. `switch_built` restores `last_at_ns` after its append (`journal/commit_point.rs`). `compactor/tests.rs::a_step_with_a_switch_behind_it_keeps_its_own_time` runs a `set_times(Now)` step and a landing take, each with a switch record right behind it, and compares every time with a reopened copy's and with the same volume's once built. Probe P2.15c (let the append move `last_at_ns`) fails it: the file's times differ by the gap between the two records.
  2. *Every switch record is asked of the builder.* Applied. `record_with` passes the record `switch_built` returns to `History::want_through` after `appended_step`; `on_built` does the same; `start_history` asks through the journal's last frame when it is a switch record. `a_batch_one_record_short_of_a_switch_record_is_followed` holds a batch that has read through a step before that step's switch record is appended and shows the volume still comes to rest built through its last record; the copies of 2.15 whose last record is an unbuilt switch record reach it with no call. The marker of the fast path is unset at open, so the first act after a reopen makes a full pass. The test needed one test-only hook the page does not list, `History::on_step_recorded`, which runs between a step's record and its switch record: the `batch_read` hook alone cannot place a batch's read inside one `record_with`.
  3. *Counted failpoints.* Applied, with the counts measured, not read. A mount of a staged volume whose root client is attached appends 2 records before its first client call (the rebind, then the switch record), makes 3 journal fsyncs (the rebind's own and the builder's before each of its two batches; the review's count of 2 was wrong), records 1 step and runs 2 batches. `ci/e2e-tree-recording-faults.sh` waits for the mount to be at rest before it starts the client (a metrics file written after the wait began, with `built_through >= appended` and the new `history.pointers_at_steps` at zero), uses `kill@4` for the append and fsync points, `kill@2` for `tree.before_step` and `kill@3` for the builder's points, and now fails unless the receipt holds exactly the calls that returned before the call its count names. `tree_store.rs`'s `tree.wal_after_payload=kill@3` holds the builder with `tree.build_before_batch=error@*`, so its third append is the drop's whatever the timing. `tree_clients.rs::three_clients_killed_all_end_at_boot` runs `progress` and `settle` before it counts one record. `ci/e2e-tree-enospc.sh`'s `error@3` now meets the builder's fsync before the mount's first switch record: its outcome is unchanged (every call answers EIO) and only its comment changed. Every other counted spec under `crates/*/tests` hits the record, fsync or batch its comment names: each counts steps (a switch record is no step), or a first hit, or records with no step between them, or batches of steps that are settled one by one. Files changed outside lane A's list for this: `crates/agentfs-core/tests/{tree_store.rs,tree_clients.rs,tree_batch_alloc.rs}`, `ci/e2e-tree-enospc.sh`, `.github/workflows/phase-1.yml`.
  4. *Why the switch is still needed with heads.* Not decided here: it is the owner's. The comparison, for the question: a kept head costs one entry in `Checkpoint::step_heads` and pins `built.log` and the builder's heads from the oldest head on. Never switching would delete `Entry::Switch`, `switch_built`, the batch with no step when a volume goes quiet, the timing-dependent record numbers and tests 2.11, 2.15 and 2.16; a restart would read a head's oid from `Checkpoint::pointers`, and the builder would keep heads for the published checkpoint's head steps. Its costs: `step_heads` grows to about one entry per pointer for the life of the volume, the builder's heads map is bounded by pointers and not by the built point, and that nothing else needs `built.log` trimmed by heads is unproven. The measured cost of the switch as built, on a mount: one append, one journal fsync and one builder batch for each built point that leaves a pointer at a built step. B stands as built.
  5. *The pass is bounded.* Applied by the first option: the actor keeps the pointers that are at a step (`TreeActor::at_steps`, moved only by `apply_pointers`, which applies a step's or a switch's changes), and `switch_built` passes over those alone, O(1) when the built point has not moved. No hold of its own is needed.
  6. *The fork exception and queue entries.* Applied: `a_forks_branch_is_switched_once_the_fork_is_built`, `a_forks_branch_is_the_only_head_and_its_first_save_folds`, `a_switched_queue_entry_is_deleted_by_its_landing`. The required outcome of P2.15d is that the first switch record names the root alone; the probe (take the oid from another pointer at the same step) fails there. With the builder held, the fork step's own `record_with` appends that first switch record, not `on_built`, which then appends nothing.
- **Where the page was wrong.**
  - *2.1, 2.2, 2.4, 2.14, 2.15: "copy the state area" is not a kill unless the state area holds still.* Two tasks go on beside an idle builder: the drops thread unlinks base files, and the repack task writes temporary packs and expires superseded ones. A copy taken then can fail or miss a file. The tests wait for the builder to be idle (`History::wait_builder_idle`, test-only) and skip a file that vanishes from `bases/` or `history.git/objects/pack/` while it is copied, which is what a kill may leave there and what open repairs.
  - *2.4, "reopen at each of the two checkpoints".* A journal with the old checkpoint and the new one's trimmed built log is no state a kill can leave. The test parks a compaction at `Written` (its checkpoint written, not renamed), runs the records after B, and copies there for the old checkpoint; it copies again after the compaction for the new one.
  - *2.6, "no segment left" as a power-loss mark.* The newest segment is never covered, so a prefix replay always leaves it; that state is reachable only by losing a segment's directory entry, which the unit test `a_checkpoint_with_no_segment_after_it_opens` covers. The driver's marks are `CP 1` to `CP 5` (parked at `written`, `renamed`, `dir_synced`, `before_unlink`, and after the unlink) and `CN 1` (a write appended to a new segment and not fsynced). The power-loss job built a binary without failpoints, which ignores `AGENTFS_COMPACT_GATE`: `phase-1.yml` now builds `--features failpoints`, and the driver refuses any other build.
  - *2.10, P2.10a and P2.10b name tests that hold no lock.* `tree_pages.rs` and the compactor's unit tests drive a bare `TreeActor`, so the guard never fires in them, and P2.10b survived `tree_pages.rs`. `tree_stages.rs::a_page_fill_under_the_lock_is_a_page_image_hold` is new and fails under P2.10b ("JournalRead under the actor's lock in a other hold"). P2.10a was not planted: no test runs `relocate_if_moved` under the lock with frames carried.
  - *"Tests wait on History's published event."* The publish comes before the unlink, so a test that reads the journal directory after `wait_checkpointed` races the unlink (`fill_journal`, `compaction_progresses_under_constant_writes_to_one_file`). New: `History::compaction_ended`, `wait_compacted(generation)` and `compactor_through()`, which also move the epoch so the metrics file is rewritten after the unlink. `ci/trees-figures.py` waits on `history.compactor_through` in place of the nudge and of "when the journal reached 64 MiB".
  - *The due rule read `Durability::len()`.* That figure counts unlinked files a reader still holds, so the length after a compaction was inflated by the segments it had just unlinked, the next due length doubled, and `a_held_open_pins_only_its_own_bytes` hung waiting for a compaction that was never due. The due rule now reads the linked files (`Durability::linked()`); `Metrics::journal_bytes` still reads `len()`. This is a change of when compaction runs, decided here against the page's wording.
  - *`fill_journal`'s users "change no further".* With the checkpoint in a file and the newest segment never covered, a filled journal leaves up to one segment, not under 1 MiB. `a_log_held_open_does_not_pin_other_saves` and `carried_frames_free_their_old_segments` assert one segment more.
  - *`Relocations::dropped` moved the generation at every unlink*, so `relocate_if_moved` walked under the lock after a compaction that carried nothing. It now moves only when a carried frame's entry goes.
  - *`tree_batch_alloc.rs` (not listed).* Its window of 50 creates ran in 2 builder batches or, when the builder's first batch ended before the creates were appended, 3, and a third batch costs 0.3 to 1.2 MB at 20,000 files. At the branch tip it was 2 in 21 runs of 21; with this slice 3 in 5 runs of 31, one over the bound. The cost per batch is unchanged by the slice (26.3 MB and 38.5 MB for 100 one-step batches, the same at 0eecabc). The test now settles after each step, so the batches are the same at any timing, and its bound is 1.75.
  - *The library conformance leg uses its own gate type*, `compact_turns::Turns`, not a `ChannelGate`: it parks the compactor at each gap in turn while one whole replayed call runs, without a thread to answer it. The mounted leg answers the FIFO gate from a thread that asks the same `Turns`.
- **Mutation probes run** (planted, run, reverted): P2.1 (the check at `Written` fails, as the page says), P2.2, P2.3, P2.4, P2.7 (the library replay reports 13 traces and their calls), P2.10b, P2.11, P2.12, P2.13, P2.15a, P2.15b, P2.15c, P2.15d, P2.16. P2.15a fails earlier than the page says (no switch record is found); P2.15b fails in the first run, not the third (the second save's own unbuilt step is switched).
- **Not built.** The `held_open` figures workload of 2.9: it needs a row in `ci/trees-budget.json` and in the tracer's list of figure names, neither in lane A's paths.
- **Not run.** 2.9's release figures (6.5 GB free on the container's disk; `close_1g` needs about 5 GB beside a release build). `ci/power-loss-trees.sh` (no kernel modules in the container). Probes P2.5, P2.6, P2.8, P2.10a and P2.14.

## 2026-11-22, PR #41 at a819159: `ci/e2e-tree-enospc.sh` red, a counted failpoint the slice 2 audit missed

**The red.** phase-1's "journal and state exhaustion" job: `rename_while_full tree.build_after_built_log` waits 60 s for `tree CopyStale` and never sees it (`ci/e2e-tree-enospc.sh:195-196`).

**Mechanism (read from the code, not run).** The case mounts with `tree.build_after_built_log=sleep(30000)@2`, assuming the mount's rebind is batch 1 and the rename's batch is hit 2. Since slice 2 each built event that leaves a pointer at a built step appends a switch record and asks the builder for it (`on_built`, `switch_built`, `History::want_through`), and a switch-only batch is a full batch that passes the failpoint. So hit 2 is the switch record's batch, which has no ref to move; the woken copy succeeds on the full disk and `CopyStale` is never logged. `ci/e2e-tree-recording-faults.sh` was re-counted for this (`kill@3`, an `at_rest` wait); this script's two `sleep(30000)@2` specs were not. It is a race, not a fixed miscount: the count depends on whether the rename's record lands before the worker's `on_built`, and the setup mount is killed without waiting to be at rest.

**Fix to make in the end fix round (test only):**
1. Move `at_rest` from `ci/e2e-tree-recording-faults.sh` into `ci/tree-git-test-lib.sh`, taking the metrics file path.
2. In `rename_while_full`: mount the setup and the faulted mount with `--metrics-output`; `at_rest` before the setup mount's kill and after the root is attached; take the hit count as a parameter (`tree.before_step` stays `@2`, `tree.build_after_built_log` becomes `@3`); rewrite the counts comment.
3. Assert before the client starts that the log does not yet hold the "triggered" line, so a miscount fails at once with a clear message.

**Not proven; settle with one run before closing:** why the rename's own batch logged neither `CopyStale` nor `HistoryBuildFailed` while the disk was still full. Either it found room (repack expiry or the drops thread freed pages), or the builder stalled until unmount, which would be a product bug. After the 60 s loop and before removing the filler, print whether `destination` is in git, the state area's `du`, and `history.built_through` against `appended`.

**Class.** A counted hit on a builder failpoint depends on how many no-step batches a mount makes. The page (`latency-upkeep.md`, "keep their batches") left the re-count to a hand audit. A failpoint qualifier that counts only batches holding a step would remove the class; argue it as its own change.

## 2026-11-22, PR #41 at 69d2434: `e2e (FUSE mount)` red, `a_drain_ends_a_crashed_hosts_clients` fails only sometimes

**The red.** `crates/agentfs/tests/launcher_host.rs:186`: "the host wrote no hello", from the second host's `hello()` at line 800, started right after `host.kill()` (798). The same test passed on the same sha in the push run, so it is a race, and by the repo's rule a broken test or a product bug to find, never a rerun.

**Not determined.** `hello()` panics before the test reads the second host's exit status or stderr, so the log cannot say why the host exited.

**Suspicion (read, not run): a stale test assumption.** "Once `wait()` returns for the killed host, the state lock is free." Tree git children keep the lease descriptor through exec (`tree_git.rs`, `attach_lease_signal`), so a `git fast-import` child of a builder batch holds the lock until it exits. The second host tries the lock once, gets `Busy` and exits 75 before its hello. Slice 2 widens the window: `on_built` appends a switch record and asks the builder, so a batch follows the open's own batch right when the test kills. `a_restarted_host_rebinds_and_stacks` (kill at line 659) has the same race. **Not excluded:** a slice 2 product bug where the second open refuses the killed state (a fold error on a switch record, exit 1).

**For the end fix round:**
1. Make `Host::hello` on EOF wait for the child and panic with its exit status and stderr, so a failing run says which case it is.
2. If 75: the test must wait on the event, not a timer: the killed host's process group (its git children) has exited, or the documented launcher contract of starting again on 75. Use it at lines 663 and 799, and add a case that holds the lease in a child across the kill and asserts the first start exits 75 and a later one writes its hello.
3. If exit 1: a product bug. Reproduce in `crates/agentfs-core/tests/tree_actor.rs` by copying the state area between a step's built-log entry and its switch-only batch, then reopening.

## 2026-11-22, latency slice 6 (lane A): landed; where the page was wrong or silent

The slice is built: FLUSH answers at once (`Recording::flush`), a client's end no longer waits for the disk, a move into the tree makes every record durable before it unlinks the area's file, and the power-loss oracle judges a prefix. None of the points below changed product behaviour from the page, so none stopped the slice. The frozen page is not edited.

- **Owned paths.** The brief says "see Slice 6 page owned paths"; the page gives slice 6 no list of its own ("lane A's list is slice 2's alone"). Lane A's list was used. Every file touched is in it.
- **D2, out of the tree: "`place` fsyncs the area file and its directory ... Today it does neither" is wrong.** `AreaWrite::place` (`crates/agentfs-core/src/tree_actor/untracked.rs`) already writes under `.staging`, `sync_all`s the file, renames it into place and fsyncs the parent directory, all before the view records the tracked name's unlink. Nothing was changed there (the file is also outside lane A's paths); `tree_view.rs` has a comment at the call and `docs/trees.md` states it.
- **"§18 row 1058"** is line 1064 of `git-model.md` today (the validation table's journal row). Rows 142 and 179 were where the page says. Row 142 was split: `close` has a row of its own.
- **The model.** Power loss before an fsync is not modelled (`MODEL.md`: "the model's crash keeps every journaled call"), so `VERIFYING.md` "The procedure" step 1 has no rule to encode: the change is to git-model.md's three rows alone. `MODEL.md` and `VERIFYING.md` are unchanged. The owner's answer is OWNER-GUIDANCE.md's entry of 2026-11-21 ("A"); the rules ledger is outside lane A's paths and has no row for close durability.
- **6.3, P6.3.** `call_path.rs::tests::fsync_still_waits` cannot see a mutation made in `tree_view.rs` (routing the view's FSYNC through `session.flush`): only the power-loss F, SF, RN, HC and CP marks catch it, in CI. Not planted.
- **6.4, "the recovered journal is a prefix of the appended records".** No tool lists a state area's journal records, so the oracle judges what a program can see: the driver logs every call that changes the volume before it makes it (`$work/ops`, off the device), and the recovered files must be those of that run cut after some call, a write perhaps partway, with every call before the last returned fsync or FSYNCDIR inside the cut. A close's save record is not a call of its own in that log: recovery saves an open from its calls, so a lost close record changes no file. `ci/power-loss-trees-check.sh self-test` runs the oracle on planted images with no device; the driver runs it first. P6.4 (skip the prefix check) fails the self-test: "a later call without an earlier one: not refused".
- **6.5's unit test needs a test-only hook.** `agentfs-fuse` cannot reach `Durability::hold_fsync_with`, and a thread-scoped failpoint on the fsync races the builder's own fsync, which can cover the record first. `View::in_durable` (`#[cfg(test)]`) runs in place of the wait: the test sees the tree's entry recorded and the area's file still there, fails the wait, and finds the file kept. P6.5 (unlink before the wait) fails it: `[(true, false)]`.
- **The held-file leg of `power-loss-trees.sh` raced the compactor.** The compactor compacts at a built event once the journal is 64 MiB long and then not before it has doubled. A built event that arrives while `big` is being written (a fork sync of an earlier save, still in flight) compacts with `big` open, frees nothing, and the leg then waits 600 s for a carry that never comes. It passed in CI at a819159 by timing; a local run with the new legs before it hung there. The driver now waits, before `big`'s first byte, for the fork's sync of `big`'s create and for the mount to be at rest (the faults script's rule: a fresh metrics file, built through every record, no pointer at a step).
- **`Recording::flush` does not look its open up.** It takes neither the actor's lock nor the fsync, as the page says, so it cannot answer `NotFound`; the view answers `EBADF` for a handle it does not hold before it calls.
- **A client's end has no test of its own** for "waits for no fsync": `hold_fsync_with` is private to `tree_actor`, and `session.rs` is outside it. The change is one call removed.
- **The first commit of the slice has a `wip(trees)` subject**: pushed to the wip branch before the rule against amending could be weighed against the subject. The later commits are conventional.
- **Not run.** `ci/power-loss-trees.sh` (no `dm_log_writes` in the container): its two workloads and its checker were run on a local mount with `dmsetup` faked, each mark judged on a live copy of the state area, which is a kill's image, not a power cut's. `cargo test --workspace` in one invocation (run as five invocations that cover every package). The mdBook build.

## 2026-11-22, PR #41 at 26ddacf: both phase-1 power-loss jobs red, in the driver's own wait (no power cut judged)

**The red.** `ci/power-loss-trees.sh:238-240`, the held-file leg's wait before the `HC 1` mark, times out after 600 s on ext4 and xfs alike: "a compaction carrying h's frames: not reached", with `checkpoints 1`, `journal_carried_bytes 0`, `journal_dropped 0`, everything built and the compactor idle. No image was replayed and the oracle judged nothing, so this is not a durability failure.

**Mechanism (read from the code and the metrics, not run).** The one compaction ran while `big` was still open, so nothing was covered and it carried and dropped nothing. It still set `len_after_last` (`history/compactor.rs`), and the next compaction is due at `max(64 MiB, 2 x len_after_last)`. The journal ends at about 151 MB, under twice the 74 MiB it held then, so no later compaction is ever due. The script's comment "the only built event from big's first byte to its save is that save's" is stale: every batch end is a built event to the compactor, batches with no step included. Which no-step batch fired is not proven; the likeliest is the take worker asking history to build through the last appended record while fork syncs of the new no-fsync legs are still queued (`at_rest` does not check `queued`).

**For the end fix round (test only):**
1. `at_rest` also requires `queued == 0`.
2. Do not depend on a single compaction: after `close(big)`, while nothing is carried, write and close further logged files, each at least the journal's current length, bounded to about 3; `big` is saved by then, so the next due compaction must carry `h`.
3. On failure print `history.built_through`, `queued`, `stages.compaction.count` at each metrics rename while `big` is open, and `mount.log`.
Proof: both power-loss jobs reach `ack-hc-1` and the oracle judges every cut.

**A product question this exposes (do not fix in the fix round; report it):** a compaction that frees nothing still doubles the threshold for the next one, so dead bytes can sit in the journal until it doubles again. Counting only what a compaction left behind after it freed something would retry a fruitless fold at every built event while a file is held open, so the rule needs a design answer, not a patch.

## 2026-11-22, PR #41 at a6a8174: `e2e (phase 1 tree views)` fails only sometimes, a race in `ci/trees-untracked.py`

**The red.** `ci/trees-untracked.py:310`, `early_fork`: `assert name not in names` fails for `repo`. The same job passed on the same sha in the other ci run.

**Mechanism (read, not run).** `early_fork` asserts the fork's listing lacks `repo`, but `repo` is a recorded directory (`docs/trees.md`: "`git init repo` in a view records the directory `repo`"), and every step that saves the root enqueues a sync in each fork, taken at the next built event. "The fork never shows `repo`" holds only while the script outruns that sync. The failing run was a slow one (3.6 s where the passing run took 0.26 s), and its fork was mid-sync. Nothing a user would see is wrong. Whether slices 2 or 6 shifted the odds is not established.

**For the end fix round (test only):** in `early_fork`, drop `repo` from the absent list; first wait (as `delivered()` does in `ci/e2e-trees.sh`) until the fork shows the root's last tracked names (`draft.txt`, `t2`, `repo`), then assert `repo` is an empty directory, `repo/.git` and `repo/x` do not exist in the fork, and the untracked names and `u/x.log` are absent. Update the module docstring's `early-fork` line. Today the absence checks pass trivially when the sync has not arrived.

## 2026-11-22, PR #41 at d38694e: `a_log_held_open_does_not_pin_other_saves` fails only sometimes; a product bug in the compactor's due rule

**The red.** `crates/agentfs-core/tests/tree_actor.rs:1330`: "journal holds 69207626 bytes" (ci `test (default)`).

**Mechanism (read, not run).** A compaction that runs at a built event while a large file is still open covers nothing it can drop (`journal/compaction.rs`, `carry` keeps a covered segment whose live bytes outweigh its dead ones), returns `Ok(Some)`, and sets `len_after_last` to the journal's whole length (`history/compactor.rs`). The next compaction is due at twice that, so the save that kills those bytes is not due. The test's two waits accept that no-op checkpoint. The power-loss driver's timeout (entry above) is the same mechanism.

**A user sees it.** With the default `Upkeep`, any built event (another file's step, a fork sync, a switch-only batch) while a large open is in flight and the journal is at least 64 MiB triggers it. Up to the journal's length at that moment stays as dead bytes until the journal doubles and a built event follows; on an idle volume, indefinitely. There is no absolute bound, which contradicts `docs/trees.md` ("under about twice the live opens' bytes plus a segment").

**The rule to build (main loop's decision; build this, do not redesign):** subtract the bytes of opens that have ended from the baseline.
- `journal/mod.rs`: `Change::saved_open() -> Option<u64>` beside `build_cost`.
- `history/built.rs`: `appended_step` takes that handle (the actor's call and the replay's); state keeps `(step, handle)` pairs; `History::take_ended(through) -> Vec<u64>` gives the saves built at or below `through`.
- `history/compactor.rs`: `compact_once` also returns each open's call-frame bytes from the fold's opens. `run` keeps `pinned: BTreeMap<u64, u64>` and `released: u64`, and is due at `len >= compact_at(from, len_after_last - released)`. Each `Ok(Some)` replaces `pinned` and zeroes `released`.
- Check, and say in the report: whether every way an open ends is a `Save` step; held page writes are keyed by file, not by open, and are not covered by this rule.
- `docs/trees.md`: the bound's sentence, to match.

**Tests.** A unit test of the due formula with `released`. New, deterministic, fails today: `segment_bytes: 1` and a small `compact_from`; open `big`, write, make an unrelated step, wait for the compactor (a checkpoint is published, nothing dropped); close `big`, settle, wait for the compactor: the chunk's segment is gone.

**Test-only, needed regardless.** `fill_journal`'s generation wait accepts any checkpoint (same pattern at `tree_actor.rs:1375-1387` and `1798-1812`). Add `History::wait_compactor(through)`, a blocking form of `compactor_through` (`compaction_ended` already notifies); in `fill_journal` call it after the create and before the first chunk, and in place of the two waits at 1313-1314. Do not use `trees::compacted_through(settled)` there: a compaction at the save, one record short of a trailing switch record, would hang it. With this rule in, the power-loss driver's fix (entry above) may reduce to its `queued == 0` wait and its diagnostics; keep whichever the run proves.

## 2026-11-22, PR #41 at 3527853: `repack::tests::a_snapshot_reads_through_a_roll_up` timed out once in ci `test (all)`; mechanism NOT found yet

**The red.** `crates/agentfs-core/src/tree_actor/history/repack.rs:905`, `guarded` inside `wait_written` (called at 1244) hit its `RecvTimeoutError`; 1024 passed, 1 failed. The same test passed on earlier runs of the same code, so it fails only sometimes.

**Not investigated.** Nothing is known beyond the log line. Candidates to test, none shown: the switch record's extra no-step builder batch changes which built event the repack task's roll-up follows, so the "written" event the test waits for never fires in one ordering; or the test's guard is a wall-clock bound that a loaded runner misses (then the guard is a timer standing in for an event and the wait itself is wrong).

**For the CI fix:** find the mechanism first (the orderings of built events, `wait_batch_end` and the repack task's steps in this test, with the switch record in), reproduce it deterministically, then fix the product or the test accordingly. Do not lengthen the guard as the fix.
