# Recording without a mount on trees

**Proposed, not built.** Not built on tree volumes; the recording engine still offers it on the state areas the CSI
driver creates, until the driver moves. This page proposes recording, committing and pushing a volume with no mount on the tree
views. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

The recording engine records without a mount ([configuration](../../configuration.md#flags-and-environment)):

- `agentfs run` runs the commit and push loops in the foreground with no mount: it serves `push` on the control
  socket until a signal, makes a final commit and pushes once more. Its Rust tests create their state areas through
  the library.
- `agentfs record` and `flush` record named paths of the backing tree directly.

On the tree views every change enters through a view: a write made beside the mount is not seen. On a tree volume
`run`, `record` and `flush` refuse, as not built, and the Hermes migration Job copies files in through
`mount -- COMMAND` ([deployment](../../deployment.md#10-hermes)). They are deleted with the recording engine at M9 ([one engine](one-engine.md), "Deferred extras").

## What it proposes

A launcher that holds the history service with no view host, so the push loop and the control socket run with no
mount, and an import that saves a directory's files into the root as one save. Its gate decides whether either is
needed once every launcher writes through a view.
