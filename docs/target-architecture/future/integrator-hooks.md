# Checkpoints and patch notices on trees

**Proposed, not built.** This page proposes the integrator's checkpoint and the patch queue's notices on the tree
views. The commit that lands this slice deletes this page and moves the behaviour into the library or operator
docs.

## What stands until then

The recording engine ships both ([supported configurations](../../supported-configurations.md), "Integration point"
and "Knobs"; [patches](../../patches.md)):

- **Checkpoint and the change line.** `agentfs checkpoint [-m TEXT]`, or the control socket's `checkpoint`: an
  integrator says a batch of work is done, which cuts one commit on `refs/agentfs/<volume>/change-line`. A remote
  with `agentfsPush = changeLine` gets only that line.
- **Patch notices.** The hook `$GIT_DIR/hooks/patch-notify` and the webhook `agentfs.patchNotifyUrl` get one notice
  each time the queue's verdicts change; a failure raises `NotifyFailed` and is sent again.

The tree views have neither. [Integrator patches](integrator-patches.md) (milestone M6) builds the queue, `restore`
and `revert`, not these. They go dark when the `agentfs` binary moves (M7) and are deleted at M9
([one engine](one-engine.md), "Deferred extras").

## What it proposes

- **Checkpoint** as a mark on the root's history: one commit per change intent, on a line the publisher can push in
  place of every save.
- **Notices** from the tree queue's verdicts, through the same hook and webhook, carried by the telemetry domain's
  events rather than a second path.

Its gate decides whether a checkpoint's line is a ref of its own or a publisher's view of the root, and whether a
notice is one more consumer of the queue's events.
