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

- **A branch**: a line of one or more commits, each with exactly one
  parent, on a **base**, the last commit of the volume's branch it builds
  on (the merge base of its tip and the branch head: the head, or any
  ancestor of it). agentfs merges it onto the head with
  `git merge-tree --write-tree --merge-base=<base>`, a three-way merge
  against the base, and lands it as **one** commit. A one-commit patch is
  a branch of one. Only the operator's `$GIT_DIR/info/attributes` steers
  the merge, never the volume's own `.gitattributes`. The merge needs git
  2.40 or later in the state area's environment.
- The producer's commits are not on the volume's branch: they stay
  reachable at `refs/agentfs/<volume>/applied/<n>`, which names the tip.
- At most 4096 paths changed from base to tip, regular files and symlinks only (no
  submodules), and only paths agentfs records (no `.git` components).
- Pushed to `refs/agentfs/<volume>/inbox/<n>` on the volume's **patch
  remote** (below), where `<n>` is ten zero-padded digits from
  `0000000001`. Numbers are judged in order; a missing number stops the
  ones after it. A patch that cannot land is set aside whole and the
  others go on ([section 4](#4-a-patch-set-aside)).

The commit agentfs makes for it has the tip's author and message (for a
branch of more than one commit, followed by one line per commit, oldest
first: `<short oid> <subject>`), agentfs's committer identity, and the
trailers:

```text
Agentfs-Trigger: patch
Agentfs-Patch: <the tip>
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

# Or queue a branch you made: its commits since the volume's branch are
# the patch (no merge commits among them).
git fetch origin "$BRANCH"
git switch -c change FETCH_HEAD
# ... edit, git commit, as many times as you like ...
agentfs patch submit --volume "$VOL" HEAD
```

It prints `queued patch <n> as refs/agentfs/<volume>/inbox/<n> (<tip>,
<k> commit(s) on <base>)`. It refuses a branch with a merge commit in it
(`... in the patch has 2 parents`), one that shares no history with the
volume's branch, and a commit already on the branch. Options:

| Option | Meaning |
|---|---|
| `--volume V` | The volume (or `AGENTFS_VOLUME`) |
| `--remote R` | The remote name or URL (default `origin`) |
| `--branch B` | The volume's branch (default `agentfs/<volume>`) |
| `--repo DIR` | The producer's repository (default `.`) |
| `--put PATH=FILE`, `--delete PATH` | Build the patch on the remote head; repeatable |
| `-m MSG`, `--author 'Name <email>'` | Message and author of a built patch |
| `--replace N` | Replace queued patch N (it must still be the commit listed), or fill a gap at N |
| `--skip N` | Replace queued patch N with an empty patch, or fill a gap at N with one, so the queue moves on |
| `--barrier` | Make it a barrier: while it is set aside, every later patch waits behind it. A built or `--skip` patch gets the trailer `Agentfs-Barrier: yes`; a given commit must carry it in its last message paragraph (`git commit --amend --trailer 'Agentfs-Barrier: yes'`) |

A gap is a number below the highest one listed that the remote has
neither as `inbox/N` nor as `applied/N`; `--replace N` and `--skip N` fill
it create-only. Both refuse a number the volume has already applied (the
remote's `applied/N`, or `last_applied` in its status ref, which is pushed
even while the applied refs wait for the branch): a replacement would
never be applied. Submit a new patch instead. A patch the volume set aside
is still queued (`status.json` lists it), so `--replace N` and `--skip N`
take it even when later numbers have landed.
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
NUM=$(printf '%010d' $((10#${N:-0} + 1)))
# 2. The tip of a branch of single-parent commits on the branch (here: HEAD).
OID=$(git rev-parse HEAD)
# 3. Create the ref only if nobody has it (an empty lease means "must not exist").
git push "$URL" --force-with-lease="refs/agentfs/$VOL/inbox/$NUM:" "$OID:refs/agentfs/$VOL/inbox/$NUM"
```

If the push is rejected (`stale info`), another producer took that number:
list again and try the next one. Never push to a number that exists
unless you mean to replace that patch (lease on its current oid:
`--force-with-lease=refs/agentfs/$VOL/inbox/$NUM:<oid listed>`), and never reuse a number below
the highest applied one: it is ignored as `stale` (the warning
`PatchIgnored`), never applied, and deleted by the next queue push.

### Approve a patch

With `agentfs.patchAccept = approval` on the volume, a patch that merges
cleanly lands only once it is approved. In a producer repository with the
same remote:

```sh
agentfs patch approve --volume "$VOL" 4                 # the tip inbox/4 holds now
agentfs patch approve --volume "$VOL" 4 --patch <oid>   # refused if inbox/4 is another commit
```

It pushes `refs/agentfs/<volume>/approved/<n>` = the tip (leased on what
that ref held), and prints `approved patch <n> (<tip>) as <ref>`. The
volume fetches approvals with its inbox. An approval counts only for the
tip it names: a replaced patch needs approving again. Delete the ref on the
remote to withdraw an approval before the patch lands.

## 2. When patches apply

At every mount, after journal replay and before the workload can see the
tree. In Kubernetes that means a **pod restart** (`kubectl delete pod`, or
a rollout). With the volume stopped, `agentfs patch apply` does the same:

```sh
agentfs --state "$STATE" --root "$BACKING" patch apply   # exit 1 when blocked or a patch is set aside, 75 when busy
```

The mount first fetches `inbox/*` from the patch remote, waiting at most
`agentfs.patchFetchTimeout` (default `30s`). If that fails, the alarm
`PatchFetchFailed` is raised and the patches already fetched apply. The
mount log says what happened:

```text
patch 3 applied as 6f1c...
patch 4 held (conflict): merging onto the branch head 9a0b... conflicts in config/app.yaml
patch 5 applied as 77d2...
patches: through 5 applied, 1 queued (1 set aside)
```

After the next push to the patch remote, `refs/agentfs/<volume>/applied/<n>`
holds each applied patch and its `inbox/<n>` is gone.

**The same patch commit twice** (two receivers submitting one fork's
change at once, or a producer pushing a commit again) applies once (design
§12): the later number gets a marker commit with the head's tree, whose
subject is `duplicate of patch <m>: ...`, its refs move like an applied
patch's, and the `PatchDuplicate` event (not an alarm) and `status.json`'s
`duplicate_now` say so. Nothing is merged, so a change the workload has
reverted since stays reverted. `agentfs patch submit` of a commit the
remote already queues or has applied is refused before it pushes
(`already queued as inbox … or applied as …; a change lands at most
once`); only submits at the same moment both get through. To apply the
same content again on purpose, submit a new commit (`git commit --amend
--no-edit` on the patch gives it a new id).

## 3. Check the queue

```sh
agentfs --state "$STATE" status          # patches:   last applied 5, 1 queued (1 set aside), ok (from origin)
agentfs --state "$STATE" patch status    # the whole status; --json for scripts
```

`patch status` lists each set-aside patch with its verdict:

```text
held:      4 (conflict): merging onto the branch head 9a0b... conflicts in config/app.yaml
waiting:   6 behind 4 (same_files)
awaiting:  7 (approval)
not judged: 8 (command): the acceptance rule command was killed after 60s
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
| `duplicate_now` | `[{inbox, patch, applied_as}]`: numbers this run consumed as duplicates of a patch commit an earlier number (`applied_as`) applied; absent when there are none |
| `queue` | `[{inbox, patch, verdict}]`: every patch still queued, ascending. A set-aside one has a `verdict` (below); the one the run stopped at and those after it have none |
| `blocked` | `{inbox, patch, reason, message, paths}`, or `null` |
| `ignored` | `[{ref, inbox, patch, why}]`: inbox refs that are `malformed` (not ten digits), `stale` (at or below the last applied number, with another commit, and not set aside) or `withdrawn` (a set-aside patch whose inbox ref is gone; its mark is deleted) |
| `fetch` | `{remote, ok, error}` of this run's fetch, or `null` when none ran |

A `verdict` is one of:

| `kind` | Fields | Meaning |
|---|---|---|
| `held` | `reason`, `message`, `paths` | Held whole for a reason in [section 4](#4-a-patch-set-aside) |
| `waiting` | `why` (`same_files` or `barrier`), `behind` | Behind these earlier set-aside patches |
| `awaiting` | `what` (`approval` or `command`) | The acceptance rule has not accepted it yet |
| `not_judged` | `rule`, `message` | The acceptance rule could not judge it |

Telemetry: `agentfs.patch.queue_depth`, `agentfs.patch.blocked` (alert on
it being 1), `agentfs.patch.held` (patches set aside at the last run), the
span `agentfs.patch` (outcome `applied`, `held`, `waiting`, `awaiting`,
`not_judged` or `blocked`), the `PatchBlocked`, `PatchHeld`,
`PatchRuleFailed` and `PatchFetchFailed` alarms, and the `PatchIgnored`
warning. Waiting and awaiting raise nothing: the status carries them.

## 4. A patch set aside

A patch that cannot land is **set aside whole** and the queue goes on:
nothing of it is written, it stays queued with a verdict, the local ref
`refs/agentfs/<volume>/held/<n>` marks it (never pushed), and every run
judges it again (a later head can make a conflict merge cleanly). A held or
not-judged patch raises `PatchHeld` (`patch <n> (<oid>) set aside, held
(<reason>): ...`).

- **Waiting.** A later patch changing a path a set-aside patch changes
  waits behind it (`waiting: 6 behind 4 (same_files)`); once 4 lands, is
  replaced or is skipped, 6 is judged.
- **Barriers.** While a barrier (`--barrier`) is set aside, every later
  patch waits behind it, whatever it changes (`behind 4 (barrier)`).
- **Acceptance rule** (`agentfs.patchAccept`, [section 6](#6-configuration)).
  `approval`: awaiting until [approved](#approve-a-patch). `command`: the
  hook `$GIT_DIR/hooks/patch-accept` gets the candidate as JSON on stdin;
  exit 0 lands it, 1 holds it as `refused` (the first line of its stderr is
  the message), 75 leaves it awaiting. Any other status, a signal, the
  timeout or a hook that cannot start leaves it **not judged** and raises
  `PatchRuleFailed`: fix the hook; the next run judges it again.

A run **stops** (blocks) only at `gap`, `unrecorded` or `failed`: that
patch and every later one stay queued unjudged, `PatchBlocked` is raised
(`patch queue blocked at <n> (<oid>, <reason>): ...`), and **the volume
mounts anyway** with the patches before it applied.

| Reason | Effect | Cause | Fix |
|---|---|---|---|
| `conflict` | held | The patch conflicts with changes on the branch since its base; `paths` lists them | Rebase it onto the current head and `--replace N` |
| `gap` | stops | Number N is missing while later ones are queued (also a held patch withdrawn above the last applied number) | `agentfs patch submit --volume "$VOL" --skip N`, or `--replace N` with the missing patch (both fill N create-only) |
| `parent_not_in_history` | held | Its history does not meet the volume's branch | Rebase onto the branch and `--replace N` |
| `merge_commit` | held | A commit of the branch has more than one parent | Rebase or squash it into a line of single-parent commits and `--replace N` |
| `unsupported` | held | A root commit, a submodule, or a path agentfs does not record | Remove the path and `--replace N`, or `--skip N` |
| `too_large` | held | More than 4096 paths | Split it: `--replace N` with the first part, then submit the rest |
| `refused` | held | The acceptance rule refused it | Replace it (`--replace N`) or skip it (`--skip N`) |
| `unrecorded` | stops | The tree differs from history where the patch writes (`paths` lists them): a change not recorded, a pending repair, something untracked in the way | Record the paths with the volume stopped, `agentfs --state "$STATE" --root "$BACKING" record <paths>` (or finish the repair or restore that left them), then restart. The mount only commits what is journaled; it never records other differences by itself |
| `failed` | stops | Writing or recording failed, or an LFS object could not be found | Read the message; fix the state area or the LFS server, restart |

To withdraw a set-aside patch, delete its `inbox/<n>` on the remote: the
next run deletes its mark (`PatchIgnored`, `withdrawn`) and the patches
waiting on it are judged. Below the last applied number that is all; above
it, the missing number is a `gap` like any other.

`blocked at 0 (failed)` means the queue could not run at all (the LFS
cache or the backing tree could not be opened, a ref was locked, history
could not be read): no patch was applied this time. The mount says
`patches: the queue could not run: ...` and raises `PatchBlocked`;
`meta/patch.json`, the gauge and the status ref say blocked. Fix the
cause and restart, or run `agentfs patch apply` with the volume stopped.

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
(`patch(es) N were already applied; their refs are moved`). A crash after
the patch was written into the tree but before it was recorded is noted in
`meta/patch.json` (`in_flight`): if the producer then replaced, skipped
or withdrew the patch, the next run puts the paths it wrote back as the
branch has them (or, failing that, records them as they stand) before
applying anything, so the tree never keeps content history lacks.
Nothing to do by hand.

## 6. Configuration

| Key | Default | Meaning |
|---|---|---|
| `agentfs.patchRemote` | the only remote, if exactly one | The remote whose `inbox/*` is fetched and which gets the queue push; `none` turns both off (patches already fetched still apply) |
| `agentfs.patchFetchTimeout` | `30s` | How long a mount waits for the fetch; `0` or `off` skips it |
| `agentfs.patchAccept` | `cleanMerge` | The acceptance rule for a cleanly merged patch: `cleanMerge` lands it, `approval` waits for `agentfs patch approve`, `command` asks `$GIT_DIR/hooks/patch-accept` ([section 4](#4-a-patch-set-aside)) |
| `agentfs.patchAcceptTimeout` | `60s` | How long the hook may run before its process group is killed and the patch is not judged |

The queue push (status ref, `applied/<n>`, deleting `inbox/<n>` of applied
numbers, stale ones included) runs after each branch push attempt to the
patch remote, failed ones included, so the status is current on the
remote even while the branch cannot push (applied refs and deletions wait
for a branch push that worked); `agentfs push` prints it as
`<remote>: queue: ...`. Its failures are `PushFailed` with stage `queue`
and never count against the branch.
