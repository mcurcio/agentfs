# Fork delivery and unlinked forks on trees

**Proposed, not built.** This page proposes handing a fork's change to a receiver, and forks that never land, on the
tree views. The commit that lands this slice deletes this page and moves the behaviour into the library or operator
docs.

## What stands until then

The recording engine ships both ([forks](../../forks.md), [supported configurations](../../supported-configurations.md)
examples 5 and 8):

- **`fork deliver`.** A finished linked fork's change is handed to a receiver, run by the receiver with its own
  credentials: `--to inbox` queues it as a patch, `branch` pushes a pull request's head branch, `command` and
  `webhook` hand the payload to a program.
- **Unlinked forks.** `fork create --link unlinked`, or a CSI fork class with `link: unlinked`: a fork that never
  delivers to its source and pushes only to remotes of its own, or none.

On the tree views a fork lands in its source as it saves, unless its setting needs approval
([landing approvals](landing-approvals.md)). Neither capability exists there. Each goes dark when its launcher moves
(M7 for the binary, M8 for the CSI driver) and is deleted with the recording engine at M9
([one engine](one-engine.md), "Deferred extras"). Until this slice lands, M8 refuses a claim on an unlinked class.

## What it proposes

- **Delivery** as a fork setting beside approval: a fork whose setting says "deliver" does not land as it saves;
  when it ends, its change is handed to a receiver by the same four routes. The receiver's acceptance rule gates it,
  as the patch queue's does.
- **An unlinked fork** as a fork whose setting says "never land": its saves stay on its own branch, published only
  to its own remotes.

Its gate decides whether delivery is a landing that waits (one mechanism with approval) or a separate hand-off, and
what a delivered change's commits record about the fork.
