# Runbook: a fork whose publish is refused

A fork publishes its change by pushing its history, then
`refs/agentfs/<fork>/change` and `refs/agentfs/<fork>/finish`
create-only, to its fork remote (design
[§11.5](../design.md#115-forks)). Most failures (an unreachable remote,
bad credentials, a busy remote) are retried with backoff and need no
runbook: fix the remote and the next attempt publishes. A few mean no
retry can ever succeed. One function, `fork::finish::refused_reason`,
decides which; those make the fork **refused**, and agentfs stops
retrying it until an operator acts.

Only a **linked** fork (the default, `agentfs.fork.link` unset) publishes,
so only a linked one can be refused. A linked fork that cannot reach its
source waits and alarms; it never delivers anywhere else, so the fixes
below are always on the fork remote (or the source's state area), never
a different target. An unlinked fork delivers nothing: `fork finish`
refuses it (`fork <fork> is unlinked`) and `fork status` has no state for
it. It pushes only to its own remotes (`agentfs.fork.remotes`), never to
the source's, and those pushes are an ordinary volume's
([stuck-pushes.md](stuck-pushes.md)); with none it is local-only and
pushes nothing ([design §11.5](../design.md#115-forks), "The link").

## How it shows

- The alarm `ForkRefused`, raised once when the refusal is recorded,
  with the fork, the fork remote, the reason and the remote's message
  (a log record `agentfs.fork_refused` when telemetry is on, `alarm:` on
  stderr otherwise). The `agentfs.fork.publish` span ends with
  `agentfs.fork.publish.outcome = refused` and
  `agentfs.fork.refused.reason`.
- `agentfs fork finish` exits 1 with `publishing is refused (<reason>)`
  and a link to the section below for that reason.
- `agentfs fork status` (no lock, no network) says `state: refused` and,
  on its `publish:` line, the reason, when, which remote and the message;
  `--json` has them under `refused`.
- Under CSI: the deleted claim's PV stays `Released`, DeleteVolume says
  `volume <pv>: the fork is refused`, and the node plugin's release scan
  starts no further finisher for the fork.

Look at the fork first. Outside Kubernetes:

```sh
agentfs --state "$STATE" fork status
```

Under CSI, in the node plugin pod on the fork's node (see
[csi-recovery.md](csi-recovery.md#a-fork-that-will-not-delete) for
finding `$NODE_POD` and `$PV`):

```sh
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
  agentfs --state /var/lib/agentfs-csi/volumes/$PV/state fork status
```

Then find the reason below, and take one of [the two exits](#the-two-exits).

## `another_writer`

**What happened.** `refs/agentfs/<fork>/change` or
`refs/agentfs/<fork>/finish` on the fork remote already holds another
commit. agentfs creates these refs only if they are absent and never
moves one, so a ref someone else wrote would otherwise be overwritten.
A fork name is unique at create (`{pvName}` under CSI, a random suffix
from the CLI), so the usual causes are a person or a tool writing under
`refs/agentfs/`, or a fork remote restored from a backup that holds an
old fork of the same name.

**Fix.** Find what the remote holds and who wrote it:

```sh
git ls-remote "$FORK_REMOTE_URL" "refs/agentfs/$FORK/*"
```

If it is not this fork's change (compare with `change:` and `finish:` in
`fork status`), delete it on the remote, with the remote's own
credentials:

```sh
git push "$FORK_REMOTE_URL" --delete "refs/agentfs/$FORK/finish"
```

If it is something a receiver needs, keep it and abandon this fork
instead.

## `too_large`

**What happened.** One commit of the fork's history is larger than the
fork remote's `agentfsMaxPushSize`, or one LFS object is larger than its
`agentfsMaxLfsObjectSize` ([configuration](../configuration.md#push-size),
design §11.3). A single commit is never split, so no retry can send it.

**Fix.** Raise the limit on the fork remote's section in the source's
settings (under CSI, the profile's config), if the host takes pushes
that large. The fork reads its settings again at every attempt.

## `rejected`

**What happened.** The fork remote refused the create-only push of the
change and finish refs: a pre-receive hook, a branch or tag protection
rule, or a forge permission that does not let agentfs's credentials
create `refs/agentfs/*`. The history push itself may have succeeded.

**Fix.** Allow the identity in the fork remote's credentials to create
refs under `refs/agentfs/`. The remote's message, in `fork status` and
the alarm, usually names the rule.

## `remote_gone`

**What happened.** The finish commit names the fork remote it was
finished for (`Agentfs-Fork-Remote`), and the fork's settings no longer
have that remote as a valid fork remote: the section was removed or
renamed, a second section was flagged `agentfsForkRemote`, or the
section is now invalid (`RemoteMisconfigured` says why). A published
fork is judged against the remote its finish names, so agentfs never
publishes it elsewhere.

**Fix.** Restore the remote, under the same name, in the source's
settings files (under CSI, the profile's config).

## The two exits

**1. Fix the cause, then retry.** `--retry` clears the refusal and
publishes again; without it, `fork finish` returns the recorded refusal
and pushes nothing. Outside Kubernetes:

```sh
agentfs --state "$STATE" --root "$BACKING" fork finish --retry --timeout 2m
```

Under CSI (`--wait-lock 6m` waits out the release scan's own finisher):

```sh
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
  agentfs --wait-lock 6m --state /var/lib/agentfs-csi/volumes/$PV/state \
    --root /var/lib/agentfs-csi/volumes/$PV/backing fork finish --retry --timeout 2m
```

Exit 0 is published (under CSI, the next DeleteVolume completes); 1 is
refused again (with the reason) or not published by the timeout; 75 is
another process holding the fork.

**2. Give the fork up.** Abandoning records the reason, the time and
your uid; nothing agentfs runs pushes the fork afterwards, and under CSI
the next DeleteVolume applies the class's `onDelete`, discarding the
change:

```sh
agentfs --state "$STATE" fork abandon --reason "<why>"
```

Under CSI:

```sh
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
  agentfs --wait-lock 6m --state /var/lib/agentfs-csi/volumes/$PV/state \
    fork abandon --reason "<why>"
```

Abandon is refused for a published fork, which has nothing to discard.
