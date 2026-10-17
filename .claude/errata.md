# Errata (append-only)

Spec, design or brief contradictions found while cutting or implementing. Newest last. Each entry:
date, who, where, what, and what was done.

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
