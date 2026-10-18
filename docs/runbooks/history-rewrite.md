# Runbook: removing a recorded secret from history

agentfs never scans content and never rewrites history on its own
([§10](../design.md#10-secrets-not-in-scope)). A secret that was recorded
is in the state area's history and on every remote it reached. Removing
it is this operator procedure ([§15](../design.md#15-recovery-and-undo)):
rewrite the volume's branch with `git filter-repo`, outside the state
area, bring it back, and replace it on each remote with `agentfs push
--force-with-lease`. It is the one case where the volume's branch is
replaced, by an operator, and only through that command: no runbook
`git push`es a branch (a test, `runbooks_push_only_the_namespace`,
holds them to refs under `refs/agentfs/`).

`ci/e2e-runbooks.sh` runs every block below, as written, in CI (against
a `file://` remote), and checks that the secret's blobs are gone from the
state area and the remote, that pushes are fast-forwards again and that
`Agentfs-Seq` keeps increasing. `ci/e2e-runbooks.sh forks` runs the
whole procedure again with [Forks](#forks), on a volume with two forks.

If the volume has forks (design [§11.5](../design.md#115-forks)), read
[Forks](#forks) before you start: two of its steps come between steps 2
and 3, and one after step 9.

Throughout:

```sh
STATE=/data/state                    # the state area
BACKING=/data/backing                # the backing tree
VOL=hermes                           # the volume
BRANCH=refs/heads/agentfs/$VOL
SECRET_PATH=config/.env              # the recorded file, relative to the tree
BACKUP=/data/state.before-rewrite    # where the state area is backed up
```

Needs `git` and `git-filter-repo` (packaged as `git-filter-repo` in
Debian and Ubuntu, `pip install git-filter-repo` elsewhere).

## 0. Rotate the secret first

Whatever was recorded must be treated as leaked: it may already be in
clones, forks, caches and backups this procedure cannot reach. Rotate it
before anything else.

## 1. Stop the workload and agentfs, and commit everything

Stop the workload and the daemon (`agentfs mount` or `agentfs run`; in
Kubernetes, scale the workload to zero and run the rest from a one-off
pod with the volume's PVC). Then commit whatever is journaled and check
that nothing is left:

<!-- ci: rewrite-flush -->
```sh
agentfs --state "$STATE" --root "$BACKING" flush
agentfs --state "$STATE" status
```

`status` must show `backlog:   0 record(s)` and `lock:      free`.

## 2. Keep it from being recorded again

Mark the path `-agentfs` in the operator attributes (it is then never
recorded, even if the workload writes it again), or move the file out of
the backing tree. A path in history keeps being recorded otherwise
(§4).

<!-- ci: rewrite-exclude -->
```sh
printf '%s -agentfs\n' "$SECRET_PATH" >> "$STATE/git/info/attributes"
```

## 3. Back up the state area

<!-- ci: rewrite-backup -->
```sh
cp -a "$STATE" "$BACKUP"
```

The backup holds the secret: delete it once the procedure is verified
(§9).

## 4. Rewrite the branch in a mirror clone

`filter-repo` runs on an independent mirror, never on the state area, so
it cannot change the state area's config, remotes or reflogs.
`--prune-empty never` keeps every commit, even one that only held the
secret, so the `Agentfs-Seq` trailers recovery depends on stay in order
(§3.3). filter-repo also rewrites commit ids quoted in messages, such as
`Agentfs-Restore` trailers.

<!-- ci: rewrite-filter -->
```sh
WORK=$(mktemp -d)
git clone -q --mirror --no-local "$STATE/git" "$WORK/rw.git"
git -C "$WORK/rw.git" filter-repo --force --prune-empty never --invert-paths --path "$SECRET_PATH"
```

To remove a secret from inside a file that should stay, use
`--replace-text` instead of `--invert-paths --path`: a file with one
`<secret>==>***REMOVED***` line per secret.

## 5. Bring it back into the state area

<!-- ci: rewrite-fetch -->
```sh
git --git-dir "$STATE/git" fetch -q --force "$WORK/rw.git" '+refs/heads/*:refs/heads/*' '+refs/agentfs/*:refs/agentfs/*'
```

## 6. Drop the old objects

The state area keeps a reflog of every branch update
(`core.logAllRefUpdates=always`), which still reaches the old commits:

<!-- ci: rewrite-gc -->
```sh
git --git-dir "$STATE/git" reflog expire --expire=now --all
git --git-dir "$STATE/git" gc -q --prune=now
```

## 7. Verify the state area

<!-- ci: rewrite-verify -->
```sh
agentfs --state "$STATE" status
git --git-dir "$BACKUP/git" rev-list --all --objects | awk -v p="$SECRET_PATH" '$2 == p {print $1}' | sort -u > "$WORK/secret-blobs"
test -s "$WORK/secret-blobs"
if git --git-dir "$STATE/git" cat-file --batch-all-objects --batch-check='%(objectname)' | grep -F -x -f "$WORK/secret-blobs"; then echo "secret objects remain"; false; fi
test -z "$(git --git-dir "$STATE/git" log --all --format=%H -- "$SECRET_PATH")"
```

`status` must show the same `committed: through seq N` as before the
rewrite; the checks must print nothing and succeed.

## 8. Forget the push bookkeeping

Each remote's push state names commits that no longer exist. It is only
bookkeeping (§11.2), rebuilt on the next push. A remote with
`agentfsMaxPushSize` measures that push from the branch the remote
advertises (§11.3), so an up-to-date remote is not pushed again in steps:

<!-- ci: rewrite-push-state -->
```sh
rm -f "$STATE"/meta/push/*.json
```

## 9. Rewrite every remote

For each remote the secret reached (`git --git-dir "$STATE/git" config
--get-regexp 'remote\..*\.(push)?url'` lists them; `NAME` is the remote's
name), replace the branch with `agentfs push --force-with-lease`, leased
on the tip the remote shows: it fails if the branch moved since you
looked (`lease broken`), and refuses a branch that shares no commit with
the rewritten one (`shares no commit`: the secret was in the first
commit, so the rewrite changed the root; continue under a new name
instead, as in [divergence.md](divergence.md) case B). It also refuses
(`which carries agentfs trailers`) when an old commit it would drop has no
rewritten copy on the head, the same `Agentfs-Seq`, author and committer:
a commit filter-repo pruned (step 4 keeps them all with `--prune-empty
never`), or one recorded elsewhere that this state area never had. The old tip, gone
from the state area since step 6, is fetched into a scratch repository
for that check, never into the state area, and removed when the command
ends, however it ends; one left by a killed command
(`$STATE/meta/push/<remote>.lease.git`) is removed by the next push. `git ls-remote`, the fetch and the
deletions need git credentials for the remote, as in
[divergence.md](divergence.md).

<!-- ci: rewrite-remote -->
```sh
NAME=origin
URL=$(git --git-dir "$STATE/git" config "remote.$NAME.url")
OLD_TIP=$(git ls-remote "$URL" "$BRANCH" | cut -f1)
agentfs --state "$STATE" push --remote "$NAME" --force-with-lease "$OLD_TIP"
BAD=$(git --git-dir "$BACKUP/git" rev-list --reverse "$BRANCH" -- "$SECRET_PATH" | sed -n 1p)
test -n "$BAD"
git init -q --bare "$WORK/$NAME.git"
git --git-dir "$WORK/$NAME.git" fetch -q "$URL" "refs/agentfs/$VOL/*:refs/agentfs/$VOL/*"
if git --git-dir "$WORK/$NAME.git" cat-file -e "$BAD^{commit}" 2>/dev/null; then
  git --git-dir "$WORK/$NAME.git" for-each-ref --contains "$BAD" --format='%(objectname) %(refname:lstrip=3)' "refs/agentfs/$VOL/" |
    while read -r OID REF; do
      [ "refs/agentfs/$VOL/$REF" != "$BRANCH" ] || continue
      git --git-dir "$STATE/git" push -q "$URL" --force-with-lease="refs/agentfs/$VOL/$REF:$OID" --delete "refs/agentfs/$VOL/$REF" ||
        echo "refs/agentfs/$VOL/$REF moved since it was listed: left"
    done
fi
```

The loop deletes the volume's other refs on that remote that hold the old
history, found by reachability: each ref under `refs/agentfs/$VOL/` that
contains `$BAD`, the first commit that recorded the secret (`diverged/`
heads, and producers' patches and `applied/` commits based on it, which
cannot land on the rewritten history anyway: producers submit them
again). Every other ref stays: an `inbox/<n>` based before the secret,
the queue's `status`, an `overwritten/` tip of another writer's history
from before it, and, for a fork with a hidden push target, its branch
itself (`refs/agentfs/$VOL/history`, just replaced). Each deletion is
leased on the oid listed, so a ref written or moved since (a patch
submitted meanwhile) is left, and a moved one is named. To keep a ref
that holds the secret, rewrite it like the branch instead (fetch it into
the mirror before step 4).

Replacing the branch does not remove the old objects from the server. What does
depends on the host:

- **A bare repository you run** (ssh, a file server): expire its reflog
  and collect garbage on the server, where `$SERVER_REPO` is its path:

  <!-- ci: rewrite-purge-bare -->
  ```sh
  git --git-dir "$SERVER_REPO" reflog expire --expire=now --all
  git --git-dir "$SERVER_REPO" gc -q --prune=now
  ```

- **GitHub**: the objects stay reachable by id, in cached views, pull
  request refs and forks. Ask GitHub Support to purge them (they need the
  commit ids, which `$WORK/rw.git/filter-repo/commit-map` lists), and
  delete forks you control.
- **Gitea, GitLab**: run the repository's housekeeping or garbage
  collection as an administrator; check forks and mirrors.
- **Clones**: every existing clone still has the secret.

## 10. Restart and verify

Start agentfs again. Its first push to each remote is up to date or a
fast-forward:

<!-- ci: rewrite-restart -->
```sh
agentfs --state "$STATE" push
agentfs --state "$STATE" status
```

Then delete what still holds the secret locally:

```sh
rm -rf "$WORK" "$BACKUP"
```

## Forks

A fork copies the source's history up to its base and pushes it to its
fork remote, so a secret recorded before a fork's base is in that fork's
history too: in its state area, on its fork remote, and in its change's
parent. Forks whose base predates the bad commit are unaffected: the
commits they share with the source keep their ids under filter-repo, so
they finish, publish and land as before.

Find what is affected by reachability, never from a list kept beside the
forks: a ref is affected if it contains the first commit that recorded
the secret. Then abandon every fork whose base contains it, before
anything is rewritten, so no finisher pushes the old history again.

Also set:

```sh
FORK_STATES="/data/forks/*/state"    # the forks' state areas (CSI: /var/lib/agentfs-csi/volumes/*/state)
```

Under CSI, run the next two blocks in each node plugin pod
(`kubectl -n agentfs-csi exec -it "$NODE_POD" -c agentfs-csi -- sh`),
with the state area paths of that node. Stop the forks' pods first, as
the workload in step 1: a running fork holds its state lock, and
`fork abandon` then exits 75.

### F1. Find the bad commit and every affected ref (after step 2)

The first commit on the volume's branch that recorded the path, and on
each remote the source's settings name (its own remotes and every fork
remote: `agentfsForkRemote` sections), every ref of another volume under
`refs/heads/agentfs/` or `refs/agentfs/` that contains it. A remote that
does not have the commit has no ref that contains it. The volume's own
refs are rewritten in step 9 and left out here.

<!-- ci: rewrite-fork-find -->
```sh
BAD=$(git --git-dir "$STATE/git" rev-list --reverse "$BRANCH" -- "$SECRET_PATH" | sed -n 1p)
test -n "$BAD"
FORK_WORK=$(mktemp -d)
: > "$FORK_WORK/affected"
for REMOTE in $(git --git-dir "$STATE/git" config --name-only --get-regexp '^remote\..*\.url$' | sed 's/^remote\.//; s/\.url$//'); do
  URL=$(git --git-dir "$STATE/git" config "remote.$REMOTE.url")
  git clone -q --mirror "$URL" "$FORK_WORK/$REMOTE.git"
  git --git-dir "$FORK_WORK/$REMOTE.git" cat-file -e "$BAD^{commit}" 2>/dev/null || continue
  git --git-dir "$FORK_WORK/$REMOTE.git" for-each-ref --contains "$BAD" --format='%(objectname) %(refname)' refs/heads/agentfs/ refs/agentfs/ |
    while read -r OID REF; do
      case "$REF" in "refs/heads/agentfs/$VOL" | "refs/agentfs/$VOL/"*) continue ;; esac
      echo "$URL $OID $REF" >> "$FORK_WORK/affected"
    done
done
cat "$FORK_WORK/affected"
```

### F2. Abandon the forks whose base contains it (after F1)

For each fork of the volume, abandon it on record only when
`git merge-base --is-ancestor "$BAD" <base>` holds. Nothing agentfs runs
pushes an abandoned fork again, and under CSI the next DeleteVolume
applies its `onDelete`. A published fork cannot be abandoned (it says
so): its refs are in `$FORK_WORK/affected` and go in F3, and its change,
not landed yet, can no longer land: its parent is not in the rewritten
history (`parent_not_in_history`, design §12).

<!-- ci: rewrite-fork-abandon -->
```sh
for FORK_STATE in $FORK_STATES; do
  [ "$(git --git-dir "$FORK_STATE/git" config agentfs.fork.source)" = "$VOL" ] || continue
  FORK_BASE=$(git --git-dir "$FORK_STATE/git" config agentfs.fork.base)
  if git --git-dir "$STATE/git" merge-base --is-ancestor "$BAD" "$FORK_BASE"; then
    agentfs --state "$FORK_STATE" fork abandon --reason "history rewrite of $VOL: base $FORK_BASE contains $BAD" ||
      echo "$FORK_STATE: not abandoned; its refs are deleted in F3"
  fi
done
```

An abandoned fork's state area still holds the secret: delete it (CLI:
its state area and backing tree; CSI: delete its claim, and remove
`<basePath>/<pv>` by hand if its class keeps volumes with
`onDelete: retain`). Never mount it again: an abandoned fork that is
mounted pushes its branch on its interval like any volume.

### F3. Delete the affected refs on each remote (after step 9)

These are other worktrees' refs (the abandoned forks' branches and their
`refs/agentfs/<fork>/` refs), which no runbook `git push`es: each worktree
writes only its own branch. Delete them where each repository is kept,
each only while it still holds the oid F1 found, so a ref that moved
since is left for you to look at:

- **A bare repository you run**: on the server, with `$SERVER_REPO` its
  path and `$SERVER_URL` the URL F1 listed it under (copy
  `$FORK_WORK/affected` there). `update-ref -d <ref> <oid>` deletes only
  a ref at that oid:

  <!-- ci: rewrite-fork-refs -->
  ```sh
  while read -r URL OID REF; do
    [ "$URL" = "$SERVER_URL" ] || continue
    git --git-dir "$SERVER_REPO" update-ref -d "$REF" "$OID" || echo "$REF moved since F1: left"
  done < "$FORK_WORK/affected"
  ```

- **A forge**: delete each ref with the forge's own tools (GitHub: `gh
  api -X DELETE repos/<owner>/<repo>/git/refs/<the ref without refs/>`),
  after checking it still points at its oid: a forge's API takes no
  lease.

Then purge each host as in step 9 (a bare repository: `reflog expire` and
`gc` on it; a forge: its own purge), fork remotes included, and continue
with step 10. Forks that were not abandoned publish as before.

### What agentfs cannot find

The steps above reach the source's state area and remotes, the forks'
state areas you list and their fork remotes. They cannot reach, and you
must check yourself:

- delivery targets: the repositories the branch method pushed to (pull
  request heads), and what webhook and command receivers did with a
  change;
- receivers' clones and mirrors of a fork remote, and any repository a
  change was landed in from them;
- users' CI caches and artifacts built from a fork's branch;
- fork state areas kept on disk: forks under CSI whose class has
  `onDelete: retain`, and CLI forks you did not list in `FORK_STATES`;
- clones, forks and caches on a forge, as in step 9.

The rewritten source's history keeps the ids of everything before the bad
commit, so forks based earlier stay landable; everything from the bad
commit on has new ids, so any change based there that was not landed
before the rewrite cannot be landed after it.
