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
