# Recording without a mount on trees

**Proposed, not built.** This page proposes recording, committing and pushing a volume with no mount on the tree
views. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

The recording engine records without a mount ([configuration](../../configuration.md#flags-and-environment)):

- `agentfs run` runs the commit and push loops in the foreground with no mount: it serves `push` on the control
  socket until a signal, makes a final commit and pushes once more. Several end-to-end scripts drive a volume this
  way (`ci/e2e-remotes.sh`, `ci/e2e-ssh.sh`, `ci/e2e-telemetry.sh`, `ci/e2e-alerts.sh`).
- `agentfs record` and `flush` record named paths of the backing tree directly, as the Hermes migration Job's
  initial import does ([deployment](../../deployment.md#10-hermes)).

On the tree views every change enters through a view: a write made beside the mount is not seen. M7 rewrites the
migration Job to copy files in through the mount and repoints the scripts; `run`, `record` and `flush` are deleted
with the recording engine at M9 ([one engine](one-engine.md), "Deferred extras").

## What it proposes

A launcher that holds the history service with no view host, so the push loop and the control socket run with no
mount, and an import that saves a directory's files into the root as one save. Its gate decides whether either is
needed once every launcher writes through a view.
