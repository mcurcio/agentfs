# Landing patches while a workload runs

**Proposed, not built.** This page proposes landing integrator patches into a volume while a workload has it mounted. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

Patches land only at mount, with nothing attached. An apply while a workload is attached judges the queue and
answers that the patches land at the next mount. Remote changes never enter a running workload.

## What it proposes

A later gate that lets a patch land while the workload runs. The risk is the race between a landing and the
workload's writes across pods.

## What has to be settled first

- Which referee decides a patch against a file a program has open.
- How every pod sees the landed change at once.
- Whether the same rules serve a fork's save and an integrator's patch: [integrator patches](integrator-patches.md).
