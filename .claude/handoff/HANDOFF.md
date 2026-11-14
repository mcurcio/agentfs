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

## State (branch tip: B3's figures commit after f24ed4e)
- Landed and green before speed: module cut 5.1–5.3; M1 architect (5aeebfe, 6c7a936, 6d14e2c) and S1 (c23d594, 992c2ca, 1f44944); fast regression suite (de3e5de, 28f386b); speed B0 figures harness (c22a820, c828899), B1 (5771ad6, 32114b5), B2b interfaces (a78ac03, 3a35108), B2a (6e4ce1e). M1 S2 was folded into B0.
- **B2b landed with its remainder** (bb0d7b6..3b1ed68): the trailing builder, built log, lag gate, takes decided off the actor.
- **B4 landed** (c2bf696): hashed sub-folders (`files/`, `lines/`, `names/` under `<aa>/<bb>/`), object numbers in the journal checkpoint and `BuiltSnapshot`, steps applied in place in the builder, no `git diff` for a first save, state format `agentfs-trees-7`. Erratum "speed B4" in `.claude/errata.md`.
- Figures (image run 43580482406, release, CI runners): create 50,000 by 85.8 s ext4 / 79.4 s XFS (bar 50,000 by 300 s), `drain_s` 124 / 112 s (bar 360 s); close_4k p50 1.48 / 1.31 ms (bar 3.5 ms; an A/B on one machine showed no call-path change); close_64m 0.28 / 0.26 s. Restart reopen, debug: 600 files 0.47 s (B2b 2.6 s), 10,000 files 5.9 s.
- **B3 landed** (f24ed4e): file maps (no file bytes in the actor's memory), journal segments (`journal/`, 64 MiB, checkpoint segment plus drop behind, carry forward by the dead-over-live rule), checkouts by rename, save digest of pieces, state format `agentfs-trees-8`. Image run 43587792812: close_1g 5.44 s ext4 / 5.75 s XFS (bar 12 / 8), write_1g ratio 0.67 / 0.72 (bar 1.5); close_4k p50 1.42 / 0.91 ms, 50,000 creates in 97.7 / 70.4 s, drain 126 / 118 s, close_64m 0.40 / 0.47 s. The 1 GiB save's history did not build within 180 s on XFS (builder holds the file whole; 6.1 GB RSS locally). Erratum "speed B3".
- Open errata: S2, B3-dispatch, B2b, B2b rest, B4, B3. `planted/budget.json` is stale; only the owner may re-record it.

## Next, in order
1. One end review (verifier, durability lens, coherence lens) and ONE fix agent; then one full phase-1 dispatch on the final SHA.
2. Re-cut M1 S3's page on top of B2b, then S3 → S4 → S5 → S6, docs step, end review and fix (`m1-pages.md`).
3. Then the plan in `docs/target-architecture/future/one-engine.md`: M2 → M3 → M4 → M5 → M7 (with M6) → baseline.

## How to work (the owner's complaints, 2026-11-13/14: far too slow; "a test suite should run in seconds to minutes, not hours")
- Never dispatch the full phase-1 sweep or run the full model gate per slice. The default regression tier is enough: `cargo test -p agentfs-model`, `ci/model-digest.sh check`. One full sweep at the end of an epic.
- Build and test locally only the crates a slice changes, plus fmt and clippy; CI on GitHub runs the rest. Start the next slice when local checks pass; watch CI alongside and fix a red in the next step.
- No design or review loops for slices that already have frozen pages. One review at the end.
- Keep messages to the owner minimal: questions and actions needed only. He merges PRs; never push to main or force-push.
- Machine pitfalls in the previous session: 4 CPUs, little disk. The docker volume `agentfs-model-target` grew past 20 GB; use `CARGO_INCREMENTAL=0` and check `df -h /`. Run dockerd detached (`setsid nohup dockerd >/tmp/dockerd.log 2>&1 </dev/null &`), never under a time-limited background task.
- Commit trailers per AGENTS.md "Git"; no model IDs in commits or docs.

## Not done or not run
- The speed epic's end review, M1 S3–S6. The builder's memory and time on a 1 GiB save (line map whole, JSON) is unmeasured past the 180 s cap.
- The PR #39 description is stale (it describes step 6); rewrite it when the speed epic lands.
- A rule change for the owner's skills repo (scoping the repository rule to projects that use a database) sits unpushed in the previous machine's local clone and is lost with it; it needs the owner's go to redo.
