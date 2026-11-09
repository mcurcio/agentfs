# The fork and upstream rules in plain git

[`demo.sh`](demo.sh) shows how a fork and its upstream work, using only `git` and coreutils: no Rust and no agentfs
binary. It builds throwaway repositories under a `mktemp` directory and removes them when it ends. It needs no network
and no secrets. Each scene checks its results, and a wrong result stops the script with a non-zero exit.

The rule ids below (R3b and so on) are those of [the rules ledger](../rules-ledger.md). What each rule means in git is
in [agentfs in git terms](../git-model.md). [The Rust model](../../../crates/agentfs-model/MODEL.md) is still where
these rules are tested. This page does not replace it.

## What this demo does not cover

- **Line identity.** The real referee compares line identities that each save records (R10). This demo compares bytes
  at one position. Its files keep their number of lines, and its referee stops on any file that does not. So it shows
  no insert, delete, move or rename (R10, R11). A line written from A to B and back to A would fool it (MODEL.md
  finding 13); it would not fool the real referee.
- **Queues and locks.** The script calls each landing and sync by hand, in order. There is no queue (R2) and no lock
  that waits (R9). An "open file" here is a name in a shell variable. Nothing holds a descriptor.
- **Sync commits.** A fork with saves its upstream has not taken needs a sync commit that keeps its undecided lines
  (R3a, R3d, R6.1). That takes the line map. The demo builds only syncs that fast-forward, and it stops rather than
  fake the other kind.
- **Saves.** A save here is a whole file written and committed. There are no byte calls (R8), no journal (R12), no
  transactions (R5) and no filesystem.
- **The record of a decision.** The real `Agentfs-Collision` trailer names line identities and versions (git-model.md
  §3). The demo's trailer gives the line number instead.
- **Remotes.** The demo has none. A remote such as GitHub is a mirror of a tree, not an upstream.

## Running it

```sh
bash docs/target-architecture/demo/demo.sh
```

Every commit has a fixed date, so every run prints the same commit ids. The output below is the script's own output,
and CI fails if a line of it is missing from this page.

Each scene is one repository. A tree is a branch with its own working tree, and a fork records its one upstream in git's
own config:

```sh
git -C "$repo" worktree add -q -b "$1" "$volume/$1" "$2"
git -C "$repo" config "branch.$1.remote" .
git -C "$repo" config "branch.$1.merge" "refs/heads/$2"
```

The service identity is the same for every commit. A trailer names the writer (R6).

## Scene 1: a fork records its changes and lands them

The fork saves twice. Each save is a commit on the fork's own branch, and the upstream does not move. Then each save
lands, one at a time. Each was made on the upstream's current head, so each landing is a fast-forward and adds no
commit (R6.2). Every fork commit ends up in the upstream's history (R7).

A branch moves in one ref transaction that names the old value, and the working tree follows:

```sh
git -C "$volume/$1" read-tree -m -u "$2" "$3"
```

```text
== Scene 1: a fork records its changes and lands them ==
  ok: each save is a commit on the fork
  ok: the save names its writer (R6)
  ok: the save says it is a save
  ok: the save names its tree
  ok: the upstream has not moved
  landing 067ae5f into upstream: fast-forward
  landing 470889e into upstream: fast-forward
  ok: the upstream is at the last save, with no landing commit (R6.2)
  ok: the upstream history holds the first save on its own (R7)
  ok: the upstream working tree shows the change
```

## Scene 2: the fork and its upstream edit different lines

Here the real system differs from `git merge`, so the demo builds the step itself. Git treats neighbouring changed lines
as one conflict. The real referee judges each line (R3e.1), and so does the demo's `judge_file`. It reads three
versions of each file the change touched: the line the writer saw (the change's first parent), the upstream's line
now, and the change's line. Then it builds the commit from plumbing:

```sh
git -C "$repo" commit-tree "$tree" -p "$head" -p "$commit" -F - <<EOF
```

The scene runs twice on one file. With lines far apart, the tree the referee builds is the very tree `git merge-tree`
builds. With neighbouring lines, `git merge-tree` exits 1 with a conflict, and the referee keeps both lines.

```text
== Scene 2: the fork and its upstream edit different lines ==
Lines far apart: git merge and the referee agree.
  landing b27b687 into upstream: merged per line
  ok: the landing is a merge: upstream first, the fork commit second
  ok: the landing names the fork writer (R6)
  ok: the landing says what it is
  ok: the tree is the one git merge builds
  ok: no collision is recorded
  sync of fork: at b010233, nothing held back
Neighbouring lines: git merge conflicts, the referee does not (R3e.1).
  ok: git merge reports a conflict
  landing 982b91f into upstream: merged per line
  ok: the upstream line survives
  ok: the fork line survives
  ok: still no collision
```

## Scene 3: both sides edit the same line

The fork and its upstream each change line 2, and neither saw the other's line. The upstream's queue decides it, once
(R3a), and the upstream's line wins (R3b). The fork's save also changed line 4, which nobody else touched. A change
lands whole or loses whole (R3e), so line 4 does not land either.

`git merge -X ours` is the closest thing git offers, and the scene shows that it is not the rule: it keeps the
upstream's side of the conflict but still takes the fork's clean line 4.

The loss is recorded, not thrown away. The upstream gets a merge commit whose tree is unchanged and whose second parent
is the losing commit. One ref transaction moves the branch and creates `refs/collisions/<losing commit>`, and the
merge's `Agentfs-Collision` trailer names the file, the losing commit, the ref and the line. The transaction is what
the script feeds `git update-ref --stdin`:

```text
start
update refs/heads/upstream <new head> <old head>
create refs/collisions/<losing commit> <losing commit>
prepare
commit
```

The history of the upstream then reads:

```text
*   bfaab55 Landing by alice
|\
| * 48f58e6 Direct by alice
* | 286d506 Direct by ursula
|/
* abb1ca0 Root by agentfs
```

The fork keeps showing its own line until it syncs (R3a). The sync is a fast-forward, because the upstream's history
now reaches the fork's commit. A later save on top of the upstream's line has seen it, so it replaces it with no
collision (R3c).

```text
== Scene 3: both sides edit the same line ==
  ok: git merge -X ours keeps the fork clean line
  landing 48f58e6 into upstream: lost, upstream's lines stay
  ok: the upstream line wins (R3b)
  ok: the clean line of the losing save is gone too (R3e)
  ok: the upstream tree did not change
  ok: the loss is a merge in the upstream history
  ok: one collision ref, kept for the losing commit
  ok: the ref holds the losing commit
  ok: the result commit records the decision
  ok: the losing line stays readable
  ok: the losing commit names its writer
  ok: the fork still shows its own line until it syncs (R3a)
  sync of fork: at bfaab55, nothing held back
  ok: the sync brings the decision: a fast-forward to the upstream
  ok: the fork now shows the upstream line
  ok: the fork clean line went back too
A save made after the sync has seen the upstream line, so it wins (R3c).
  landing 055d639 into upstream: fast-forward
  ok: the fork line replaces the upstream line
  ok: and records no second collision
```

## Scene 4: a sync held back in order while a file is open

The upstream makes four commits. Only the third touches `notes.txt`, which a program in the fork has open. A sync
delivers the upstream's commits in order, up to the first that touches the open file, and holds back that one and every
later one (R9.3). So the fourth commit waits too, though it touches a different file.

The commits come from the upstream's first parents, walked back to the first commit the fork has:

```sh
git -C "$repo" rev-list --first-parent "$up"
```

In the real system the waiting sync is woken by the close. Here the script calls the sync again. The program closes the
file without changing a byte, so there is no save (R8.2), and the fork can still fast-forward.

```text
== Scene 4: a sync held back in order while a file is open ==
  upstream commits, in order: 810d0db 333e26a f88da43 e51cc21
  only f88da43 touches notes.txt
  sync of fork: at 333e26a, held back from f88da43 because notes.txt is open
  ok: delivered in order up to the commit before the open file
  ok: the first commit arrived
  ok: the commit touching the open file is held back
  ok: and so is the commit after it, which does not touch the file
  ok: the open file is untouched
  ok: the other file moved on
The program closes the file without changing a byte: no save (R8.2).
  sync of fork: at e51cc21, nothing held back
  ok: the rest arrives
  ok: the file shows the upstream change
  ok: the other file is current
```

## Scene 5: a fork of a fork

There are three trees, but every step has one fork and its one upstream. `land` and `sync_tree` take a fork and read its
upstream from git's config, so the code has no idea how deep it is.

First, fork f2 and its upstream f1. Their lines differ, so f2's change lands in f1 as a merge naming its writer, carol.

Then fork f1 and its upstream f0. The landing commit f1 made is now an ordinary fork commit. f0 has changed the same
line, so that commit loses whole, and f0's queue records the collision once (R3a, R3b). The record is in f0, and
the commit still names carol.

Then each fork learns the decision from its own one upstream: f1 syncs from f0, and only after that can f2 learn it,
by syncing from f1.

```text
== Scene 5: a fork of a fork ==
Fork f2, upstream f1: the lines differ, so the landing applies.
  landing 5312579 into f1: merged per line
  ok: f1 holds carol line
  ok: f1 holds bob line
  ok: the landing in f1 names carol
Fork f1, upstream f0: the same referee, one level up.
  landing 64e0dd4 into f0: merged per line
  landing 66907c1 into f0: lost, upstream's lines stay
  ok: bob line landed in f0
  ok: the f0 line wins against carol line (R3b)
  ok: the collision is recorded once
  ok: the decision is in f0
  ok: the carried change still names carol
  ok: f1 keeps its own line until it syncs (R3a)
Each fork learns the decision from its own one upstream.
  sync of f1: at 1472f5e, nothing held back
  ok: f1 shows the f0 line
  ok: f2 still shows its own line
  sync of f2: at 1472f5e, nothing held back
  ok: f2 shows the f0 line
  ok: f2 shows bob line
  ok: carol commit stays in the f0 history (R7)

all scenes passed
```
