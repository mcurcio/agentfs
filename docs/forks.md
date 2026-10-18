# Forked volumes

A fork is a volume started from another volume's latest commit: a
sandbox edits it like any directory, and when it ends the fork publishes
one change, a commit on the base it started from, for whatever receives
it to review and land. agentfs never accepts or merges
([design §11.5](design.md#115-forks)).

```sh
# A fork of the volume at ./state, in ./fork with its tree in ./fork-tree:
agentfs --state ./fork --root ./fork-tree fork create --source-state ./state
# ... the sandbox works in ./fork-tree; record or mount it as usual ...
agentfs --state ./fork --root ./fork-tree fork finish    # finish once, publish
agentfs --state ./fork fork status                         # state and publish result
# A receiver lists the published changes, each checked, from its own clone:
agentfs fork list --remote origin --source src
# and lands the exact change it reviewed in the source:
agentfs patch submit --volume src <change commit>
```

- Forks push `refs/heads/agentfs/<fork>`, and on finish
  `refs/agentfs/<fork>/change` and `/finish`, to the fork remote: the one
  remote section with `agentfsForkRemote = true`, else the source's patch
  remote. A source never pushes to a flagged remote. With
  `agentfs.forkRefs = hidden` in the source's settings, new forks keep
  their history at `refs/agentfs/<fork>/history` instead, off every
  branch ([docs/configuration.md](configuration.md#agentfs)).
- A source declared local-only (`agentfs.remotes = none`, no flagged
  section) has no remote to publish to: its forks publish the same refs
  into the source's own history, in its state area on the same disk,
  through the source's running daemon or under its lock (`fork finish`
  exits 75 while another command holds it). Nothing applies them:
  whatever reads the source's history decides (`git --git-dir <source
  state>/git log agentfs/<fork>`;
  [configuration](configuration.md#declaring-remotes)). Once the source
  declares a remote, its pushes carry those refs there too, create-only.
- A fork is `linked` by default: its finish delivers the change back to
  the source as above, and while it cannot reach the source it waits and
  alarms, never delivering anywhere else. `fork create --link unlinked`
  (or `link: unlinked` in a fork StorageClass) makes an independent fork
  instead: it pushes its own branch where a linked fork would publish,
  and nothing is ever delivered to the source. It is never finished
  (`fork finish` refuses it), records no path to a local-only source and
  is then local-only itself. The link is fixed at create
  ([design §11.5](design.md#115-forks), "The link").
- Under CSI, a claim with a `dataSource` claim on a fork StorageClass
  (`fork: "true"`, reclaim policy `Delete`) is a fork; deleting it
  finishes the fork, and the driver deletes the volume only once it is
  published ([docs/csi.md](csi.md)).
- A finished fork is read-only. A fork remote that refuses for good
  leaves the fork `refused` until `fork finish --retry`; `fork abandon
  --reason TEXT` discards a fork on record, and nothing agentfs runs
  pushes it again. What each refusal means and how to clear it:
  [runbooks/fork-refused.md](runbooks/fork-refused.md). Removing a
  recorded secret from a source and its forks:
  [runbooks/history-rewrite.md](runbooks/history-rewrite.md#forks).

## Delivering a change

`agentfs fork deliver` hands one published change, checked as `fork
list` checks it, to a receiver by one method, from the receiver's own
clone and credentials ([design §11.5](design.md#115-forks),
"Delivery"). The CSI driver never delivers: a receiver runs this.

```sh
# Queue it in a volume's inbox (at most once; applied at its next mount):
agentfs fork deliver src-1a2b3c4d --to inbox --volume src
# Open a pull request's head, agentfs-review/<fork>, one squashed commit
# on base, or merged onto a branch's head:
agentfs fork deliver src-1a2b3c4d --to branch --onto main
# Run a program (no shell) with the JSON payload on stdin:
agentfs fork deliver src-1a2b3c4d --to command -- ./review.sh
# POST the payload, with Idempotency-Key: <change>:
agentfs fork deliver src-1a2b3c4d --to webhook --url https://ci.example.com/hook \
  --credential-file /run/secrets/hook-token
```

It exits 0 when delivered or already delivered, 1 when refused, and 75
when the receiver is unavailable: run it again. The payload (schema 1)
names the fork, source, base, change and finish commits and where to
fetch them; receivers dedupe on `change`. Delivering pushes a sandbox's
unreviewed output into the target repository, where its own CI acts on
it.
