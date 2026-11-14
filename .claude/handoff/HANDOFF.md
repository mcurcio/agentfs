# Handoff: PR #39, tree views speed and M1 built (2026-11-14)

Start here. This folder is working state for the next session, not product docs. **Delete `.claude/handoff/` in the commit that finishes this work, before #39 goes to the owner for review.**

## The problem, plainly
agentfs is moving to one engine, the tree views (`crates/agentfs-core/src/tree_actor/`). The owner wants all of it on ONE PR, #39, branch `trees/module-cut-step-6`, and said "Keep going" until the baseline: Hermes sidecar CI on tree views, after milestones M1…M7. The owner ruled "Speed before S3" with the bar "Match recording engine"; the speed epic and M1 (volumes, clients, forks, lifecycle) are built. Next is M1's end review.

## Precedence
1. `OWNER-GUIDANCE.md` (dated, verbatim; all answers through 2026-11-13 are recorded there).
2. `owner-notes.md` here (the same words, with the questions as asked).
3. `speed-brief.md` (bar, evidence, owner direction), then `speed-pages.md` (frozen slice pages B2a, B2b, B4, B3), `speed-design.md` (v4 module design) and `speed-interfaces.md`.
4. `m1-design.md` (rev. 2, with its "Gate outcome" section), `m1-pages.md` (S1–S6 pages; S3–S6 are overridden by `m1-s3-recut.md`, `m1-s4-recut.md`, `m1-s5-recut.md`, `m1-s6-recut.md`, re-cut on the speed epic), `m1-impl-brief.md` (rails).
The code and the library/human docs win over all of these where they disagree; report the disagreement.

## State
- Speed epic built (B0..B4, B3), reviewed (`review/`) and fixed (Fix 1 3e9e1bc..fede47f, Fix 2 d7666ef.., Fix 3
  a19b657..6c39416). Figures in docs/performance.md. Residuals, each to be a bead (text in `.claude/errata.md`,
  "speed fix 3"): coherence 5, 6, 7, builder `owners` growth, checkpoint size at millions of files, B4 8, memory per
  file to 2 KiB, pack count. `planted/budget.json` is stale; only the owner may re-record it.
- M1 built: S3 9ea0aae (volume events as commits), S4 3674582 (clients, sessions, view host; 82982a0 conformance
  fix), S5 fff3ddb (fork keys, end, drop), S6 af72555 (stage, Stop drain, release). Docs step done: docs/trees.md and
  the library describe M1; the domains' invariants (C1..C7, VO1..VO6) are in docs/library/core.md, "Domain
  invariants", and code comments cite them there; clients.md trimmed to its unbuilt parts; one-engine.md marks M1
  built; recording-failures runbook section 7. Per-slice deviations are in `.claude/errata.md`.
- M1 end review done (verifier, durability, coherence lenses) and its one fix round landed: the contract re-cut (pod
  context on launcher calls, agentfs events stamped by the store, `QueueFacts::queued` alone and fallible, one drop
  judge, `VolumeError::Stopped`), level-triggered recovery (Trees' `settle(clients)` ends unattached unkeyed forks;
  `reconcile` releases dropped forks' claims and runs at the drain), bounded waits in the tree e2e scripts. Errata:
  "M1 end review fix round"; four residuals under "M1 residuals" (coordinator to a library, `stage` judges the name,
  failpoints armed by event, take worker to core), each to become a bead.

## Next, in order
1. Confirm CI (ci, image, phase-1) green on the fix round's SHA; cut beads for the four "M1 residuals".
2. One full phase-1 dispatch on the final SHA (`gh workflow run phase-1.yml`), read its result.
3. Then the plan in `docs/target-architecture/future/one-engine.md`: M2 -> M3 -> M4 -> M5 -> M7 (with M6) -> baseline.
4. `docs/target-architecture/future/save-and-take-speed.md` still reads "Proposed, not built" with pre-speed figures;
   decide whether the speed epic built it (then delete it and repoint its links) or trim it to what remains.

## How to work (the owner's complaints, 2026-11-13/14: far too slow; "a test suite should run in seconds to minutes, not hours")
- Never dispatch the full phase-1 sweep or run the full model gate per slice. The default regression tier is enough: `cargo test -p agentfs-model`, `ci/model-digest.sh check`. One full sweep at the end of an epic.
- Build and test locally only the crates a slice changes, plus fmt and clippy; CI on GitHub runs the rest. Start the next slice when local checks pass; watch CI alongside and fix a red in the next step.
- No design or review loops for slices that already have frozen pages. One review at the end.
- Keep messages to the owner minimal: questions and actions needed only. He merges PRs; never push to main or force-push.
- Machine pitfalls in the previous session: 4 CPUs, little disk. The docker volume `agentfs-model-target` grew past 20 GB; use `CARGO_INCREMENTAL=0` and check `df -h /`. Run dockerd detached (`setsid nohup dockerd >/tmp/dockerd.log 2>&1 </dev/null &`), never under a time-limited background task.
- Commit trailers per AGENTS.md "Git"; no model IDs in commits or docs.

## Not done or not run
- The full phase-1 sweep on the final SHA; the speed residuals and the four M1 residuals above.
- The PR #39 description is stale (it describes step 6); rewrite it when the speed epic lands.
- A rule change for the owner's skills repo (scoping the repository rule to projects that use a database) sits unpushed in the previous machine's local clone and is lost with it; it needs the owner's go to redo.
