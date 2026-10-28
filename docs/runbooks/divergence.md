# Runbook: a diverged remote

Someone else moved the volume's branch on a remote, so agentfs can no
longer push to it without overwriting their commits, which it never
does. Design: [§11.3](../design.md#113-limits-and-rejected-pushes),
[§11.4](../design.md#114-shared-repositories). `ci/e2e-runbooks.sh` runs
case A below, block for block, in CI.

## Symptoms

- The `Diverged` alarm (log event `agentfs.diverged`), once, naming the
  saved ref; then `PushFailed` with kind `non_fast_forward` at every
  attempt, with the usual backoff.
- The gauge `agentfs.push.diverged{remote}` is 1, and
  `agentfs.push.oldest_unpushed_age` keeps growing.
- `agentfs status`:
  `remote: origin ...: last pushed ..., diverged: head saved as refs/agentfs/<volume>/diverged/20261012T110302Z`.

When it happened, agentfs pushed its own head to
`refs/agentfs/<volume>/diverged/<UTC timestamp>` on that remote, and it
moves that ref forward with every commit it records afterwards, so both
lines of history stay on the remote, the volume's newest work included.
If someone else moves the saved ref too, agentfs leaves it and the
`PushFailed` message says so. Recording, committing and the other remotes
carry on; only this remote's branch is stuck, and the patch queue's refs
on it wait too (the queue push writes nothing while the branch is not
this volume's, §11.3).

Throughout:

```sh
STATE=/data/state                                  # the state area
VOL=hermes                                         # the volume
NAME=origin                                        # the diverged remote's name
URL=https://github.com/acme/agent-history.git      # its push URL
BRANCH=refs/heads/agentfs/$VOL
```

Git needs its own credentials for `$URL` here (agentfs's credential file
is not used by the `git` command): a credential helper, or a token in
the URL for this session only. `agentfs push` uses the volume's own.

For an SSH remote, point `git` at a key with write access and at the
remote's own known_hosts file, so this session trusts exactly the host
agentfs trusts (`ci/e2e-ssh.sh` pushes as another writer this way):

```sh
URL=ssh://git@git.example.com/acme/agent-history.git
export GIT_SSH_COMMAND="ssh -i /path/to/operator-key -o IdentitiesOnly=yes \
  -o UserKnownHostsFile=/run/secrets/agentfs/origin-ssh/known_hosts \
  -o StrictHostKeyChecking=yes"
```

## 1. Inspect

Fetch both lines into a scratch repository; nothing here changes the
state area or the remote.

<!-- ci: divergence-inspect -->
```sh
SCRATCH=$(mktemp -d)
git init -q --bare "$SCRATCH"
git --git-dir "$SCRATCH" fetch -q "$URL" "+$BRANCH:refs/theirs/branch" "+refs/agentfs/$VOL/diverged/*:refs/diverged/*"
git --git-dir "$SCRATCH" fetch -q "$STATE/git" "+$BRANCH:refs/ours/branch"
REMOTE_TIP=$(git --git-dir "$SCRATCH" rev-parse refs/theirs/branch)
git --git-dir "$SCRATCH" for-each-ref refs/diverged
# What the other writer added, with authors and committers:
git --git-dir "$SCRATCH" log --format='%h %an <%ae> / %cn: %s' refs/ours/branch..refs/theirs/branch
# Their newest commit in full: agentfs commits carry Agentfs-* trailers.
git --git-dir "$SCRATCH" log -1 --format=%B refs/theirs/branch
```

## 2. Decide who wrote it

Every worktree writes one branch, its own, for good
([§19.1](../design.md#191-decisions-2026-10-12), "one branch per
worktree"): a branch another worktree writes is never taken over.

| What the remote's new commits look like | What happened | Resolve with |
|---|---|---|
| No `Agentfs-*` trailers, a person or another tool as committer | A foreign or mistaken writer pushed to the volume's branch | Case A |
| `Agentfs-*` trailers, but another `Agentfs-Pod` or committer replica, and `Agentfs-Seq` unrelated to ours | Another worktree writes this branch: two recorders use the same volume name in this repository (names must be unique, §11.4) | Rename this volume: it continues under a new name (under CSI, a new claim: its PV name is the volume name; a CLI or sidecar volume, a new state area with its own `--volume`). The other worktree keeps the branch. This volume's head stays saved under its `diverged/*` ref: take what you need from there with a restore ([recovery-and-undo.md](recovery-and-undo.md), §5) |
| `Agentfs-*` trailers from this volume, with `Agentfs-Seq` beyond ours | This state area is older than the remote: it was restored from a backup, or replaced | Case B |

If the foreign commits hold work you want, keep them (case A saves them)
and bring their content into the volume afterwards with a restore from
that ref using plain git ([recovery-and-undo.md](recovery-and-undo.md),
§5).

## 3. Case A: keep the volume's history, set the foreign tip aside

First save the foreign tip under `refs/agentfs/<volume>/overwritten/`, so
nothing is lost. Then put the branch back at the volume's head with
`agentfs push --force-with-lease`, leased on the tip you inspected: it
replaces only this volume's own branch, on one remote, and only if that
remote still holds the lease (it fetches it aside if the state area lacks
it), the lease shares a commit with the head, and none of the commits it
drops was made by an agentfs recorder (an `Agentfs-Seq` trailer) unless
the head holds it rewritten. This is the one case where the branch is
replaced, by an operator (§15).

<!-- ci: divergence-case-a -->
```sh
TS=$(date -u +%Y%m%dT%H%M%SZ)
git --git-dir "$SCRATCH" push -q "$URL" "refs/theirs/branch:refs/agentfs/$VOL/overwritten/$TS"
agentfs --state "$STATE" push --remote "$NAME" --force-with-lease "$REMOTE_TIP"
```

It prints `<remote>: replaced <lease> with <head>` and exits 0. Exit 1
with `lease broken` means the branch moved again since you inspected it
(inspect again); `shares no commit` means the remote's branch is another
history, which this command never replaces (see the table: rename this
volume); `which carries agentfs trailers` names a commit with an
`Agentfs-Seq` trailer that this state area lacks, which is never
replaced. It is one of:

- case B or another worktree's branch (see the table): continue under a
  new name;
- this volume's own commits that a person amended, rebased,
  cherry-picked or squashed onto the branch (a squash-merge whose message
  keeps the trailers counts too): they carry the trailers but are new
  commits. Either continue under a new name, or have whoever wrote them
  take those commits off the branch from their own clone (leaving the
  volume's commits and their own foreign ones), then inspect again
  (step 1) and repeat case A with the new tip.

This works while the volume is mounted: it never goes through
the daemon and takes only that remote's push lock (exit 75 while a push
to it runs: retry).

## 4. Case B: the state area is stale

The remote holds this volume's newer history; this state area, restored
from a backup or replaced, must not write the branch again
(`agentfs push --force-with-lease` refuses it: `which carries agentfs
trailers`). Continue under
a new name, started from the branch, so nothing on either side is lost:

1. Stop the workload and the stale volume, and keep its state area as it
   is: its head is saved on the remote under `diverged/*` already.
2. Under CSI, follow [moving.md](moving.md) from its step 2: a start class
   with `startFrom: <remote>:agentfs/<volume>@<the branch's tip>` (a
   branch, never a `diverged/*` ref: that is refused), a new claim on it,
   and readers pointed at the new branch.
3. A CLI or sidecar volume has no start command yet (§19.2): create a new
   volume (`agentfs init --volume <new name>`) whose backing tree holds the
   branch's tip (`git --git-dir "$STATE/git" fetch -q "$URL" "$BRANCH"`,
   then `git archive` of that commit into it), and record it.

What the stale state area held beyond the remote is on its `diverged/*`
ref: restore what you need from there in the new volume
([recovery-and-undo.md](recovery-and-undo.md), §5). Case B is not
exercised in CI; rehearse it on a copy first.

## 5. Verify

The next push of that remote is a fast-forward (or up to date), and the
divergence clears. Push now: `agentfs push` pushes directly with no mount running,
and asks the running mount (or `run`) to push at once otherwise.

<!-- ci: divergence-verify -->
```sh
agentfs --state "$STATE" push
agentfs --state "$STATE" status
```

`status` must no longer say `diverged` for that remote, and the gauge
`agentfs.push.diverged` returns to 0.

## 6. Clean up

The diverged refs were only a safety copy of the local head, which the
branch now holds again. Delete them from the remote (keep the
`overwritten/` ref as long as you need the foreign commits):

<!-- ci: divergence-cleanup -->
```sh
for ref in $(git --git-dir "$SCRATCH" for-each-ref --format='%(refname:lstrip=2)' refs/diverged); do
  git --git-dir "$SCRATCH" push -q "$URL" --delete "refs/agentfs/$VOL/diverged/$ref"
done
rm -rf "$SCRATCH"
```

To keep it from happening again: restrict who can push the
`agentfs/*` branches on the remote where the host allows it (§11.4), and
keep volume names unique per repository. Runbooks never `git push` a
branch: a test (`runbooks_push_only_the_namespace`) holds every `git push`
in them to refs under `refs/agentfs/`, without a force.
