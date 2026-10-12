# Runbook: the patch queue

Producers outside a volume (iac, an operator, CI) change its tree through
history: they queue patches on the volume's remote, and agentfs applies
them, in order, the next time the volume mounts, before anything can see
the tree. Design: [§12](../design.md#12-patch-queue-for-iac-phase-3-built),
[§11.3](../design.md#113-limits-and-rejected-pushes). `ci/e2e-patches.sh`
follows this runbook against a smart-HTTP remote and a real mount.

Throughout:

```sh
STATE=/data/state                                  # the state area
BACKING=/data/backing                              # the backing tree
VOL=hermes                                         # the volume
BRANCH=agentfs/$VOL                                # the volume's branch
```

## What a patch is

- **One commit with exactly one parent**, the branch commit it was written
  against: the head, or a first-parent ancestor of it (agentfs merges it
  onto the head as `git cherry-pick` would).
- At most 4096 changed paths, regular files and symlinks only (no
  submodules), and only paths agentfs records (no `.git` components).
- Pushed to `refs/agentfs/<volume>/inbox/<n>` on the volume's **patch
  remote** (below), where `<n>` is ten zero-padded digits from
  `0000000001`. Numbers are applied in order; a missing number blocks
  the ones after it.

The commit agentfs makes for it has the patch's author and message,
agentfs's committer identity, and the trailers:

```text
Agentfs-Trigger: patch
Agentfs-Patch: <patch commit>
Agentfs-Inbox: <n>
```

## 1. Submit a patch

### With `agentfs patch submit`

Run it in the producer's own git repository: it drives the producer's
`git`, so that repository's remotes, credential helpers and SSH setup
apply, and agentfs needs none of the volume's credentials. The repository
needs a remote that points at the volume's patch remote (`--remote`,
default `origin`) and a `user.name` and `user.email` for built patches.

```sh
# Build a patch on the branch head from files.
agentfs patch submit --volume "$VOL" --put config/app.yaml=./app.yaml -m "Raise the limit"
agentfs patch submit --volume "$VOL" --delete old/notes.md -m "Drop old notes"

# Or queue a commit you made (its parent must be on the volume's branch).
git fetch origin "$BRANCH"
git switch -c change FETCH_HEAD
# ... edit, git commit ...
agentfs patch submit --volume "$VOL" HEAD
```

It prints `queued patch <n> as refs/agentfs/<volume>/inbox/<n> (<oid>,
parent <commit>)`. Options:

| Option | Meaning |
|---|---|
| `--volume V` | The volume (or `AGENTFS_VOLUME`) |
| `--remote R` | The remote name or URL (default `origin`) |
| `--branch B` | The volume's branch (default `agentfs/<volume>`) |
| `--repo DIR` | The producer's repository (default `.`) |
| `--put PATH=FILE`, `--delete PATH` | Build the patch on the remote head; repeatable |
| `-m MSG`, `--author 'Name <email>'` | Message and author of a built patch |
| `--replace N` | Replace queued patch N (it must still be the commit listed) |
| `--skip N` | Replace queued patch N with an empty patch, so the queue moves on |
| `--depth D` | Fetch only D commits (shallow producers) |
| `--attempts K` | Numbers to try when other submitters race (default 10) |

### With plain git

The same protocol, by hand:

```sh
URL=https://git.example.com/acme/agent-history.git
# 1. The next number: one past the highest inbox or applied number.
git ls-remote "$URL" "refs/heads/$BRANCH" "refs/agentfs/$VOL/inbox/*" "refs/agentfs/$VOL/applied/*"
N=$(git ls-remote "$URL" "refs/agentfs/$VOL/inbox/*" "refs/agentfs/$VOL/applied/*" |
  sed 's#.*/##' | sort | tail -n1)
REF=$(printf 'refs/agentfs/%s/inbox/%010d' "$VOL" $((10#${N:-0} + 1)))
# 2. A commit whose parent is on the branch (here: the current HEAD).
OID=$(git rev-parse HEAD)
# 3. Create the ref only if nobody has it (an empty lease means "must not exist").
git push "$URL" --force-with-lease="$REF:" "$OID:$REF"
```

If the push is rejected (`stale info`), another producer took that number:
list again and try the next one. Never push to a number that exists
unless you mean to replace that patch (lease on its current oid:
`--force-with-lease=$REF:<oid listed>`), and never reuse a number below
the highest applied one: it is ignored as `stale`.

## 2. When patches apply

At every mount, after journal replay and before the workload can see the
tree. In Kubernetes that means a **pod restart** (`kubectl delete pod`, or
a rollout). With the volume stopped, `agentfs patch apply` does the same:

```sh
agentfs --state "$STATE" --root "$BACKING" patch apply   # exit 1 when blocked, 75 when busy
```

The mount first fetches `inbox/*` from the patch remote, waiting at most
`agentfs.patchFetchTimeout` (default `30s`). If that fails, the alarm
`PatchFetchFailed` is raised and the patches already fetched apply. The
mount log says what happened:

```text
patch 3 applied as 6f1c...
patches: through 3 applied, none queued
```

After the next push to the patch remote, `refs/agentfs/<volume>/applied/<n>`
holds each applied patch and its `inbox/<n>` is gone.

## 3. Check the queue

```sh
agentfs --state "$STATE" status          # patches:   last applied 3, 2 queued, blocked at 4 (conflict) (from origin)
agentfs --state "$STATE" patch status    # the whole status; --json for scripts
```

From anywhere with read access to the remote:

```sh
git fetch "$URL" "refs/agentfs/$VOL/status"
git show FETCH_HEAD:status.json
```

`status.json` (version 1) is rewritten at every mount and force-pushed:

| Field | Meaning |
|---|---|
| `version` | 1 |
| `volume`, `branch`, `head` | The volume, its branch and the head after the run |
| `updated` | When the run ended (RFC 3339, UTC) |
| `last_applied` | `{inbox, patch, commit}` of the highest applied patch, or `null` |
| `applied_now` | The numbers this run applied |
| `queue` | `[{inbox, patch}]` still queued, the blocked one first |
| `blocked` | `{inbox, patch, reason, message, paths}`, or `null` |
| `ignored` | `[{ref, inbox, patch, why}]`: inbox refs that are `malformed` (not ten digits) or `stale` (at or below the last applied number, with another commit) |
| `fetch` | `{remote, ok, error}` of this run's fetch, or `null` when none ran |

Telemetry: `agentfs.patch.queue_depth`, `agentfs.patch.blocked` (alert on
it being 1), the span `agentfs.patch`, and the `PatchBlocked` and
`PatchFetchFailed` alarms.

## 4. A blocked queue

The first patch that cannot be applied blocks the queue: it and every
later patch stay queued, `PatchBlocked` is raised (`patch queue blocked at
<n> (<oid>, <reason>): ...`), and **the volume mounts anyway** with the
patches before it applied. Nothing of the blocked patch is written.

| Reason | Cause | Fix |
|---|---|---|
| `conflict` | The patch conflicts with changes on the branch since its parent; `paths` lists them | Rebase it onto the current head and `--replace N` |
| `gap` | Number N is missing while later ones are queued | Submit N, or `--skip N` |
| `parent_not_in_history` | Its parent is not on the volume's branch | Rebase onto the branch and `--replace N` |
| `merge_commit` | More than one parent | Squash it and `--replace N` |
| `unsupported` | A root commit, a submodule, or a path agentfs does not record | Remove the path and `--replace N`, or `--skip N` |
| `too_large` | More than 4096 paths | Split it: `--replace N` with the first part, then submit the rest |
| `unrecorded` | The tree holds changes history does not have yet where the patch writes | Mount without the patch (it blocks, the mount records the change), then restart again |
| `failed` | Writing or recording failed, or an LFS object could not be found | Read the message; fix the state area or the LFS server, restart |

To rebase a conflicting patch:

```sh
git fetch origin "$BRANCH"
git switch -c fix FETCH_HEAD
git cherry-pick <patch oid>          # resolve, git cherry-pick --continue
agentfs patch submit --volume "$VOL" --replace 4 HEAD
```

or, for a built patch, submit the new content on the head:

```sh
agentfs patch submit --volume "$VOL" --replace 4 --put config/app.yaml=./app.yaml -m "Raise the limit, rebased"
```

To give up on a patch: `agentfs patch submit --volume "$VOL" --skip 4`
(an empty patch takes its place; the commit it leaves on the branch says
so). Then restart the pod, or run `agentfs patch apply`.

## 5. Crashes

Whether patch N is applied is decided by the branch, from the trailers
above, never by refs or `meta/patch.json`. The refs move only after the
commit, so a crash at any point applies each patch exactly once: a
recorded but uncommitted patch is committed by journal replay, and a
committed patch whose refs did not move is recognised and its refs moved
(`patch(es) N were already applied; their refs are moved`). Nothing to do
by hand.

## 6. Configuration

| Key | Default | Meaning |
|---|---|---|
| `agentfs.patchRemote` | the only remote, if exactly one | The remote whose `inbox/*` is fetched and which gets the queue push; `none` turns both off (patches already fetched still apply) |
| `agentfs.patchFetchTimeout` | `30s` | How long a mount waits for the fetch; `0` or `off` skips it |

The queue push (status ref, `applied/<n>`, deleting `inbox/<n>`) runs after
each branch push to the patch remote; `agentfs push` prints it as
`<remote>: queue: ...`. Its failures are `PushFailed` with stage `queue`
and never count against the branch.
