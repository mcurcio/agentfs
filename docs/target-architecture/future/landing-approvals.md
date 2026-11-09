# Landing approvals and timing on trees

**Proposed, not built.** This page proposes approvals and timing for landings in the tree queues. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

Every landing is accepted when its queue takes it. Nothing waits for approval, and nothing waits for a quiet moment.

## What it proposes

- **Approval.** A landing may wait for an acceptance rule, as the existing patch queue does with `cleanMerge`,
  `approval` and `command`. The rule is configurable with a sensible default, because a clean merge is not always an
  acceptable merge.
- **Timing.** A fork's saves land continuously. An integrator's patches wait until a chosen moment (for example,
  until the workload detaches), and the integrator can ask to apply them now.

The queue's order, the referee and collisions stay as the [git model](../git-model.md) has them. An approval decides
whether a change is offered to the referee; it never changes how the referee decides.
