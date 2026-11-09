#!/usr/bin/env bash
# agentfs's fork and upstream rules, shown with git and coreutils alone: no
# Rust, no agentfs binary, no network, no secrets. It builds throwaway
# repositories under a mktemp directory and removes them on exit. Each scene
# asserts its results, and a wrong result stops the script with a non-zero exit.
#
# walkthrough.md, beside this file, explains each scene and says what this demo
# does not cover. The rule ids (R3b and so on) are those of ../rules-ledger.md.

set -euo pipefail

# Only this script's own repositories: no outside configuration or hooks.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null

# git's author and committer are one service identity; a trailer names the
# writer (git-model.md §2).
export GIT_AUTHOR_NAME=agentfs GIT_AUTHOR_EMAIL=agentfs@example.invalid
export GIT_COMMITTER_NAME=agentfs GIT_COMMITTER_EMAIL=agentfs@example.invalid

# A fixed date gives every run the same commit ids, so walkthrough.md can quote
# them. No rule reads a date (R2).
export GIT_AUTHOR_DATE=2026-01-01T00:00:00Z GIT_COMMITTER_DATE=2026-01-01T00:00:00Z

top=$(mktemp -d)
trap 'rm -rf "$top"' EXIT

volume='' # this scene's directory: one working tree per branch
repo=''   # the working tree that holds the scene's one repository
open_files=' ' # entries "tree:path", standing in for the kernel's open files

# Reporting ----------------------------------------------------------------

note() { printf '%s\n' "$*"; }
scene() { printf '\n== %s ==\n' "$*"; }
ok() { printf '  ok: %s\n' "$*"; }
fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

assert_eq() { # what expected actual
  [ "$2" = "$3" ] || fail "$1: expected [$2], got [$3]"
  ok "$1"
}

reaches() { # head commit: does head's history hold commit?
  git -C "$repo" merge-base --is-ancestor "$2" "$1"
}

assert_reaches() { # what head commit
  reaches "$2" "$3" || fail "$1: $2 does not reach $3"
  ok "$1"
}

assert_not_reaches() { # what head commit
  if reaches "$2" "$3"; then fail "$1: $2 reaches $3"; fi
  ok "$1"
}

# Reading a volume ---------------------------------------------------------

head_of() { git -C "$repo" rev-parse "refs/heads/$1"; }
short() { git -C "$repo" rev-parse --short=7 "$1"; }
tree_of() { git -C "$repo" rev-parse "$1^{tree}"; }
parents_of() { git -C "$repo" log -1 --format=%P "$1"; }
upstream_of() {
  local ref
  ref=$(git -C "$repo" config "branch.$1.merge")
  printf '%s\n' "${ref#refs/heads/}"
}
trailer() { # key commit
  git -C "$repo" log -1 --format="%(trailers:key=$1,valueonly)" "$2"
}
line() { # tree file n: line n of the file in that tree's working tree
  head -n "$3" "$volume/$1/$2" | tail -n 1
}
collision_refs() { git -C "$repo" for-each-ref --format='%(refname)' refs/collisions/; }
graph() { # tree: the history, without the padding git leaves at line ends
  local row
  git -C "$repo" log --graph --abbrev=7 --format='%h %s' "$1" >"$top/graph"
  while IFS= read -r row; do
    printf '%s\n' "${row%"${row##*[! ]}"}"
  done <"$top/graph"
}

# Trees --------------------------------------------------------------------

new_volume() { # name first_tree
  volume=$top/$1
  repo=$volume/$2
  open_files=' '
  mkdir -p "$volume"
  git init -q -b "$2" "$repo"
}

# A fork is a branch with its own working tree, whose upstream is another
# branch of the same repository (git-model.md §2).
new_fork() { # fork upstream
  git -C "$repo" worktree add -q -b "$1" "$volume/$1" "$2"
  git -C "$repo" config "branch.$1.remote" .
  git -C "$repo" config "branch.$1.merge" "refs/heads/$2"
}

# Write a whole file and commit it, naming one writer (R6).
commit_file() { # tree writer cause file line...
  local tree=$1 writer=$2 cause=$3 file=$4
  shift 4
  printf '%s\n' "$@" >"$volume/$tree/$file"
  git -C "$volume/$tree" add -- "$file"
  git -C "$volume/$tree" commit -q -F - <<EOF
$cause by $writer

Agentfs-Writer: $writer
Agentfs-Cause: $cause
Agentfs-Tree: $tree
EOF
}

seed() { commit_file "$1" agentfs Root "${@:2}"; }

# A program's close: a save, one commit on the tree's own branch.
save() { commit_file "$1" "$2" Direct "${@:3}"; }

open_file() { open_files="$open_files$1:$2 "; }
close_file() { open_files=${open_files/ $1:$2 / }; }
is_open() { [[ $open_files == *" $1:$2 "* ]]; }

# Move a tree's branch in one ref transaction that names the old value, and
# optionally create a collision ref in the same transaction. The working tree
# then follows, rewriting only the files that differ (git-model.md §3).
move_tree() { # tree old new [collision_ref commit]
  {
    printf 'start\n'
    printf 'update refs/heads/%s %s %s\n' "$1" "$3" "$2"
    if [ $# -gt 3 ]; then printf 'create %s %s\n' "$4" "$5"; fi
    printf 'prepare\ncommit\n'
  } | git -C "$repo" update-ref --stdin >/dev/null
  git -C "$volume/$1" read-tree -m -u "$2" "$3"
}

# The referee --------------------------------------------------------------

# Judge one file of a change, line by line. A line is its position here: every
# file in this demo keeps its number of lines, and the referee refuses any
# other. The real referee compares recorded line identities (R10), which this
# byte comparison only stands in for (see walkthrough.md).
#
#   saw  the line the writer saw: the change's first parent
#   up   the upstream's line now
#   in   the change's line
#
# Appends one number per collided line to lost_lines, and writes the merged
# file to $take/merged.
judge_file() { # first_parent head commit path
  local n=0 saw up in
  git -C "$repo" cat-file blob "$1:$4" >"$take/saw"
  git -C "$repo" cat-file blob "$2:$4" >"$take/up"
  git -C "$repo" cat-file blob "$3:$4" >"$take/in"
  if [ "$(wc -l <"$take/saw")" != "$(wc -l <"$take/up")" ] ||
    [ "$(wc -l <"$take/saw")" != "$(wc -l <"$take/in")" ]; then
    fail "$4: this demo's referee only handles files that keep their line count"
  fi
  : >"$take/merged"
  while IFS= read -r saw <&3 && IFS= read -r up <&4 && IFS= read -r in <&5; do
    n=$((n + 1))
    if [ "$in" = "$saw" ] || [ "$in" = "$up" ]; then
      # Not an edit in this change, or the same version: the upstream's stays.
      printf '%s\n' "$up" >>"$take/merged"
    elif [ "$up" = "$saw" ]; then
      # Written over the line the upstream holds (R3c): it replaces it.
      printf '%s\n' "$in" >>"$take/merged"
    else
      # Concurrent, each written without seeing the other: a collision, and
      # the upstream's line stays (R3b).
      printf '%s\n' "$up" >>"$take/merged"
      lost_lines="$lost_lines $n"
    fi
  done 3<"$take/saw" 4<"$take/up" 5<"$take/in"
}

# One queue take of one landing: `commit`, a commit of `fork`, into the fork's
# one upstream.
land() { # commit fork
  local commit=$1 fork=$2 upstream head first writer path tree merge n blob
  local collisions='' take_lost=0 lost_lines
  upstream=$(upstream_of "$fork")
  head=$(head_of "$upstream")
  first=$(git -C "$repo" rev-parse "$commit^1")

  # Made on the upstream's current head: a fast-forward, no landing commit (R6.2).
  if [ "$first" = "$head" ]; then
    move_tree "$upstream" "$head" "$commit"
    note "  landing $(short "$commit") into $upstream: fast-forward"
    return
  fi

  local take=$top/take
  rm -rf "$take"
  mkdir "$take"
  export GIT_INDEX_FILE=$take/index
  git -C "$repo" read-tree "$head"

  local ref=refs/collisions/$commit
  for path in $(git -C "$repo" diff-tree -r --no-renames --name-only "$first" "$commit"); do
    lost_lines=''
    judge_file "$first" "$head" "$commit" "$path"
    if [ -n "$lost_lines" ]; then
      take_lost=1
      for n in $lost_lines; do
        collisions="${collisions}Agentfs-Collision: $upstream $path Line $commit $ref line-$n
"
      done
    else
      blob=$(git -C "$repo" hash-object -w "$take/merged")
      git -C "$repo" update-index --add --cacheinfo "100644,$blob,$path"
    fi
  done

  # A change lands whole or loses whole (R3e): one lost line and the upstream's
  # tree stays as it was, the change's clean lines included.
  if [ "$take_lost" = 1 ]; then
    tree=$(tree_of "$head")
  else
    tree=$(git -C "$repo" write-tree)
  fi
  unset GIT_INDEX_FILE

  # The landing commit names the writer it carries, and records any decision
  # in its own message (git-model.md §3).
  writer=$(trailer Agentfs-Writer "$commit")
  merge=$(
    git -C "$repo" commit-tree "$tree" -p "$head" -p "$commit" -F - <<EOF
Landing by $writer

Agentfs-Writer: $writer
Agentfs-Cause: Landing
Agentfs-Tree: $upstream
$collisions
EOF
  )

  if [ "$take_lost" = 1 ]; then
    move_tree "$upstream" "$head" "$merge" "$ref" "$commit"
    note "  landing $(short "$commit") into $upstream: lost, upstream's lines stay"
  else
    move_tree "$upstream" "$head" "$merge"
    note "  landing $(short "$commit") into $upstream: merged per line"
  fi
}

# One queue take of a sync into `fork`. The upstream's commits the fork lacks
# are delivered in order, up to the first that touches a file the fork has open
# (R9.3). That commit and every later one are held back.
#
# This demo builds only the case where the fork can fast-forward. A fork with
# saves its upstream has not taken needs a sync commit (R6.1), which needs the
# real system's line map; the script stops rather than fake it.
sync_tree() { # fork
  local fork=$1 upstream fork_head up c file pending='' cut held=''
  upstream=$(upstream_of "$fork")
  fork_head=$(head_of "$fork")
  up=$(head_of "$upstream")

  reaches "$up" "$fork_head" ||
    fail "sync of $fork: it has saves $upstream has not taken; this demo does not build a sync commit"

  # The upstream's first parents, back to the first commit the fork has. Read by
  # ancestry alone: a range walk would stop early by commit date (MODEL.md
  # finding 5).
  for c in $(git -C "$repo" rev-list --first-parent "$up"); do
    if reaches "$fork_head" "$c"; then break; fi
    pending="$c $pending"
  done

  cut=$fork_head
  for c in $pending; do
    for file in $(git -C "$repo" diff-tree -r --no-renames --name-only "$c^1" "$c"); do
      if is_open "$fork" "$file"; then held=$file; fi
    done
    if [ -n "$held" ]; then break; fi
    cut=$c
  done

  if [ "$cut" != "$fork_head" ]; then move_tree "$fork" "$fork_head" "$cut"; fi
  if [ -n "$held" ]; then
    note "  sync of $fork: at $(short "$cut"), held back from $(short "$c") because $held is open"
  else
    note "  sync of $fork: at $(short "$cut"), nothing held back"
  fi
}

# Scenes -------------------------------------------------------------------

scene 'Scene 1: a fork records its changes and lands them'
new_volume one upstream
seed upstream notes.txt 'first' 'second' 'third'
new_fork fork upstream
base=$(head_of upstream)

save fork alice notes.txt 'first' 'second by alice' 'third'
c1=$(head_of fork)
save fork alice notes.txt 'first' 'second by alice' 'third by alice'
c2=$(head_of fork)

assert_eq 'each save is a commit on the fork' "$c1" "$(git -C "$repo" rev-parse "$c2^1")"
assert_eq 'the save names its writer (R6)' alice "$(trailer Agentfs-Writer "$c1")"
assert_eq 'the save says it is a save' Direct "$(trailer Agentfs-Cause "$c1")"
assert_eq 'the save names its tree' fork "$(trailer Agentfs-Tree "$c1")"
assert_eq 'the upstream has not moved' "$base" "$(head_of upstream)"

land "$c1" fork
land "$c2" fork

assert_eq 'the upstream is at the last save, with no landing commit (R6.2)' "$c2" "$(head_of upstream)"
assert_reaches 'the upstream history holds the first save on its own (R7)' upstream "$c1"
assert_eq 'the upstream working tree shows the change' 'third by alice' "$(line upstream notes.txt 3)"

scene 'Scene 2: the fork and its upstream edit different lines'
new_volume two upstream
seed upstream notes.txt 'one' 'two' 'three' 'four' 'five'
new_fork fork upstream

note 'Lines far apart: git merge and the referee agree.'
save upstream ursula notes.txt 'one by ursula' 'two' 'three' 'four' 'five'
save fork alice notes.txt 'one' 'two' 'three' 'four' 'five by alice'
git_tree=$(git -C "$repo" merge-tree --write-tree upstream fork)
fork_save=$(head_of fork)
before=$(head_of upstream)
land "$fork_save" fork
merge=$(head_of upstream)
assert_eq 'the landing is a merge: upstream first, the fork commit second' "$before $fork_save" "$(parents_of "$merge")"
assert_eq 'the landing names the fork writer (R6)' alice "$(trailer Agentfs-Writer "$merge")"
assert_eq 'the landing says what it is' Landing "$(trailer Agentfs-Cause "$merge")"
assert_eq 'the tree is the one git merge builds' "$git_tree" "$(tree_of "$merge")"
assert_eq 'no collision is recorded' '' "$(collision_refs)"
sync_tree fork

note 'Neighbouring lines: git merge conflicts, the referee does not (R3e.1).'
save upstream ursula notes.txt 'one by ursula' 'two by ursula' 'three' 'four' 'five by alice'
save fork alice notes.txt 'one by ursula' 'two' 'three by alice' 'four' 'five by alice'
git_status=0
git -C "$repo" merge-tree --write-tree upstream fork >"$top/git-merge" || git_status=$?
assert_eq 'git merge reports a conflict' 1 "$git_status"
land "$(head_of fork)" fork
assert_eq 'the upstream line survives' 'two by ursula' "$(line upstream notes.txt 2)"
assert_eq 'the fork line survives' 'three by alice' "$(line upstream notes.txt 3)"
assert_eq 'still no collision' '' "$(collision_refs)"

scene 'Scene 3: both sides edit the same line'
new_volume three upstream
seed upstream notes.txt 'first' 'second' 'third' 'fourth'
new_fork fork upstream

save upstream ursula notes.txt 'first' 'second by ursula' 'third' 'fourth'
save fork alice notes.txt 'first' 'second by alice' 'third' 'fourth by alice'
losing=$(head_of fork)
before=$(head_of upstream)

# The closest thing git offers: -X ours keeps the upstream's side of a conflict,
# yet still takes the change's clean line, which R3e forbids.
git -C "$repo" worktree add -q --detach "$volume/probe" upstream
git -C "$volume/probe" merge -q --no-edit -X ours fork >/dev/null
assert_eq 'git merge -X ours keeps the fork clean line' 'fourth by alice' "$(line probe notes.txt 4)"
git -C "$repo" worktree remove --force "$volume/probe"

land "$losing" fork
result=$(head_of upstream)
ref=refs/collisions/$losing

assert_eq 'the upstream line wins (R3b)' 'second by ursula' "$(line upstream notes.txt 2)"
assert_eq 'the clean line of the losing save is gone too (R3e)' 'fourth' "$(line upstream notes.txt 4)"
assert_eq 'the upstream tree did not change' "$(tree_of "$before")" "$(tree_of "$result")"
assert_eq 'the loss is a merge in the upstream history' "$before $losing" "$(parents_of "$result")"
assert_eq 'one collision ref, kept for the losing commit' "$ref" "$(collision_refs)"
assert_eq 'the ref holds the losing commit' "$losing" "$(git -C "$repo" rev-parse "$ref")"
assert_eq 'the result commit records the decision' \
  "upstream notes.txt Line $losing $ref line-2" "$(trailer Agentfs-Collision "$result")"
assert_eq 'the losing line stays readable' 'second by alice' \
  "$(git -C "$repo" show "$ref:notes.txt" | head -n 2 | tail -n 1)"
assert_eq 'the losing commit names its writer' alice "$(trailer Agentfs-Writer "$ref")"
assert_eq 'the fork still shows its own line until it syncs (R3a)' 'second by alice' "$(line fork notes.txt 2)"

graph upstream

sync_tree fork
assert_eq 'the sync brings the decision: a fast-forward to the upstream' "$result" "$(head_of fork)"
assert_eq 'the fork now shows the upstream line' 'second by ursula' "$(line fork notes.txt 2)"
assert_eq 'the fork clean line went back too' 'fourth' "$(line fork notes.txt 4)"

note 'A save made after the sync has seen the upstream line, so it wins (R3c).'
save fork alice notes.txt 'first' 'second, alice again' 'third' 'fourth'
land "$(head_of fork)" fork
assert_eq 'the fork line replaces the upstream line' 'second, alice again' "$(line upstream notes.txt 2)"
assert_eq 'and records no second collision' "$ref" "$(collision_refs)"

scene 'Scene 4: a sync held back in order while a file is open'
new_volume four upstream
seed upstream notes.txt 'notes'
seed upstream other.txt 'other'
new_fork fork upstream

save upstream ursula other.txt 'other 1'
u1=$(head_of upstream)
save upstream ursula other.txt 'other 2'
u2=$(head_of upstream)
save upstream ursula notes.txt 'notes from ursula'
u3=$(head_of upstream)
save upstream ursula other.txt 'other 3'
u4=$(head_of upstream)
note "  upstream commits, in order: $(short "$u1") $(short "$u2") $(short "$u3") $(short "$u4")"
note "  only $(short "$u3") touches notes.txt"

open_file fork notes.txt
sync_tree fork
assert_eq 'delivered in order up to the commit before the open file' "$u2" "$(head_of fork)"
assert_reaches 'the first commit arrived' fork "$u1"
assert_not_reaches 'the commit touching the open file is held back' fork "$u3"
assert_not_reaches 'and so is the commit after it, which does not touch the file' fork "$u4"
assert_eq 'the open file is untouched' 'notes' "$(line fork notes.txt 1)"
assert_eq 'the other file moved on' 'other 2' "$(line fork other.txt 1)"

note 'The program closes the file without changing a byte: no save (R8.2).'
close_file fork notes.txt
sync_tree fork
assert_eq 'the rest arrives' "$u4" "$(head_of fork)"
assert_eq 'the file shows the upstream change' 'notes from ursula' "$(line fork notes.txt 1)"
assert_eq 'the other file is current' 'other 3' "$(line fork other.txt 1)"

scene 'Scene 5: a fork of a fork'
new_volume five f0
seed f0 notes.txt 'first' 'second' 'third'
new_fork f1 f0
new_fork f2 f1

note 'Fork f2, upstream f1: the lines differ, so the landing applies.'
save f1 bob notes.txt 'first' 'second' 'third by bob'
bob=$(head_of f1)
save f2 carol notes.txt 'first by carol' 'second' 'third'
carol=$(head_of f2)
land "$carol" f2
carried=$(head_of f1)
assert_eq 'f1 holds carol line' 'first by carol' "$(line f1 notes.txt 1)"
assert_eq 'f1 holds bob line' 'third by bob' "$(line f1 notes.txt 3)"
assert_eq 'the landing in f1 names carol' carol "$(trailer Agentfs-Writer "$carried")"

note 'Fork f1, upstream f0: the same referee, one level up.'
save f0 dana notes.txt 'first by dana' 'second' 'third'
land "$bob" f1
land "$carried" f1
result=$(head_of f0)
ref=refs/collisions/$carried
assert_eq 'bob line landed in f0' 'third by bob' "$(line f0 notes.txt 3)"
assert_eq 'the f0 line wins against carol line (R3b)' 'first by dana' "$(line f0 notes.txt 1)"
assert_eq 'the collision is recorded once' "$ref" "$(collision_refs)"
assert_eq 'the decision is in f0' \
  "f0 notes.txt Line $carried $ref line-1" "$(trailer Agentfs-Collision "$result")"
assert_eq 'the carried change still names carol' carol "$(trailer Agentfs-Writer "$result")"
assert_eq 'f1 keeps its own line until it syncs (R3a)' 'first by carol' "$(line f1 notes.txt 1)"

note 'Each fork learns the decision from its own one upstream.'
sync_tree f1
assert_eq 'f1 shows the f0 line' 'first by dana' "$(line f1 notes.txt 1)"
assert_eq 'f2 still shows its own line' 'first by carol' "$(line f2 notes.txt 1)"
sync_tree f2
assert_eq 'f2 shows the f0 line' 'first by dana' "$(line f2 notes.txt 1)"
assert_eq 'f2 shows bob line' 'third by bob' "$(line f2 notes.txt 3)"
assert_reaches 'carol commit stays in the f0 history (R7)' f0 "$carol"

printf '\nall scenes passed\n'
