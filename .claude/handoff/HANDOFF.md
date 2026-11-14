# Handoff: PR #39, tree views speed then M1 (2026-11-14)

Start here. This folder is working state for the next session, not product docs. **Delete `.claude/handoff/` in the commit that finishes this work, before #39 goes to the owner for review.**

## The problem, plainly
agentfs is moving to one engine, the tree views (`crates/agentfs-core/src/tree_actor/`). The owner wants all of it on ONE PR, #39, branch `trees/module-cut-step-6`, and said "Keep going" until the baseline: Hermes sidecar CI on tree views, after milestones M1…M7. M1 (volumes, clients, forks, lifecycle) is paused after S1 because the tree views were far too slow; the owner ruled "Speed before S3" and set the bar "Match recording engine". Speed work is half done.

## Precedence
1. `OWNER-GUIDANCE.md` (dated, verbatim; all answers through 2026-11-13 are recorded there).
2. `owner-notes.md` here (the same words, with the questions as asked).
3. `speed-brief.md` (bar, evidence, owner direction), then `speed-pages.md` (frozen slice pages B2a, B2b, B4, B3), `speed-design.md` (v4 module design) and `speed-interfaces.md`.
4. `m1-design.md` (rev. 2, with its "Gate outcome" section), `m1-pages.md` (S1–S6 pages; S3's page must be re-cut after B2b, because S3's row commits move into the builder), `m1-impl-brief.md` (rails).
The code and the library/human docs win over all of these where they disagree; report the disagreement.

## State
- Speed epic built (B0..B4, B3) and reviewed (`review/`); Fix 1 (3e9e1bc..fede47f) and Fix 2 (d7666ef and the
  commit after it) applied the review. Figures at d7666ef (image run 43608163463): close_4k p50 1.23 / 0.74 ms,
  50,000 creates in 43.5 / 18.8 s, history_1g 31.5 / 32.5 s, memory_1g +36 MiB; all bars met.
- Residuals with plans, in `.claude/errata.md` ("speed fix 2"): frame relocation owned by the journal (coherence 3),
  `TreeActor` constructors crate-private (coherence 5), power-loss crossing compaction (P3-4), builder `owners`
  growth (P3-6), about 20 KiB of view memory per file. `planted/budget.json` is stale; only the owner may re-record it.

## Next, in order
1. One full phase-1 dispatch on the final SHA (`gh workflow run phase-1.yml`), read its result.
2. Re-cut M1 S3's page on top of B2b, then S3 -> S4 -> S5 -> S6, docs step, end review and fix (`m1-pages.md`).
3. Then the plan in `docs/target-architecture/future/one-engine.md`: M2 -> M3 -> M4 -> M5 -> M7 (with M6) -> baseline.

## How to work (the owner's complaints, 2026-11-13/14: far too slow; "a test suite should run in seconds to minutes, not hours")
- Never dispatch the full phase-1 sweep or run the full model gate per slice. The default regression tier is enough: `cargo test -p agentfs-model`, `ci/model-digest.sh check`. One full sweep at the end of an epic.
- Build and test locally only the crates a slice changes, plus fmt and clippy; CI on GitHub runs the rest. Start the next slice when local checks pass; watch CI alongside and fix a red in the next step.
- No design or review loops for slices that already have frozen pages. One review at the end.
- Keep messages to the owner minimal: questions and actions needed only. He merges PRs; never push to main or force-push.
- Machine pitfalls in the previous session: 4 CPUs, little disk. The docker volume `agentfs-model-target` grew past 20 GB; use `CARGO_INCREMENTAL=0` and check `df -h /`. Run dockerd detached (`setsid nohup dockerd >/tmp/dockerd.log 2>&1 </dev/null &`), never under a time-limited background task.
- Commit trailers per AGENTS.md "Git"; no model IDs in commits or docs.

## Not done or not run
- M1 S3–S6; the full phase-1 sweep on the final SHA; the residuals above.
- The PR #39 description is stale (it describes step 6); rewrite it when the speed epic lands.
- A rule change for the owner's skills repo (scoping the repository rule to projects that use a database) sits unpushed in the previous machine's local clone and is lost with it; it needs the owner's go to redo.
