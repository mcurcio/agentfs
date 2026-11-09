# Forked volumes

A fork is a volume started from another volume's latest commit, its
**source**, at its **base**. A sandbox edits it like any directory. A fork
records, pushes and recovers like any volume. When it **finishes**, it
publishes one **change**: a commit whose parent is the base and whose tree
is the fork's final tree. Whatever receives the change decides what to do
with it. agentfs never reviews, accepts or merges a fork's change.

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

Under CSI, a claim with a `dataSource` claim on a fork StorageClass
(`fork: "true"`, reclaim policy `Delete`) is a fork. Deleting it finishes
the fork, and the driver deletes the volume only once it is published
([csi.md](csi.md#14-forks)).

## Where a fork publishes

- **The fork remote.** A fork pushes `refs/heads/agentfs/<fork>`, and on
  finish `refs/agentfs/<fork>/change` and `refs/agentfs/<fork>/finish`, to
  the fork remote: the one remote section with `agentfsForkRemote = true`,
  else the source's patch remote. A flagged section is never one of the
  source's own push remotes, and a source never pushes to it. Two flagged
  sections, or an invalid one, leave no fork remote: forks are refused
  rather than fall back. While the source sets `lfs.url` or `lfs.pushurl`,
  a flagged section must name its own `lfsurl` or `lfspushurl`, so a fork's
  LFS objects never reach the source's store.
- **Hidden history.** With `agentfs.forkRefs = hidden` in the source's
  settings, new forks keep their history at `refs/agentfs/<fork>/history`
  instead, which is never under `refs/heads/` on any remote
  ([configuration](configuration.md#agentfs)). Create gives a new fork the
  source's setting; changing it later moves no fork that exists.
  Whether a forge runs CI or webhooks on a push to `refs/agentfs/*` is
  that forge's own behaviour and is not verified here: check each forge
  before relying on `hidden` to keep a sandbox's output away from its CI.
- **A local-only source.** A source with no remote section and no flagged
  section has no remote to publish to. Its forks publish the same refs into
  the source's own history, in its state area on the same disk, through the
  source's running daemon or under its lock (`fork finish` exits 75 while
  another command holds it). The fork's objects and LFS objects are copied
  across, and the refs are written only after each is judged. Nothing
  applies them: whatever reads the source's history decides
  (`git --git-dir <source state>/git log agentfs/<fork>`;
  [configuration](configuration.md#remotes-modes)). Once the source gains a
  remote, its pushes carry those refs there too, create-only, and a remote
  holding one at another commit fails that push; it is never overwritten.
  The source must stay on its node at its path until the fork is published.

  The fork's output is untrusted, so receiving refuses, writing no ref
  (`unsafe`): a ref that is the source's own (its branch, or one under
  `refs/agentfs/<source>/`, compared ignoring case); a base that is not on
  the source's branch; and an LFS object whose content is not its name.

## The link

A fork is `linked` by default: its finish delivers the change back to the
source as above. While it cannot reach the source it waits and alarms, and
never delivers anywhere else.

`fork create --link unlinked` (or `link: unlinked` in a fork StorageClass)
makes an independent fork instead. Nothing is ever delivered to the
source, and it never pushes to the source's remotes, its fork remote
included. It pushes its own branch only to remotes of its own: `fork create
--link unlinked --remotes FILE` (a config file of `[remote "<name>"]`
sections, plus `lfs.url` and `lfs.pushurl` if needed), or a fork class's
`remotes: <profile>`. Without them it is local-only, and deleting it
deletes its history. That is the user's choice. An unlinked fork is never
finished (`fork finish` refuses it) and records no path to a local-only
source. `--remotes` is refused for a linked fork. The link and the remotes
file's path are fixed at create.

## Create, finish and publish

- **Create** (`fork create`) reads the source's settings and branch head
  without its lock and with no credentials. It refuses a source with no
  history, a source with no fork remote, and a source whose own forks could
  not roll up into it (an unlinked fork, one whose history is hidden, or a
  fork of a local-only source). A CLI source must keep its settings in
  included files, because the fork reads only those; a CSI source's are
  its profile's. Create packs every object reachable from the base into the
  fork's own object store, so a fork outlives its source, and writes the
  base's tree into the backing tree. Each step is idempotent, so a retry
  resumes.
- **Finish** commits what is journaled, records the paths every repair
  queue left, and writes the change (none when the final tree is the
  base's) with the `change` and `finish` refs. A second finish returns the
  same record, and a crash between any two writes converges. With no fork
  remote, finish writes nothing and the fork stays active.
- **Publish** pushes the fork's history to the fork remote, then the
  `change` and `finish` refs **create-only**: never forced, and a ref the
  remote holds at another id is another writer's and is never moved. A
  listing after the push confirms both refs. Failures that can clear
  (unreachable, bad credentials, a busy remote, a moved branch) are
  retried with backoff.
- **Read-only after finish.** A fork whose branch head is its finish commit
  takes no changes: `mount`, `run`, `record`, `restore`, `revert` and
  `patch apply` refuse it. Under CSI its fronts mount it read-only, so the
  kernel answers every write with `EROFS`.
- **Status** (`fork status [--json]`, no lock and no network) is `active`
  before the finish commit, then `finished`, then `published` or `refused`,
  or `abandoned`. A fork published as a top-level fork because its parent
  had finished shows that parent.

A fork whose included settings file cannot be read does not open, and
names the file. A fork with no valid fork remote opens, since recording
keeps working, raises `RemoteMisconfigured`, and `fork finish` writes
nothing.

## Refusal

Some failures mean no retry can succeed: a create-only ref the remote holds
at another id (`another_writer`), the remote refusing the create-only push
(`rejected`), one commit over `agentfsMaxPushSize` or one LFS object over
`agentfsMaxLfsObjectSize` (`too_large`), a finish naming a remote that is no
longer the fork's valid fork remote (`remote_gone`), and a local-only source
refusing what the fork brings (`unsafe`). The fork is then `refused`, the
alarm `ForkRefused` is raised once, and nothing agentfs runs retries it. A
fork remote that refuses for good leaves the fork refused until `fork
finish --retry`. `fork abandon --reason TEXT` discards a fork on record; it
is refused for a published fork, and the fork's interval pushes carry on if
it is mounted again. What each refusal means and how to clear it:
[runbooks/fork-refused.md](runbooks/fork-refused.md). Removing a recorded
secret from a source and its forks:
[runbooks/history-rewrite.md](runbooks/history-rewrite.md#forks).

## Listing and delivering a change

`agentfs fork list` runs in a receiver's repository with its own git and
credentials and never reads a state area. A change is listed only if its
fork name is a valid volume name, its finish commit names the same fork
and change, and the change has exactly one parent, the base, and the
finish commit's tree. Anything else is skipped with the reason. A listing
is only as trustworthy as the remote's writers: whoever may write
`refs/agentfs/*` there can publish a change that passes.

`agentfs fork deliver` hands one published change, checked as `fork list`
checks it, to a receiver by one method, from the receiver's own clone and
credentials. The CSI driver never delivers: a receiver runs this.

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

| Method | Delivers | Repeating it |
|---|---|---|
| `inbox` | the change commit into the inbox of the named volume, on `--remote` | at most once: a commit already queued or applied is "already delivered" |
| `branch` | a pull request's head, `refs/heads/<prefix><fork>` (default prefix `agentfs-review/`), created only if absent, never forced. Squashed onto the base, onto `--onto B`, or the fork's history as published | the ref already at exactly the commit this delivery creates is "already delivered"; a ref at any other commit is refused and never moved |
| `command` | the program, run without a shell with the payload as JSON on stdin, an environment of `PATH` alone, killed after `--timeout` (default 60s) | at least once: exit 0 accepted, 75 unavailable, anything else refused |
| `webhook` | one POST of the payload with `Idempotency-Key: <change>` and, with `--credential-file`, a bearer token; no redirect followed, no proxy from the environment | at least once: 2xx accepted, 409 already delivered, 429, 5xx and timeouts unavailable, other statuses refused |

It exits 0 when delivered or already delivered, 1 when refused, and 75
when the receiver is unavailable: run it again. The payload (schema 1)
names the fork, source, base, change and finish commits and where to
fetch them; receivers dedupe on `change`, which makes the at-least-once
methods at most once.

Delivering pushes a sandbox's unreviewed output into the target
repository, where its own CI and hooks act on it. The `branch` method only
creates refs under its prefix and the `inbox` method only queues, but what
runs on a new branch is the repository's affair.

## Forks of forks

A linked fork of a CLI or CSI source that has a fork remote can itself be
forked. Its changes roll up one level through the parent's patch queue
([patches](patches.md#forks-of-forks)); a parent that has finished cannot
take it, so the child publishes as a top-level fork.
