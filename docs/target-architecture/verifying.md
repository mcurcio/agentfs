# Verifying the design

The target architecture's rules are checked by an executable model on the real `git` program,
`crates/agentfs-model`, before anything is built on them. How a design change is verified there (what to run
and when, seeds and replays, planted bugs, adversarial review, reading a failure, a reviewer's checklist, and
the plan to reuse the model as the implementation's conformance suite) is described in one place:
[Verifying a design change with the model](../../crates/agentfs-model/VERIFYING.md). The rules, invariants and
findings the model checks are in [MODEL.md](../../crates/agentfs-model/MODEL.md); what they mean in git terms
is [agentfs in git terms](git-model.md).
