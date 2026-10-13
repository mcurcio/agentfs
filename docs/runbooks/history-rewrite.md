# Runbook: removing a recorded secret from history

agentfs never scans content and never rewrites history on its own
([§10](../design.md#10-secrets-not-in-scope)). A secret that was recorded
is in the state area's history and on every remote it reached. Removing
it is this operator procedure ([§15](../design.md#15-recovery-and-undo)):
rewrite the volume's branch with `git filter-repo`, outside the state
area, bring it back, and force-push it to each remote. It is the one
case where the volume's branch is force-pushed, by an operator.

`ci/e2e-runbooks.sh` runs every block below, as written, in CI (against
a `file://` remote), and checks that the secret's blobs are gone from the
state area and the remote, that pushes are fast-forwards again and that
`Agentfs-Seq` keeps increasing.

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
--get-regexp 'remote\..*\.(push)?url'` lists them), with git credentials
for it, as in [divergence.md](divergence.md). The lease makes the push
fail if the branch moved since you looked.

<!-- ci: rewrite-remote -->
```sh
URL=$(git --git-dir "$STATE/git" config remote.origin.url)
OLD_TIP=$(git ls-remote "$URL" "$BRANCH" | cut -f1)
git --git-dir "$STATE/git" push -q --force-with-lease="$BRANCH:$OLD_TIP" "$URL" "$BRANCH:$BRANCH"
for ref in $(git ls-remote "$URL" "refs/agentfs/$VOL/*" | cut -f2); do
  git --git-dir "$STATE/git" push -q "$URL" --delete "$ref"
done
```

The loop deletes the volume's other refs on that remote (`diverged/`,
`overwritten/`): they hold the old history too. To keep one, rewrite it
like the branch instead (fetch it into the mirror before step 4).

A force-push does not remove the old objects from the server. What does
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
