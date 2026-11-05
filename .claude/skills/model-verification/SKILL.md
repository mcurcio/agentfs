---
name: model-verification
description: Use when changing agentfs semantics (how saves, syncs, landings, collisions, locks, transactions or history settings behave), editing crates/agentfs-model (its rules, checks, scenarios or random runs), editing docs/target-architecture/git-model.md or MODEL.md, or reviewing a pull request that does any of these.
---

# Verifying a change to agentfs's rules

The procedure, the commands and the reviewer checklist live in one place:
`crates/agentfs-model/VERIFYING.md`. Read it before you change a rule and follow it as written; this skill
does not restate it.

| When | Section of VERIFYING.md |
|---|---|
| Before editing | "The procedure", "Adding a rule, a scenario or an invariant"; the rule must be `owner` or `approved` in `docs/target-architecture/rules-ledger.md` |
| Fixing a review finding or a failing seed | "Fix passes": delete, or correct toward a ledger rule; anything else is a question for the owner |
| Running anything | "Commands" (the container line, the `-e` seed variables) |
| A test fails | "Reading a failure", "What the ignored tests are expected to show" |
| Planting bugs | "Planting bugs (mutation probes)" |
| Reviewing a pull request | "Reviewer checklist for a design pull request" |

One trap it does not spell out: a long seed run piped through `tee` or `tail` reports the pipe's exit
status, not the tests'. Run it unpiped with its output redirected to a file, or read `${PIPESTATUS[0]}`.
