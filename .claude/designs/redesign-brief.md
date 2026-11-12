# Brief: design round 2 — step 5b redesigned, a "one engine" slice planned, step 5 re-checked

Repo /home/user/agentfs, main at 22d55f9. Read on main: docs/target-architecture/future/module-cut.md, root-branch-name.md,
docs/trees.md, docs/target-architecture/git-model.md (baseline page: numbering kept), rules-ledger.md, OWNER-GUIDANCE.md,
AGENTS.md (principles; "Design docs"), docs/architecture.md, docs/deployment.md, docs/csi.md (or the CSI docs), docs/supported-configurations.md,
and the code. Round 1's designs, reviews and critic: branch wip/module-cut-designs, .claude/designs/ (also in
/tmp/claude-0/-home-user-agentfs/41ac0cfe-83c8-57b0-9f09-59a6e4c29f38/scratchpad/next/). The owner's words from this round:
/tmp/claude-0/-home-user-agentfs/41ac0cfe-83c8-57b0-9f09-59a6e4c29f38/scratchpad/cut/owner-notes.md (the 2026-11-12 entries).

## What the owner said this round (guidance, dated 2026-11-12; quote, never paraphrase as a rule)
- One engine: "the goal is to have one engine, ostensibly the new tree view. Until its ready, nobody is actually going to use it."
  Asked whether to plan moving the sidecar and CSI onto tree views and retiring the recording engine: "yes. ... this is another
  oversight in the original plan."
- Identity vs name: "It seems more important for an _instance_ of agentfs to know that it is "the root" than to have a specially named
  branch stand-in as a root process identifier." On the split (history records tree identity; the root branch's name is configuration
  at the edge, the ref agentfs publishes): "i agree with that split -- i assumed that it already worked that way. But the trailer should
  be addressed so that it records meaningful information".
- `trees init` is a trial CLI: users "should never have to do this manually" in production; volumes are created by the sidecar/CSI from
  configuration.
- Earlier (2026-11-11): "refs/heads/main should not be special or hardcoded. Consumers will configure the name of the root branch."
- Goal-oriented rubric (2026-11-12): every plan is checked against its stated end goals: each goal has a step that delivers it.

## Rules for every designer here
- Name your layer (system for the one-engine slice; interfaces/implementation for 5b and 5). Requirements vs guidance vs proposed,
  kept apart and tagged. Domain changes are owner questions. At most 3 owner questions per design, each with background, options
  (recommended first, gives/costs/what the owner sees) and its kind (general guidance / absolute requirement). Plain words; a
  new-terms table for any new noun. Name the patterns you use with their canonical source.
- Goal coverage: list the end goals your design serves (one engine; recording is the baseline; invisible to the workload; git idioms;
  any remote; history is the product) and which step delivers each.
- Read only. No model run, no docker, no cargo build (a model gate is running on this machine; compiles skew it). git, grep, reading are fine.
  Commit nothing; edit nothing in /home/user/agentfs.
