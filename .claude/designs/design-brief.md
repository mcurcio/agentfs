# Brief: design of module-cut steps 5b, 5 and 6 (design only; nothing is implemented)

Repo /home/user/agentfs, main at 22d55f9 (steps 2-4 built). Read on main, not from memory:
- docs/target-architecture/future/module-cut.md — the frame: modules table, "Where a rule crosses a boundary", design rules,
  Steps (rows 5, 5b, 6), "How each step runs", the model gate and its light tier.
- OWNER-GUIDANCE.md (the owner's dated answers; don't reopen them without new evidence).
- AGENTS.md "Design docs": a design page lives only until its slice is built.
- crates/agentfs-model/MODEL.md, VERIFYING.md, docs/target-architecture/rules-ledger.md, git-model.md (baseline pages).
- Your step's own pages (below), docs/architecture.md, docs/configuration.md, docs/supported-configurations.md, and the code.

## Layer
Interfaces and implementation, inside the agreed frame (module-cut.md and, for your step, its own design page). Do not redesign the
frame or another step. A problem with the frame or a requirement is a question, not a quiet change.
The Architects rule applies: read the code for constraints and cost of change; design toward best practice and named patterns.

## Requirements vs guidance (keep apart; tag every statement)
- Requirement: only a question the owner answered yes as an absolute requirement, or a statement he called a rule. Quote it, dated.
- Guidance: the owner's other dated words (OWNER-GUIDANCE.md). Quote, dated.
- Proposed: yours. Everything you decide that the owner hasn't answered.
Owner guidance most relevant now (2026-11-11): "refs/heads/main should not be special or hardcoded. Consumers will configure the
name of the root branch."; "design for both, build the A solution first" (recorder); the light/full model-gate tiers.
Delivery practice the owner set: one PR per step to main (steps 3a-3c, 4 used PRs), the owner merges.

## The steps
- **5b** — Engine: the root branch's name is configured, not `main`; `ENGINE_ROOT` is deleted. Page:
  docs/target-architecture/future/root-branch-name.md (it has three open owner questions: existing volumes' `main` refs; the default
  when none is set; whether to reuse `agentfs.branch`, which the older engine already uses with default `agentfs/<volume>`).
  Its architect may pull it before step 5 — decide the order of 5 and 5b and say why.
- **5** — Engine: remove the duplicated ancestry check, name combining and R3e; a typed version; then split `tree_actor/mod.rs`.
  Gate: conformance traces on every scenario and seed stay at 0 mismatched.
- **6** — Model: R9.18, R10.5 and R10.11 (docs/target-architecture/future/transactions.md); the built-in `remote` and its replicate
  step leave `World`; it settles what a mirror's push timing must be (MODEL.md says replication is "due on every move of the root and
  every collision", stricter than any check). Gate: the digest is re-baselined on purpose; finding 30's test is no longer ignored.
  Rule changes are the owner's: any change to a rule's meaning is a question for him, with the ledger entry it changes.

## Deliverable (write to /tmp/claude-0/-home-user-agentfs/41ac0cfe-83c8-57b0-9f09-59a6e4c29f38/scratchpad/next/step<N>-design.md, ≤ 6 pages)
1. Requirements (numbered, sourced) and guidance, kept apart; then the proposed shape.
2. Canonical shape: the domain concepts touched and any change to the domain model (a change is an owner question); the design
   patterns used, named, with their canonical source.
3. At most three design rules (invariants that make the shape hold), labelled as design choices.
4. Interfaces as Rust signatures (types, traits, error enums, who is injected with what); the module map after the step.
5. What moves where, and the rules each module owns afterwards; every crossing.
6. Risks, with how the design avoids each; for model work, risks to the digest and the order of random draws; for engine work,
   risks to recording (principle 1) and to conformance traces.
7. Slices, tracer first, each green alone; the gate per slice and for the step: which tier of the model gate (VERIFYING.md's
   mechanical test), which CI (ci, image, phase-1 dispatch, full tree-view CI) with durations from AGENTS.md or measured runs.
8. Docs: which pages change, which design page is deleted when the step lands.
9. Probes of your riskiest claims, with real output. Reading, grep, git, `cargo check` and non-model `cargo test` are fine. If you
   need to edit to probe, use one scratch copy under .../scratchpad/next/probe-<N> and delete its target dir when done. Run no
   model run (no ci/model-digest.sh, run.py or agentfs-model tests) and no docker container. Disk is limited (~17 GB free):
   check `df -h /` before building. List what you did not run as NOT RUN.
10. Owner questions: AT MOST 3 for your step, each something only the owner can answer (policy, cost, data migration, a rule's
    meaning). Decide everything else and say what you decided. Each question: one plain line; background (what it is, when it comes
    up, what the answer changes); options as a comparison (recommended first: what it gives, what it costs, what the owner would see);
    and whether it asks for general guidance or an absolute requirement (with the exact sentence that would bind). Use plain words;
    define any new term in a new-terms table.

Commit nothing. Edit nothing in /home/user/agentfs.
