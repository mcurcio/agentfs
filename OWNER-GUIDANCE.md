# Owner guidance

This page keeps what the owner has said that still holds, so that a future agent does not re-ask a settled question.

Each entry gives his words and their date. **Requirement** marks a question asked as an absolute requirement and
answered yes, or a statement he called a rule. Everything else is **guidance**: it stands until new evidence
changes it. Only the text inside quotation marks is his. Everything else, including wording in square brackets and
the "Applies to" lines, is the agent's summary. Where the code and the docs show how a point was built, they are the
home of the behaviour; this page holds the words. The rules ledger and `MODEL.md` also quote him where a rule rests
on his answer.

## Documentation

- **Requirement**, 2026-11-09. Asked as an absolute requirement: a design doc exists only until its slice is built, and the commit that lands the slice deletes it and moves the behaviour into library or human docs. His answer: "Yes, this is a good idea to record: docs should be written towards future readers".
- **Guidance**, 2026-11-09. "We don't need to keep design docs focused on implementation plans. Docs should be purged and rewritten so as to focus on library/human docs. The only design docs that should persist are for future slices that haven't been implemented yet".
- **Guidance**, 2026-11-09. Of the rules ledger, `docs/target-architecture/git-model.md`, the model's `MODEL.md` and `VERIFYING.md` and the walkthrough: "These stay as the baseline proof and testbed". Applies to: `docs/target-architecture/`.

## Tooling

- **Guidance**, 2026-11-10. On opting agentfs in to his shared skills marketplace (`rules` and `workflow`): "add this config to the current PR, and include semantic release". Applies to: `.claude/settings.json`.

## Terms

- **Guidance**, 2026-11-08. "the terminology of sandbox/main/parent is nebulous. We should be discussing a pure model of parent/child or upstream/fork." Then: "Terminology: upstream/fork". Applies to: the model, the ledger and the target-architecture pages. [Main is the tree with no upstream.]

## Trees, forks and what they receive

- **Guidance**, 2026-11-01. "a virtual filesystem overlay for each client (including main) that is backed by a _real_ filesystem source-of-truth".
- **Guidance**, 2026-11-01. "the sandbox fs edits need to land back in the main container, and then future sandboxes should start from the edited files".
- **Guidance**, 2026-11-01. "Agentfs history shouod distinguish between the various containers so we can track direct edits on main vs the forks that sandboxes edit".
- **Guidance**, 2026-11-02. "no no no, dont go back on the fork design. thats a cleaner model. Im not arguing for one shared backend fs -- im arguing for making the fork design so rock-solid that it is almost as effective, by syncing forks quickly and efficiently to minimize conflict drift".
- **Guidance**, 2026-11-02. "If the changes are passing bi-directionally, then keeping the two trees relatively in-sync should minimize conflicts, right? Ideally, the two conflicting changes would be interleaved in commits, and both clients would see the others changes -- this would be the ideal parallel of a real, local filsystem, and those clients dont need to know that its all virtualized and backed by git."
- **Guidance**, 2026-11-02. Asked whether sandboxes are peers: "Yes, I think so. The sandboxes are generally peers and parallelism is assumed."
- **Guidance**, 2026-11-08. "from the perspective of f2, f1 is its upstream. remember, there is no "main" in this new model of upstreams and forks"; "shouldnt a fork receive all changes from its upstream? barring transactions and locks, a fork configured to sync should stay up to date".
- **Guidance**, 2026-11-08. On remotes: "In my mind, GitHub is something closer to a peer than an upstream. The root of the agentfs tree may mirror to GitHub for resiliency and observability — and might support PRs of its own — but we must ensure that a remote like GitHub cannot introduce conflicts." Asked whether that is a requirement: "Let’s call it guidance for now." On the shape: "I like the idea of agentfs having a native root to its tree which holds the canonical history, and then having a different model for mirror repos". [Unbuilt direction, with no design page: a slice for it must keep a remote from introducing conflicts.]
- **Guidance**, 2026-11-09. Asked whether a sync may fast-forward past a fork's own earlier sync merge, which carries no edits: "A". [The cost: no ref reaches that merge any more, against the letter of never rewriting history. Revisable. Applies to: `docs/trees.md` and ledger R6.1.]
- **Guidance**, 2026-11-08. Asked whether a file held open inside a transaction holds back syncs as an open does: "Yes. “Waiting longer” is a feature of locks and transactions, not a bug". Of a commit on files not open: "If the file is not open, then it follows the normal merge rules". Applies to: ledger R9.18 and R10.11.
- **Guidance**, 2026-11-08. On making a take one atomic step: "Without any other information, B sounds reasonable. Obviously atomic operations are more reliable". Applies to: ledger R9.
- **Guidance**, 2026-11-06. On the journal as the commit point for pointer moves, accepted as part of a batch: "we can take those recommendations for now, and i might change my mind later". [Revisable. Applies to: `docs/target-architecture/git-model.md` section 3.]
- **Guidance**, 2026-11-01. "I want the DDD to target the *ideal* design, and not just the easy one ... treating each domain as a hard boundary with strict contracts". Of commits: "THE EDITS CAN LAND IN ONE COMMIT?!?" [read as: no commit holds two writers].
- **Guidance**, 2026-10-16. "bring all of those patches back together in an automated way".
- **Guidance**, 2026-11-05. "Locks, keep snapshots on file" [the snapshot-opens idea is kept as a parked slice]. Also: "alright, lets try option 4 until it becomes a problem."; "we absolutely must avoid deadlocking, while also trying to minimize starvation". Applies to: `docs/target-architecture/snapshot-opens.md`.

## Saves, landings and shared mappings

- **Guidance**, 2026-10-16. "Whole seems best, right? A partial application has unknown side effects" [A save or a transaction lands whole or loses whole.]
- **Guidance**, 2026-11-02. "A cooperating integration can define batches/transactions across files, and they must all land together. But a default usage, like the CNI provides, can operate per-file." [By "CNI" he meant agentfs's CSI driver.]
- **Guidance**, 2026-11-02. On file watchers: "we can take the "no guarantee" path for now".
- **Guidance**, 2026-11-06. On shared memory mappings: "A is fine. but im still on the fence about memory mapped files, because they seem to be causing issues. Go with (A) for now"; of a later close, "if the mapped bytes were written second in this shared fs, then i would expect them to win".
- **Guidance**, 2026-11-09. Asked whether to keep accepting the gap or refuse shared mappings: "A for now, but C might be reasonable in the future". Applies to: `docs/target-architecture/future/shared-mappings.md`.
- **Guidance**, 2026-11-06. On state formats from earlier work: "drop support. we are pre-production and there is no need to worry about "supporting" old concepts".
- **Guidance**, 2026-11-08. On how to cut the tree actor into modules, he picked option "B" [six modules: recording, queue and locks, referee, transactions, tree history and journal, with the tree actor only composing them]. To start when the module is next opened. Applies to: `docs/target-architecture/future/module-cut.md`.
- **Guidance**, 2026-11-09. On the revised module cut: "I generally approve. I want each phase of the work to start with an architect". Applies to: every step of `docs/target-architecture/future/module-cut.md`.

## History and versions

- **Guidance**, 2026-11-03. "yes, "losing" patches should still be part of the history. i want to know who changed what when".
- **Guidance**, 2026-11-01. "A fork whose branches are rejected and never land in the main branch will inevitably be lost, and that's acceptable".
- **Guidance**, 2026-11-05. "drop squashing" [History is full lineage.]
- **Guidance**, 2026-11-02. "i would like for the git history to remain compatible across minor versions, and only break on a major version change. The various agentfs components should be within the same version family (major for sure, maybe minor too, patch doesnt need to match)".
- **Guidance**, 2026-10-18. On the operator's lease push: "an admin should have more expansive abilities to fix issues and overwrite history. it should be documented and auditable (where possible), but "admin" users are a specific class". Applies to: `docs/runbooks/divergence.md`.
- **Guidance**, 2026-10-24. "It's all event driven". [Waits end on events; the only timers are operator schedules and probes.]

## Patches

- **Guidance**, 2026-10-16. "Not A, because that assumes that a clean merge is an acceptable merge". On accepting a clean merge, 2026-10-29: "Configurable with a sensible default".
- **Guidance**, 2026-10-28. "agentfs should be a legitimate git server, and its up to the specific use case to wire up the relationship of the various distributed remotes"; "git protocol is ideal".
- **Guidance**, 2026-10-28. "agentfs should support merging live". Then, 2026-10-29: "No live landing yet", and "Build incrementally". Applies to: `docs/target-architecture/future/live-landing.md`.
- **Guidance**, 2026-10-29. "Then the question isn’t “quiet moments or not” — it’s how do we provide an API so that an integrator can trigger this action?"; on the result: "I like that flow. Approved". [agentfs sets no timing rule for writing queued patches into a running volume; it tells the integrator a patch arrived and applies when told.]
- **Guidance**, 2026-10-28. "my opinion is that squashes are too blunt, but rebase can be attractive for cleaner history. then again, merge is more explicit about the process and the merge commit might be a useful artifact to note when agentfs did its work".
- **Guidance**, 2026-10-28. "Couldn’t branches of branches be rolled up before they are merged into the “main” branch? This is not really a unique concept — it’s just how git normally works."
- **Guidance**, 2026-10-29. A checkpoint cuts "one commit per change intent".
- **Guidance**, 2026-10-15. "i dont want to invent new protocols where they might already exist".

## Forks as a product

- **Guidance**, 2026-10-15. "I want to gate the acceptance of changes upstream, but still want sandbox agents to interact with the normal filesystem. So with agentfs, I can grant them a worktree forked from the latest head, they edit whatever files they choose, and then when the sandbox ends those edits become an upstream merge patch — either through a PR or some automated flow, configurable/programmable by an implementer." [Whatever receives the change does the gating.]
- **Guidance**, 2026-10-15. "We're building an open source project; my application of it to my cluster stack will come later. Focus on capabilities. Configuration with sensible defaults". [The docs name no deployment of the owner's.]
- **Guidance**, 2026-10-15. On how a fork's change is delivered: "2 a for CSI and b for the library" [the CSI driver publishes; the library delivers].
- **Guidance**, 2026-10-15. On a fork's content: "3 potentially hostile, but don't limit CI capabilities that users could create".
- **Guidance**, 2026-10-17. "A fork can be created with or without a remote link back to its parent. Configurable."
- **Guidance**, 2026-10-17. Asked where an unlinked fork pushes: "B. And if no remote is configured, the volume is lost, and that's a choice the user is allowed to make."
- **Guidance**, 2026-10-17. "Remotes can come and go. It's not a error to have zero remotes".

## Kubernetes and storage

- **Guidance**, 2026-10-15. "Write, same node is fine". On cross-node support: "It is acceptable for the final answer to be that RWX is not supported in some or all cases. I want to do the diligence to know for sure, but if the complexity is not worth the value, then we can drop it".
- **Guidance**, 2026-10-16. "use local disk for working code and an optional backend storage_class (NFS or iscsi or other) for persisting the changes and storing branches and inboxes and such".
- **Guidance**, 2026-10-16. On "one branch per worktree, for good" [moving a volume is a new volume started from the old branch; the operator picks which branch is current]: "Go".
- **Guidance**, 2026-10-16. Of a reference git server: "Something lighter. This is a minimal fit remote. Pull Requests and the like can be achieved by configuring agentfs to include GitHub (or similar) as a remote. The point of this core server is for kubernetes-native replication"; "Ideally it's optional config in the one helm chart".

## The sandbox overlay

- **Guidance**, 2026-10-30. "the sandbox agent should only get write access to the agentfs overlay files"; "-- and potentially should only be able to see the agentfs overlay files at all" [hiding is the current choice, and "potentially" stays]; "i want sandboxes to be able to edit their own configuration and have it reflected back in the main container".
- **Guidance**, 2026-10-30. His picture of the overlay: "like a real Linux overlay filesystem ... editing a file that matches the agentfs includes makes it recorded, whereas editing a file that doesn't match the includes would mean the file is created on the underlying tmp disk". [A sandbox's save temp file lives in the tmp layer.]
- **Guidance**, 2026-10-30. "Security boundary. Agentfs is the gatekeeper that exposes only what we want sandboxes to see".
