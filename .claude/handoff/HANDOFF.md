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

## State (branch tip 9d1bc84)
- Landed and green: module cut 5.1–5.3; M1 architect (5aeebfe, 6c7a936, 6d14e2c) and S1 (c23d594, 992c2ca, 1f44944); fast regression suite (de3e5de, 28f386b); speed B0 figures harness (c22a820, c828899), B1 in-process objects (5771ad6, 32114b5), B2b interfaces (a78ac03, 3a35108), B2a journal without per-write fsync plus group commit (6e4ce1e).
- M1 S2 (release figures) was folded into speed B0; S2 itself needs no further work beyond the speed bars.
- **B2b (the trailing builder) is NOT landed.** Its four commits stayed red and were reverted in 9d1bc84. The work is on branch `wip/speed-b2b` (96595e5). Red jobs at its last push 836eeda: phase-1 "journal and state exhaustion", the tree-views tracer on ext4 and XFS (image), and at times `test (default)`. Read `gh run view <id> --log-failed` for runs on 836eeda and eacd1db before changing anything.
- Figures (release build, CI runners): close_4k p50 7.2 ms ext4 / 10.6 ms XFS (bar 3.5 ms; through-FUSE budget is 6 ms); creates 3,900–5,800 in 300 s (bar 50,000); close_64m 7.7 s ext4 / 9.3 s XFS (bar 12 s / 8 s). Per-save cost still grows ~14 µs per file in the tree until B4.
- Open erratum entries in `.claude/errata.md` (S2, B3-dispatch). `planted/budget.json` is stale; only the owner may re-record it.

## Next, in order
1. **B2b**: resume from `wip/speed-b2b` (cherry-pick -n, recommit under Conventional Commits). Fix the red jobs; keep the page `speed-pages.md` B2b. If it will not go green in two attempts, use the page's fallback split.
2. **B4** (hashed sub-folders layout, the owner's pick), then **B3** (file maps, 1 GiB bar). Nothing concurrent: they share the cargo target.
3. One end review (verifier, durability lens, coherence lens) and ONE fix agent; then one full phase-1 dispatch on the final SHA.
4. Re-cut M1 S3's page on top of B2b, then S3 → S4 → S5 → S6, docs step, end review and fix (`m1-pages.md`).
5. Then the plan in `docs/target-architecture/future/one-engine.md`: M2 → M3 → M4 → M5 → M7 (with M6) → baseline.

## How to work (the owner's complaints, 2026-11-13/14: far too slow; "a test suite should run in seconds to minutes, not hours")
- Never dispatch the full phase-1 sweep or run the full model gate per slice. The default regression tier is enough: `cargo test -p agentfs-model`, `ci/model-digest.sh check`. One full sweep at the end of an epic.
- Build and test locally only the crates a slice changes, plus fmt and clippy; CI on GitHub runs the rest. Start the next slice when local checks pass; watch CI alongside and fix a red in the next step.
- No design or review loops for slices that already have frozen pages. One review at the end.
- Keep messages to the owner minimal: questions and actions needed only. He merges PRs; never push to main or force-push.
- Machine pitfalls in the previous session: 4 CPUs, little disk. The docker volume `agentfs-model-target` grew past 20 GB; use `CARGO_INCREMENTAL=0` and check `df -h /`. Run dockerd detached (`setsid nohup dockerd >/tmp/dockerd.log 2>&1 </dev/null &`), never under a time-limited background task.
- Commit trailers per AGENTS.md "Git"; no model IDs in commits or docs.

## Not done or not run
- B2b, B4, B3, the speed epic's end review, M1 S3–S6.
- The PR #39 description is stale (it describes step 6); rewrite it when the speed epic lands.
- A rule change for the owner's skills repo (scoping the repository rule to projects that use a database) sits unpushed in the previous machine's local clone and is lost with it; it needs the owner's go to redo.
