# Plan: from the module cut to one engine (reconciled, 2026-11-12)

Working state for the owner's gate. Read on `main` 22d55f9. Closes the round-2 critic (`round2.json`, F1-F9). Live
designs: `step5b-design-r3.md` (rev. 4), `step5-design-r2.md` (rev. 5), `one-engine-design.md` (rev. 3), and
`step6-design.md` with `step6-impl-brief.md` (in flight). Superseded and marked: `step5b-design.md`,
`step5b-design-r2.md`, `step5-design.md`, `critic.md`, `design-brief.md`; round 1's `gate.html` and
`final-questions.json` are history only.

## 1. Order

**6 → P → 5b → 5 (5.1, 5.2, 5.3) → M1 → M2 → M3 → M4 → M5 → M6 → M7 → M8 → M9.** Step 7 (model) runs beside M1, after
5 and before M2. Each is its own PR from `main` after its predecessor merges; only step 6's branch was cut earlier
(22d55f9), and it lands first, so it rebases on nothing. P rebases on 6; step 7 and M1, the one parallel pair, touch
disjoint crates, and the second to merge rebases. Line numbers in every design are 22d55f9's; each implementer re-reads
them on its base.

**Why this order.** Step 6 is in flight and answered. P lands the gate's answers and the plan pages before any code
reads them. 5b before 5: step 5's slice 4 builds `Upstreams`, `refs` and `Message` from post-5b identities. 5 before
M1: step 5's design is finished against post-5b code and M1's is not, so M1's architect designs on the six modules while
5 is built, and M2, M3, M4 and M6 each extend a module 5 makes (`step5-design-r2.md` §6). Step 7 before M2: M2 adds
model rules, which a checker reading git then judges. Cost: M1's build waits for three PRs and two dispatches.

## 2. Steps, one line each

| Step | What it delivers | Gate (beyond architect, review, one fix round, ci and image) |
|---|---|---|
| 6 | Model: the mirror leaves `World` (S1); R10.5 and R10.11 at a transaction's commit (S2); R9.18, every take (S3) | full model gate per slice; one `phase-1.yml` dispatch |
| P | Docs only: the plan pages and this gate's answers (§5) | `docs_reference`, docs.yml |
| 5b | Every tree by identity (`root`, `fork/<n>`); no `main`; `TreeNames` and conformance take no root; format 3 | light tier (S1), full model gate (S2), one dispatch |
| 5 | Rules once, typed versions (5.1); verbatim moves (5.2); modules own their state (5.3) | dispatches on 5.1 and 5.3 |
| 7 | Model: the checker re-derives from git what it reads through `World` (module-cut's "later" checker row); deletes module-cut.md | full model gate, matrix changed on purpose, every planted bug caught |
| M1 | `ensure(VolumeSpec)` for every launcher; the "where" trailers (Q3); the tracer job starts | dispatch; tracer |
| M2 | Directories, symlinks, non-UTF-8 names, links; file metadata | full model gate (new rules); dispatch |
| M3 | Rules and the untracked area | dispatch |
| M4 | Mirrors, the publisher and the root's published name (its key), history a mirror's reader can read | dispatch |
| M5 | Telemetry, alarms, health, restart in place; recorder source B (module-cut's engine-source row) | dispatch |
| M6 | iac's patches at mount, restore, revert on trees (under Q1) | dispatch |
| M7 | The `agentfs` binary moves: sidecar, Docker, CLI; `agentfs trees` folds into `init`/`mount` | release-build tracer against `ci/bench-budget.json` |
| M8 | The CSI driver moves (sandbox claims per Q2) | as M7 |
| M9 | The recording engine is deleted | ci, image |

## 3. Goals and the step that delivers each

| End goal (source) | Delivered by |
|---|---|
| One engine ("the goal is to have one engine", 2026-11-12) | M1, M7, M8, M9 |
| `trees init` never run by hand ("should never have to do this manually", 2026-11-12) | M1 (`ensure` from configuration), M7 (`agentfs trees` goes) |
| The root known by identity (2026-11-12); "refs/heads/main should not be special or hardcoded" (2026-11-11) | 5b |
| "Consumers will configure the name of the root branch" (2026-11-11); "why must the branch named be fixed at creation time?" (2026-11-12) | P (the name leaves `VolumeSpec`; changeable), M4 (the publisher and its key) |
| The trailer "records meaningful information" (2026-11-12) | 5b (which tree), M1 (where, Q3) |
| Configuration: iac and the agent both edit it (AGENTS.md) | M6, before M7 (Q1) |
| Audit; recording is the baseline (principle 1) | built; step 5 rule 2; M1, M2, M3 |
| Invisible and untouchable (2) | built; M3, M4 |
| Git idioms (3) | M1 (include), M3, M4 (the published name's key) |
| Right technology, recorded (4) | M4 (transports' reuse in architecture.md), M9 |
| Keep operating (5) | M4, M5 |
| Any remote, or none (6) | 6 S1 (mirror in the model), M1 (zero remotes), M4 |
| History is the product (7) | 5b, M4 |
| Recovery (8); merging is an extra (9) | M6 |
| Three layers; OpenTelemetry for everything observable (10); "we probably need A+B eventually" (2026-11-11) | each milestone's library page; M5 (source B) |
| The model is the baseline proof ("These stay as the baseline proof and testbed", 2026-11-09) | 6 S2, S3 (finding 30 runs); 7 |
| The module cut, "B" (2026-11-08) | 5b, 5, 6, 7 |
| "drop support" for old formats (2026-11-06) | 5b (format 3), M7 and M8 (`FormatsOf`) |
| Design docs deleted when built (requirement, 2026-11-09) | §4 |
| "plan docs are updated and consistent and there are no gaps" (2026-11-11) | P |
| "goal oriented" applied to his mcurcio/skills (2026-11-12) | **outside this repo**: a PR to mcurcio/skills by the main session after this gate; no agentfs step |

## 4. Doc ownership: who writes and who deletes

| Doc | Written or changed by | Deleted by |
|---|---|---|
| `future/module-cut.md` | P: order, row 5b's text, row 7 numbered, source B moved to M5, "Costs" line (73 min), line 26, "Not in this slice" (points to one-engine); 6: row 6, l. 37, 130-131, rule-3 table; 5b: row 5b Built, first bullet; 5: row 5 | **7** (the cut's last step) |
| `future/root-branch-name.md` | P: cut to the published name (M4's key, no-name-set and rename questions; "set once by `trees init`", "fixed at create" and "2 to 3 hours" go) | **M4** |
| `future/one-engine.md` (new) | P writes it; each milestone updates its row | **M9** |
| `future/transactions.md` | 6 updates it | stays: the engine runs no transactions (an extra) |
| `future/tree-views-beyond-flat-files.md`; `tree-view-telemetry.md`; `integrator-patches.md` | — | M2; M5; M6 |
| `future/line-map-and-renames.md` | M4 | M4, or cut to its unbuilt half |
| `future/landing-approvals.md` | — | stays under Q2's recommended option; M8 under its second |
| New pages: file metadata (M2), CSI on trees (M8) | their architect | their milestone's last PR |
| `domains/volume.md` | P: `root` leaves `VolumeSpec` (l. 63); M1: `create` idempotent, `SpecMismatch`; M8: release for CSI | stays (upgrade drain, start line are extras) |
| `domains/replication.md`, `rules.md` and `views.md`, `health.md` and `telemetry.md`, `trees.md`, `clients.md` | M4; M3; M5; M8; M1 and M7 | stay while extras remain |
| `target-architecture/README.md`, `docs/SUMMARY.md` | P adds one-engine; each deleting step drops its row | — |
| Baseline: git-model.md, rules-ledger.md, MODEL.md, VERIFYING.md | 6 (§4, §9, §15; R9.18, R10.5, R10.11; findings); 5b (git-model 74, 196, 211, 228; ledger 35-36; MODEL.md 3-4) | never |
| OWNER-GUIDANCE.md | 6: step 6 answers, terminology. P: one engine, `trees init`, identity and name with "why must the branch named be fixed", goal-oriented, this gate's answers; line 26's and line 58's brackets repointed. Each later step: its own answers | never |
| AGENTS.md | M1: CI table (tracer). M6: principle 9's wording and links. M7: principles 1, 2 links; CI table (docker, hermes, power-loss, bench on trees). M8: CI table (f8, helm). M9: Layout (`agentfs-core` row), principle 4 (LFS), CI table, "Read first" | — |
| README.md | M7: quick start and what users see; `agentfs trees` gone. M9: LFS, `serve-git` gone | — |
| Human and library docs (trees, configuration, architecture, guarantees, recording, remotes, patches, forks, library/*, runbooks, deployment, csi, telemetry) | the step that changes the behaviour (5b: trees, configuration `--writer`; M1: configuration's spec keys; M4: the published name's key) | recording-only parts at M9 |

## 5. CI cost (measured figures; counts are estimates)

Before M1: **4 `phase-1.yml` dispatches** (6, 5b, 5.1, 5.3; 73 min each, run 43146523186: ~4.9 h of runner time) and
**4 full model gates** on this machine (6 ×3, 5b ×1; ~41 min each: ~2.7 h), plus ci and image (~5-9 min each, run side
by side) on every pushed SHA, about 17 SHAs; step 7 adds 1-2 model gates beside M1. M1 to M9: about one dispatch per
engine milestone (~8, ~9.7 h), M2's model gate, the tracer job (unmeasured) and both engines' jobs in ci until M9. The
"2 to 3 hours" in module-cut.md:109 and root-branch-name.md:94 is stale; P replaces it with 73 min.

## 6. What each gate decides

| Gate | Decides | Owner questions |
|---|---|---|
| **5b** (now) | the identity shape; S2 removes `TreeNames`' root (F3); no `main` anywhere | none (rev. 3's Q1 moved to one-engine Q3: every option kept 5b the same) |
| **one-engine** (now) | the shape, the order, `VolumeSpec` less `root`, `ensure` | Q1 goals before a move; Q2 sandbox approval; Q3 where a commit was made |
| **step 5** (now) | its four slices and two dispatches | none |
| 7 | which checks re-derive from git; the matrix change | its architect's |
| M1-M9 | each its own architect's gate, one layer down (G6). M4 asks the published name's key and what a rename does; M5 squares source B's lossless listener with the export; M2 and M8 write their missing pages | at most 3 each |

## 7. Step 6 follow-ups (not edits to `step6-design.md`)

1. Errata 1, in full: no `HEAD` assert in `TreeNames::new`, no "HEAD refused" digest test, no §4 doc-comment line,
   and row 6's built text without "refuses `HEAD`, as the engine's `RootName` does". 5b S2 changes `TreeNames` instead.
2. Errata 2 stands: transactions.md stays, updated.
3. **One `phase-1.yml` dispatch is needed** (design §8 is right, the brief's "none unless conformance changes" is
   not): S1 changes `bin/phase1-oracle.rs`, which only that workflow runs (module-cut row 3c).
4. In module-cut.md touch only row 6, lines 37 and 130-131 and the rule-3 table; P rewrites row 5b, the "later" rows
   and the "two later rows" sentence after rebasing.
5. The design's §11 lists Q1 and Q2 as open; both are answered (2026-11-12). The PR body says so, and carries the goal
   line of §3 (S1: any remote; S2, S3: the baseline proof).

## 8. Critic findings

| | Closed by |
|---|---|
| F1 | M4 owns the publisher, the key question, and deleting root-branch-name.md; P moves the name out of `VolumeSpec` (changeable), per "i agree with that split" |
| F2 | "Where" is M1's, asked once (one-engine Q3); milestone numbers are one-engine's everywhere (5b fixed) |
| F3 | Step 6 adds no `HEAD` check; 5b S2 removes `TreeNames`' root and rewrites MODEL.md:3-4 |
| F4, F5 | §1: one order, who rebases, why 5 precedes M1; step 6 S2, S3 serve the baseline-proof goal |
| F6 | Shown not a domain change: today's `agentfs init` is already idempotent with a mismatch refusal (configuration.md), the CSI spec requires it, volume.md's states and VO1-VO7 stay (one-engine §3) |
| F7 | §4; source B → M5; the checker row → step 7 |
| F8 | §5. Refuted in part: 232 test sites is right on 22d55f9 (`grep -c`); r2's 262 was wrong |
| F9 | Stale box, quote and preview question removed; files marked |

Unproven, unchanged: the relayed "3. A" is read as step 6's commit question (owner notes, 2026-11-12: "A"); if it
was 5b rev. 3's Q1, it matches this plan (identity now, where at M1). `shows_the_same` while a landing waits, the
fork-order argument and step 5's ~4,500 lines are from reading, not runs.
