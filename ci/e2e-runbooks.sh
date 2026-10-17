#!/usr/bin/env bash
# Runs the operator runbooks exactly as written: every shell block marked
# `<!-- ci: <name> -->` in docs/runbooks/divergence.md (case A) and
# docs/runbooks/history-rewrite.md is extracted and run in order against a
# real volume and a file:// remote, and the results are checked:
#
# - divergence: the diverged save follows the head; the foreign tip is
#   kept under overwritten/, `agentfs push --force-with-lease` makes the
#   branch the local head again without taking the foreign commit in,
#   pushes succeed and `status` no longer says diverged, the diverged refs
#   are gone;
# - history rewrite: the secret's blobs are gone from the state area and
#   the remote (the lease push of step 9 fetches the old tip aside, never
#   into the state area), every commit and the last Agentfs-Seq survive,
#   pushes are fast-forwards again, recording continues with a higher
#   Agentfs-Seq, and the secret is not recorded again;
# - LFS storage (docs/runbooks/lfs-storage.md): a dry run deletes nothing
#   and names the remote that lacks an object, a prune deletes only what
#   every remote holds, `lfs fetch` brings it back, and a remote added
#   after a prune gets its objects through `lfs fetch --all`.
#
# `ci/e2e-runbooks.sh forks` runs only the history rewrite again, with its
# 'Forks' steps, on a source with two forks, one based before the secret
# was recorded and one after: the secret's blobs are gone from the source
# remote and the fork remote, only the later fork is abandoned and its
# refs deleted (on each server, leased on the oid found), and the earlier
# one still publishes. CI runs it as a step
# of its own, so a flake of either is attributable.
#
# Needs git and git-filter-repo (the history-rewrite parts). Run from the
# repository root after `cargo build -p agentfs`:
#
#   ci/e2e-runbooks.sh
#   ci/e2e-runbooks.sh forks

. "$(dirname "$0")/lib.sh"

[ -x "$AGENTFS_BIN" ] || fail "build first: cargo build -p agentfs"

seq_of() { # commit
  git --git-dir "$STATE/git" log -1 --format=%B "$1" | sed -n 's/^Agentfs-Seq: //p'
}

# `agentfs` against the fork in $W/forks/$1.
fork_agentfs() { # name args...
  local dir=$W/forks/$1
  shift
  AGENTFS_POD=ci-0 "$AGENTFS_BIN" --state "$dir/state" --root "$dir/tree" "$@"
}

# Creates a fork of the source in $W/forks/$1; prints the fork's name.
fork_create() { # name
  mkdir -p "$W/forks/$1/tree"
  fork_agentfs "$1" fork create --source-state "$STATE" | sed -n 's/^fork \([^ ]*\) of .*/\1/p'
}

# Fails if a secret blob of $WORK/secret-blobs is in bare repository $1.
no_secret_in() { # repo
  if git --git-dir "$1" cat-file --batch-all-objects --batch-check='%(objectname)' |
    grep -F -x -f "$WORK/secret-blobs"; then
    fail "secret objects remain in $1"
  fi
  pass "no secret object in $1"
}

if [ "${1:-}" = forks ]; then
  git filter-repo --version >/dev/null 2>&1 || fail "git-filter-repo is needed for the history-rewrite runbook"
  W=$(mktemp -d)
  STATE=$W/state
  BACKING=$W/backing
  TREE=$BACKING
  VOL=forkbook
  BRANCH=refs/heads/agentfs/$VOL
  SECRET_PATH=config/.env
  BACKUP=$W/state.before-rewrite
  FORK_STATES="$W/forks/*/state"
  git init -q --bare "$W/origin.git"
  git init -q --bare "$W/forks.git"
  printf '[remote "origin"]\n\turl = file://%s\n[remote "forks"]\n\turl = file://%s\n\tagentfsForkRemote = true\n' \
    "$W/origin.git" "$W/forks.git" >"$W/profile"
  mkdir -p "$BACKING"

  step "a source whose settings are a profile, with a fork remote"
  agentfs init --volume "$VOL" --include "$W/profile" >/dev/null
  echo one >"$BACKING/a.txt"
  agentfs record a.txt >/dev/null
  agentfs push

  step "a fork based before the secret, pushed to the fork remote"
  EARLY=$(fork_create early)
  [ -n "$EARLY" ] || fail "fork create printed no name"
  echo early >"$W/forks/early/tree/e.txt"
  fork_agentfs early record e.txt >/dev/null
  fork_agentfs early push

  step "the source records a secret; a fork based after it pushes it too"
  mkdir -p "$BACKING/config"
  echo "TOKEN=hunter2" >"$BACKING/$SECRET_PATH"
  agentfs record "$SECRET_PATH" >/dev/null
  echo three >"$BACKING/c.txt"
  agentfs record c.txt >/dev/null
  agentfs push
  LATE=$(fork_create late)
  [ -n "$LATE" ] || fail "fork create printed no name"
  echo late >"$W/forks/late/tree/l.txt"
  fork_agentfs late record l.txt >/dev/null
  fork_agentfs late push
  [ -n "$(git --git-dir "$W/forks.git" log --all --format=%H -- "$SECRET_PATH")" ] ||
    fail "the secret should be on the fork remote before the rewrite"
  EARLY_BASE=$(git --git-dir "$W/forks/early/state/git" config agentfs.fork.base)
  LATE_BASE=$(git --git-dir "$W/forks/late/state/git" config agentfs.fork.base)

  for name in rewrite-flush rewrite-exclude rewrite-fork-find rewrite-fork-abandon \
    rewrite-backup rewrite-filter rewrite-fetch rewrite-gc rewrite-verify \
    rewrite-push-state rewrite-remote; do
    step "history-rewrite runbook: $name"
    run_block history-rewrite.md "$name"
  done
  for SERVER_REPO in "$W/origin.git" "$W/forks.git"; do
    SERVER_URL=file://$SERVER_REPO
    step "history-rewrite runbook: rewrite-fork-refs ($SERVER_REPO)"
    run_block history-rewrite.md rewrite-fork-refs
  done
  for SERVER_REPO in "$W/origin.git" "$W/forks.git"; do
    step "history-rewrite runbook: rewrite-purge-bare ($SERVER_REPO)"
    run_block history-rewrite.md rewrite-purge-bare
  done
  step "history-rewrite runbook: rewrite-restart"
  run_block history-rewrite.md rewrite-restart

  step "the secret is gone from both remotes; only the later fork is abandoned"
  no_secret_in "$W/origin.git"
  no_secret_in "$W/forks.git"
  assert_eq "" "$(git --git-dir "$W/forks.git" for-each-ref --format='%(refname)' \
    "refs/heads/agentfs/$LATE" "refs/agentfs/$LATE/")" "the later fork's refs are deleted"
  late_state=$(fork_agentfs late fork status --json | sed -n 's/^  "state": "\(.*\)",$/\1/p')
  assert_eq abandoned "$late_state" "the fork based after the secret is abandoned"
  early_state=$(fork_agentfs early fork status --json | sed -n 's/^  "state": "\(.*\)",$/\1/p')
  assert_eq active "$early_state" "the fork based before it is not"
  git --git-dir "$STATE/git" merge-base --is-ancestor "$EARLY_BASE" "$BRANCH" ||
    fail "the earlier fork's base is not in the rewritten history"
  pass "the earlier fork's base keeps its id"
  if git --git-dir "$STATE/git" merge-base --is-ancestor "$LATE_BASE" "$BRANCH" 2>/dev/null; then
    fail "the later fork's base is still in the rewritten history"
  fi
  pass "the later fork's base is not"

  step "the earlier fork still publishes"
  out=$(fork_agentfs early fork finish --timeout 60s)
  echo "$out"
  assert_contains "$out" "published to forks" "the earlier fork publishes"
  [ -n "$(remote_ref "$W/forks.git" "refs/agentfs/$EARLY/change")" ] || fail "no change ref on the fork remote"
  pass "its change is on the fork remote"
  set +e
  out=$(fork_agentfs late fork finish --timeout 5s 2>&1)
  code=$?
  set -e
  assert_eq 1 "$code" "the abandoned fork does not publish"
  assert_contains "$out" "abandoned" "and says why"
  no_secret_in "$W/forks.git"

  step "fork runbooks passed"
  exit 0
fi

W=$(mktemp -d)
STATE=$W/state
BACKING=$W/backing
TREE=$BACKING # lib.sh's agentfs passes it as AGENTFS_ROOT
VOL=runbooks
BRANCH=refs/heads/agentfs/$VOL
REMOTE=$W/remote.git
mkdir -p "$BACKING"
git init -q --bare "$REMOTE"
URL=file://$REMOTE

step "a volume pushing to a file:// remote"
agentfs init --volume "$VOL" --local-only >/dev/null
volume_config remote.origin.url "$URL"
echo one >"$BACKING/a.txt"
agentfs record a.txt >/dev/null
agentfs push

step "another writer moves the branch"
OTHER=$W/other
git clone -q "$URL" "$OTHER"
git -C "$OTHER" checkout -q -b w "origin/agentfs/$VOL"
echo theirs >"$OTHER/theirs.txt"
git -C "$OTHER" add theirs.txt
git -C "$OTHER" -c user.name=intruder -c user.email=i@example.com commit -q -m "a foreign commit"
git -C "$OTHER" push -q origin "w:$BRANCH"
FOREIGN=$(remote_ref "$REMOTE" "$BRANCH")
echo two >"$BACKING/b.txt"
agentfs record b.txt >/dev/null
set +e
out=$(agentfs push 2>&1)
set -e
assert_contains "$out" "diverged" "the push reports the divergence"
assert_contains "$(agentfs status)" "diverged: head saved as" "status shows it"

step "the divergence save follows the head"
echo three >"$BACKING/b2.txt"
agentfs record b2.txt >/dev/null
set +e
agentfs push >/dev/null 2>&1
set -e
LOCAL=$(local_head "$VOL")
assert_eq "$LOCAL" \
  "$(git --git-dir "$REMOTE" for-each-ref --format='%(objectname)' "refs/agentfs/$VOL/diverged/")" \
  "the one diverged ref is the new head"

step "divergence runbook: inspect"
run_block divergence.md divergence-inspect
assert_eq "$FOREIGN" "$REMOTE_TIP" "the foreign tip is found"

step "divergence runbook: case A"
NAME=origin
out=$(run_block divergence.md divergence-case-a)
echo "$out"
assert_contains "$out" "origin: replaced $FOREIGN with $LOCAL" "agentfs push replaces the branch, leased"
assert_eq "$LOCAL" "$(remote_ref "$REMOTE" "$BRANCH")" "the branch is the local head again"
assert_eq "$FOREIGN" \
  "$(git --git-dir "$REMOTE" for-each-ref --format='%(objectname)' "refs/agentfs/$VOL/overwritten/")" \
  "the foreign tip is kept under overwritten/"
git --git-dir "$STATE/git" cat-file -e "$FOREIGN^{commit}" 2>/dev/null &&
  fail "the lease push took the foreign commit into the state area"
pass "the foreign commit stays out of the state area"
set +e
out=$(agentfs push --remote origin --force-with-lease "$FOREIGN" 2>&1)
set -e
assert_contains "$out" "origin: up to date at $LOCAL" "a second lease push is up to date"

step "divergence runbook: verify"
run_block divergence.md divergence-verify
case "$(agentfs status)" in
  *diverged*) fail "status still says diverged" ;;
  *) pass "the divergence is cleared" ;;
esac

step "divergence runbook: clean up"
run_block divergence.md divergence-cleanup
assert_eq "" "$(git --git-dir "$REMOTE" for-each-ref "refs/agentfs/$VOL/diverged/")" \
  "the diverged refs are gone"

git filter-repo --version >/dev/null 2>&1 || fail "git-filter-repo is needed for the history-rewrite runbook"

step "a secret is recorded and pushed"
SECRET_PATH=config/.env
mkdir -p "$BACKING/config"
echo "TOKEN=hunter2" >"$BACKING/$SECRET_PATH"
agentfs record "$SECRET_PATH" >/dev/null
echo "TOKEN=hunter3" >"$BACKING/$SECRET_PATH"
echo three >"$BACKING/c.txt"
agentfs record "$SECRET_PATH" >/dev/null
agentfs record c.txt >/dev/null
agentfs push
COMMITS_BEFORE=$(git --git-dir "$STATE/git" rev-list --count "$BRANCH")
SEQ_BEFORE=$(seq_of "$BRANCH")
[ -n "$(git --git-dir "$REMOTE" log --format=%H "$BRANCH" -- "$SECRET_PATH")" ] ||
  fail "the secret should be on the remote before the rewrite"
BACKUP=$W/state.before-rewrite
SERVER_REPO=$REMOTE

for name in rewrite-flush rewrite-exclude rewrite-backup rewrite-filter rewrite-fetch \
  rewrite-gc rewrite-verify rewrite-push-state rewrite-remote rewrite-purge-bare \
  rewrite-restart; do
  step "history-rewrite runbook: $name"
  run_block history-rewrite.md "$name"
done

step "the secret is gone, history is otherwise intact"
assert_eq "$COMMITS_BEFORE" "$(git --git-dir "$STATE/git" rev-list --count "$BRANCH")" \
  "every commit is kept (--prune-empty never)"
assert_eq "$SEQ_BEFORE" "$(seq_of "$BRANCH")" "the last Agentfs-Seq is unchanged"
assert_eq "" "$(git --git-dir "$STATE/git" log --all --format=%H -- "$SECRET_PATH")" \
  "no commit in the state area touches the secret"
for repo in "$STATE/git" "$REMOTE"; do
  if git --git-dir "$repo" cat-file --batch-all-objects --batch-check='%(objectname)' |
    grep -F -x -f "$WORK/secret-blobs"; then
    fail "secret objects remain in $repo"
  fi
  pass "no secret object in $repo"
done
assert_eq "$(local_head "$VOL")" "$(remote_ref "$REMOTE" "$BRANCH")" "the remote has the rewritten branch"
assert_eq "" "$(git --git-dir "$REMOTE" for-each-ref "refs/agentfs/$VOL/")" \
  "the volume's other refs on the remote are gone"

step "recording and pushing carry on"
echo four >"$BACKING/d.txt"
agentfs record d.txt >/dev/null
SEQ_AFTER=$(seq_of "$BRANCH")
[ "$SEQ_AFTER" -gt "$SEQ_BEFORE" ] || fail "Agentfs-Seq went from $SEQ_BEFORE to $SEQ_AFTER"
pass "Agentfs-Seq keeps increasing ($SEQ_BEFORE -> $SEQ_AFTER)"
out=$(agentfs push)
assert_contains "$out" "origin: pushed " "the next push is a fast-forward"
echo "TOKEN=hunter4" >"$BACKING/$SECRET_PATH"
out=$(agentfs record "$SECRET_PATH")
assert_contains "$out" "skipped $SECRET_PATH" "the secret is not recorded again"
assert_eq "" "$(git --git-dir "$STATE/git" log --all --format=%H -- "$SECRET_PATH")" \
  "history still has no secret"

step "an LFS volume pushing to two file:// remotes"
LW=$(mktemp -d)
STATE=$LW/state
BACKING=$LW/backing
TREE=$BACKING
mkdir -p "$BACKING"
agentfs init --volume lfsbook --local-only >/dev/null
volume_config agentfs.lfsThreshold 1k
for r in one two; do
  git init -q --bare "$LW/$r.git"
  volume_config "remote.$r.url" "file://$LW/$r.git"
done
head -c 4000 /dev/urandom >"$BACKING/big.bin"
head -c 4000 /dev/urandom >"$BACKING/other.bin"
agentfs record big.bin other.bin >/dev/null
agentfs push
lfs_oid() { git --git-dir "$STATE/git" show "agentfs/lfsbook:$1" | sed -n 2p | sed 's/^oid sha256://'; }
lfs_cached() { [ -f "$STATE/lfs/objects/${1:0:2}/${1:2:2}/$1" ]; }
BIG=$(lfs_oid big.bin)
OTHER_OID=$(lfs_oid other.bin)
# Remote two loses other.bin's object: it must stay cached.
rm "$LW/two.git/lfs/objects/${OTHER_OID:0:2}/${OTHER_OID:2:2}/$OTHER_OID"

step "lfs-storage runbook: check what the servers hold"
out=$(run_block lfs-storage.md lfs-dry-run)
echo "$out"
assert_contains "$out" "would prune 1 object(s)" "the dry run counts one object"
assert_contains "$out" "two lacks 1" "it names the remote that lacks one"
lfs_cached "$BIG" || fail "the dry run deleted an object"

step "lfs-storage runbook: prune"
out=$(run_block lfs-storage.md lfs-prune)
echo "$out"
assert_contains "$out" "pruned 1 object(s)" "one object pruned"
lfs_cached "$BIG" && fail "big.bin's object is still cached"
lfs_cached "$OTHER_OID" || fail "other.bin's object was pruned, though remote two lacks it"
pass "only the object every remote holds was pruned"

step "lfs-storage runbook: get objects back"
run_block lfs-storage.md lfs-fetch-head
lfs_cached "$BIG" || fail "lfs fetch did not bring big.bin's object back"
pass "lfs fetch brought it back"

step "lfs-storage runbook: a new remote after a prune"
run_block lfs-storage.md lfs-prune >/dev/null
NEW_REMOTE=three
git init -q --bare "$LW/three.git"
volume_config remote.three.url "file://$LW/three.git"
set +e
out=$(agentfs push --remote three 2>&1)
set -e
assert_contains "$out" "three: failed (local, stage lfs)" "the push needs the pruned object"
assert_contains "$out" "agentfs lfs fetch --all" "and says how to get it back"
run_block lfs-storage.md lfs-new-remote
assert_eq "$(local_head lfsbook)" "$(remote_ref "$LW/three.git" refs/heads/agentfs/lfsbook)" \
  "the new remote has the branch"
[ -f "$LW/three.git/lfs/objects/${BIG:0:2}/${BIG:2:2}/$BIG" ] || fail "the new remote lacks big.bin's object"
pass "and big.bin's object"

step "runbooks passed"
