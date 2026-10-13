#!/usr/bin/env bash
# Runs the operator runbooks exactly as written: every shell block marked
# `<!-- ci: <name> -->` in docs/runbooks/divergence.md (case A) and
# docs/runbooks/history-rewrite.md is extracted and run in order against a
# real volume and a file:// remote, and the results are checked:
#
# - divergence: the foreign tip is kept under overwritten/, the branch is
#   the local head again, pushes succeed and `status` no longer says
#   diverged, the diverged refs are gone;
# - history rewrite: the secret's blobs are gone from the state area and
#   the remote, every commit and the last Agentfs-Seq survive, pushes are
#   fast-forwards again, recording continues with a higher Agentfs-Seq,
#   and the secret is not recorded again;
# - LFS storage (docs/runbooks/lfs-storage.md): a dry run deletes nothing
#   and names the remote that lacks an object, a prune deletes only what
#   every remote holds, `lfs fetch` brings it back, and a remote added
#   after a prune gets its objects through `lfs fetch --all`.
#
# Needs git and git-filter-repo (the history-rewrite part). Run from the
# repository root after `cargo build -p agentfs`:
#
#   ci/e2e-runbooks.sh

. "$(dirname "$0")/lib.sh"

[ -x "$AGENTFS_BIN" ] || fail "build first: cargo build -p agentfs"
RUNBOOKS=$(cd "$(dirname "$0")/.." && pwd)/docs/runbooks

# The fenced block after `<!-- ci: $2 -->` in runbook $1.
block() {
  awk -v m="<!-- ci: $2 -->" '
    { line = $0; sub(/^[ \t]+/, "", line) }
    !found && line == m { found = 1; next }
    found && line ~ /^```/ { if (inside) exit; inside = 1; next }
    inside { print }' "$RUNBOOKS/$1"
}

# Runs that block in this shell, as an operator would paste it.
run_block() { # runbook name
  local code
  code=$(block "$1" "$2")
  [ -n "$code" ] || fail "no block '$2' in $1"
  printf '\033[2m# %s: %s\n%s\033[0m\n' "$1" "$2" "$code"
  eval "$code"
}

seq_of() { # commit
  git --git-dir "$STATE/git" log -1 --format=%B "$1" | sed -n 's/^Agentfs-Seq: //p'
}

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
agentfs init --volume "$VOL" >/dev/null
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
LOCAL=$(local_head "$VOL")

step "divergence runbook: inspect"
run_block divergence.md divergence-inspect
assert_eq "$FOREIGN" "$REMOTE_TIP" "the foreign tip is found"

step "divergence runbook: case A"
run_block divergence.md divergence-case-a
assert_eq "$LOCAL" "$(remote_ref "$REMOTE" "$BRANCH")" "the branch is the local head again"
assert_eq "$FOREIGN" \
  "$(git --git-dir "$REMOTE" for-each-ref --format='%(objectname)' "refs/agentfs/$VOL/overwritten/")" \
  "the foreign tip is kept under overwritten/"

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
agentfs init --volume lfsbook >/dev/null
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
