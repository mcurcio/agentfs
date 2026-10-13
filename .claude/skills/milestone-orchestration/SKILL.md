---
name: milestone-orchestration
description: Build large features as a sequence of milestones with a deterministic workflow (architect, implementer, adversarial reviewer, bounded fix round, CI as referee, completeness critic). Use when the user asks to build out many features, finish a roadmap, close review gaps, or resume a paused build. Covers token-efficient orchestration, resume after interruptions, and clean handoffs.
---

# Milestone orchestration

agentfs was built this way, from M4 to M13 and then F1 to F5. That was about 140 commits, taking the tests from 182 to 825, with real CI coverage and little rework. The script is [milestone-loop.js](milestone-loop.js). Its arguments are a `base` commit SHA, a list of `milestones` (`{id, title, brief}`) and the `goals` the final critic judges. A milestone brief is a paragraph or two of plain text: see "Writing milestone briefs" below.

A workflow runs only when the user asks for one explicitly ("run the workflow", "use a workflow"). Without that, describe the plan and its rough cost, and ask.

## Running it

```js
Workflow({ script: <contents of milestone-loop.js>, args: { base, goals, milestones } })
```

Pass `args` as a JSON object, not a string. The run notifies you when it completes, so don't poll it. Read the result with `jq` on the output file, field by field. Then verify for yourself:
- the working tree is clean;
- the local head equals `origin/main`;
- every workflow run for that head is green.

## Why it is token-efficient

1. **Thin orchestrator.** The main conversation never reads code or diffs. It holds the briefs and compact structured results. The heavy reading happens in subagents, and their context is thrown away when they finish.
2. **Deterministic control flow.** The loop and its decisions are plain JavaScript, so no model spends tokens deciding what to do next. That covers plan, implement, review, fix if blocker/major, verify CI, and stop if red.
3. **Fresh, narrow agents.** Each agent has one role and one milestone. None carries the history of earlier milestones.
4. **The repo is the memory.**
   - Milestones hand off through commit SHAs: each milestone's base is the previous one's head.
   - Every milestone must keep `docs/design.md`, the configuration reference and the runbooks true.
   - The next architect therefore reads current, compact truth instead of a transcript.
5. **Structured handoffs.**
   - Every role returns JSON matching a schema. The plan includes files, tests, CI changes, docs changes, decisions and risks. The implementation report includes head, gates, CI result, deviations and problems. Each review finding has a severity, a failure scenario and a fix.
   - The implementer gets the plan verbatim, and the reviewer gets the plan plus `git diff base..head`.
   - The `problems` and `deviations` fields make agents state gaps plainly.
6. **Conventions written once.** `AGENTS.md`, which every agent reads, holds the environment's limits, the gates, CI timings and the push rules. Agents don't each rediscover that FUSE or docker don't work locally.
7. **Effort matched to role.** The architect is a read-only `Plan` agent at high effort, and the reviewer also runs at high effort. Implementers use the default effort and lean on tools and CI.
8. **CI is the referee; stop on red.**
   - Implementers prove their work on real GitHub Actions runs.
   - The run stops at the first milestone whose gates or CI are red, so nothing builds on a broken base.
   - A cheap "verify CI" step separates "still running" from "failed" before the run stops.
9. **Bounded loops.** There is one review and at most one fix round, triggered only by blocker or major findings. Minor findings are reported, not chased.
10. **Resume instead of redo.**
    - Every completed agent call is saved in the run's journal. After an interruption, `Workflow({scriptPath, resumeFromRunId})` replays the unchanged prefix from cache. Interruptions include usage limits, container restarts and a script fix.
    - To change behaviour mid-run, add new steps **after** the cached prefix; don't edit earlier prompts or `CONTEXT`. Pass identical `args`.
    - Resume works only within the same session. To move to another session, use a handoff (below).
11. **Don't poll.** Wait for completion notifications. Use one background `until` watcher only when you need to act at a specific point.

## Writing milestone briefs

- **Cite the design sections**, and say exactly what "done" means: behaviour, tests (unit, integration, CI e2e) and docs.
- **Give enough context** to start: the earlier review finding, the user's request (quoted), and known traps.
- **Order by dependency.** Put features before the packaging that exposes them (local-only mode before Helm). Put docs overhauls last, so they describe the final state.
- **Allow honest scope cuts.** If an item proves infeasible, the architect plans the closest alternative, documented as such. It must not fake coverage.
- **Keep briefs self-contained.** A brief written for one run can go into the next run's args unchanged.

## Lessons (mistakes not to repeat)

- **Slow workflows.** The bench workflow takes about 25 minutes. An implementer once reported "not green" while it was still running, and the run stopped. The script now verifies CI before stopping, and AGENTS.md lists each workflow's duration.
- **Stale journal entries.** A completion watcher grepped for a label (`critic`) that an earlier attempt of the same run had already written, so it fired early. Match only labels that can't have existed yet (the next milestone's architect), or count occurrences.
- **Pause at a clean point.** The safe point is the start of the next architect: it is read-only, so stopping it leaves nothing half-committed. Never stop an implementer or a fix agent mid-push.
- **Install real tools.** Don't write substitutes (for example, `zip` via the no-root apt recipe in AGENTS.md).
- **Workflow runs are big.** Each milestone uses about 4 to 5 agents. Thirteen milestones used several million subagent tokens. Tell the user the scale before launching.

## Handoff to another session

1. Pause at a clean breakpoint (see above) and stop the run with `TaskStop`.
2. Extract every completed step's result from the run's `journal.jsonl`. Match `type: "started"` entries (key → label) to `type: "result"` entries.
3. Write a handoff bundle:
   - `HANDOFF.md`: the state, a per-milestone table, open problems, the remaining work and how to continue.
   - `args-remaining.json`: `base` set to the current head, the remaining milestones, and **all** goals.
   - the results.
4. Upload the bundle as a draft GitHub release (`gh release create <name> --draft ...`), not into the repo, so the next session can download it. Containers are ephemeral.
5. The new session runs `milestone-loop.js` with `args-remaining.json`, once the user asks it to.
