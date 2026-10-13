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
`refs/agentfs/<volume>/diverged/<UTC timestamp>` on that remote, so both
lines of history are on the remote. Recording, committing and the other
remotes carry on; only this remote's branch is stuck.

Throughout:

```sh
STATE=/data/state                                  # the state area
BACKING=/data/backing                              # the backing tree (case B)
VOL=hermes                                         # the volume
URL=https://github.com/acme/agent-history.git      # the remote's push URL
BRANCH=refs/heads/agentfs/$VOL
```

Git needs its own credentials for `$URL` here (agentfs's credential file
is not used by the `git` command): a credential helper, or a token in
the URL for this session only.

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

| What the remote's new commits look like | What happened | Resolve with |
|---|---|---|
| No `Agentfs-*` trailers, a person or another tool as committer | A foreign or mistaken writer pushed to the volume's branch | Case A |
| `Agentfs-*` trailers, but another `Agentfs-Pod` or committer replica, and `Agentfs-Seq` unrelated to ours | Two recorders use the same volume name in this repository (names must be unique, §11.4) | Give one of them a new `agentfs.volume` (it then writes its own branch), then case A for the other |
| `Agentfs-*` trailers from this volume, with `Agentfs-Seq` beyond ours | This state area is older than the remote: it was restored from a backup, or replaced | Case B |

If the foreign commits hold work you want, keep them (case A saves them)
and bring their content into the volume afterwards with a restore from
that ref using plain git ([recovery-and-undo.md](recovery-and-undo.md),
§5).

## 3. Case A: keep the volume's history, set the foreign tip aside

First save the foreign tip under `refs/agentfs/<volume>/overwritten/`, so
nothing is lost, then move the branch back to the local head with a
lease: the push fails if the branch moved again since you inspected it.
This is the one case where the branch is force-pushed, by an operator
(§15).

<!-- ci: divergence-case-a -->
```sh
TS=$(date -u +%Y%m%dT%H%M%SZ)
git --git-dir "$SCRATCH" push -q "$URL" "refs/theirs/branch:refs/agentfs/$VOL/overwritten/$TS"
git --git-dir "$STATE/git" push -q --force-with-lease="$BRANCH:$REMOTE_TIP" "$URL" "$BRANCH:$BRANCH"
```

This works while the volume is mounted: it only reads the state area.

## 4. Case B: the state area is stale

The remote holds this volume's newer history; the backing tree may or
may not match it. Make the remote's history the base and record the
tree on top of it, so nothing on either side is lost:

```sh
agentfs --state "$STATE" --root "$BACKING" flush          # workload and mount stopped
cp -a "$STATE" "$STATE.before-reseed"                     # a backup
TS=$(date -u +%Y%m%dT%H%M%SZ)
git --git-dir "$STATE/git" update-ref "refs/agentfs/$VOL/stale/$TS" "$BRANCH"
git --git-dir "$STATE/git" fetch -q "$URL" "+$BRANCH:$BRANCH"
rm -f "$STATE"/meta/push/*.json
agentfs --state "$STATE" status                           # committed through the remote's Agentfs-Seq
# Record every path in the tree or in history, so history matches the tree:
{ (cd "$BACKING" && find . -type f -o -type l | sed 's|^\./||');
  git --git-dir "$STATE/git" ls-tree -r --name-only "$BRANCH"; } |
  grep -v -e '^\.git/' -e '/\.git/' | sort -u |
  xargs -r -d '\n' agentfs --state "$STATE" --root "$BACKING" record
```

New records continue after the remote's last `Agentfs-Seq`, and the old
local head stays under `refs/agentfs/<volume>/stale/` in the state area.
Case B is not exercised in CI; rehearse it on a copy first.

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
keep volume names unique per repository.
