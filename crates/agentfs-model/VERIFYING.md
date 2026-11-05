# Verifying a design change with the model

This page says how agentfs's landing rules are checked before anything is built on them: what to run, when,
and how to tell whether a passing run means anything. It is for maintainers changing the rules and for
reviewers of a design pull request. The rules themselves live in [MODEL.md](MODEL.md) (the rule table, the
invariants, the findings) and [agentfs in git terms](../../docs/target-architecture/git-model.md) (the
foundation the implementation follows); this page does not restate them.

## What the model is for

`crates/agentfs-model` runs the target design on the real `git` program: main and each sandbox are branches,
every save is a commit, and the rules decide what each tree shows. It is tests only and depends on no other
agentfs crate. A design rule counts as designed when the model encodes it, its scenario tests show it, and
every invariant holds after every step of every run. A rule that exists only in prose is not yet designed.

## When to run what

| You changed | Run |
|---|---|
| A rule, a check or the model's code | the crate's tests at the CI seeds, then 500 seeds, then the ignored findings |
| A rule you believe the checks already cover | the same, plus at least one planted bug the change should be caught by |
| `MODEL.md` or `git-model.md` only | the mdBook build and the docs reference test |
| Anything, before pushing | fmt, clippy and the model's tests; CI runs the rest of the workspace |

## Commands

All commands run from the repository root. Without a local Rust toolchain (on macOS, say), run them in the
container from [CONTRIBUTING.md](../../CONTRIBUTING.md) ("A local Linux gate in a container"): the model
needs only `git`, which the Rust image has. Pass the seed variables into the container with `-e`, or they
are silently dropped. A whole command, 500 seeds in the container:

```sh
AGENTFS_MODEL_SEEDS=500 docker run --rm -v "$PWD":/src -v agentfs-target:/target \
  -v agentfs-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target \
  -e AGENTFS_MODEL_SEEDS -e AGENTFS_MODEL_SEED -w /src agentfs-dev \
  cargo test --locked -p agentfs-model --test interleavings
```

Keep the build output and the registry on a disk with room; a full workspace build is several GB. The
commands below are what goes after `agentfs-dev` in that line:

```sh
# The model: every scenario, and the random runs at seeds 1..64 (what CI runs).
cargo test --locked -p agentfs-model

# One rule's scenarios (they are named r<N>_... after the rule they show).
cargo test --locked -p agentfs-model --test scenarios r9_

# More random runs: seeds 1..500. Run this before calling a rule change done.
AGENTFS_MODEL_SEEDS=500 cargo test --locked -p agentfs-model --test interleavings

# Replay one failing seed under the test that failed; the step log prints only on a failure.
AGENTFS_MODEL_SEED=37 cargo test --locked -p agentfs-model --test interleavings \
  random_interleavings_squashed -- --nocapture

# The open and rejected findings: each asserts what a rule intends and is expected to fail (below).
cargo test --locked -p agentfs-model --no-fail-fast -- --ignored

# Lint, as CI's lint job runs it.
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings

# Links: every relative link in README.md and docs/** resolves.
cargo test --locked -p agentfs-core --test docs_reference
```

Run the model's tests locally, not `cargo test --workspace`: the FUSE crates' tests belong to CI (see
"Safety"), and in this container one of them, `agentfs-fuse`'s
`creds::tests::as_root_the_caller_owns_what_it_creates`, fails because the container runs without the
capabilities and the FUSE device a mount needs. CI runs the whole workspace on every push.

The link test does not read this page, MODEL.md or CONTRIBUTING.md: check the links you change in them by
hand.

The book: CI builds it with mdBook v0.5.4, which is not in the image. mdBook runs `python3
ci/book-links.py` as a preprocessor (book.toml), so it needs `python3`, which the Rust image has. Any `WARN`
line fails CI, so treat one as a failure locally too. In the development image, on an x86_64 or aarch64 machine:

```sh
docker run --rm -v "$PWD":/src -w /src agentfs-dev sh -c '
  v=v0.5.4; t=mdbook-$v-$(uname -m)-unknown-linux-musl.tar.gz
  curl -fsSL https://github.com/rust-lang/mdBook/releases/download/$v/$t | tar -xz -C /usr/local/bin
  mdbook build 2>/tmp/mdbook.log; s=$?; cat /tmp/mdbook.log
  [ $s = 0 ] && ! grep -q WARN /tmp/mdbook.log'
```

The output goes to `target/book`. CI checks the tarball's sha256 (`.github/workflows/docs.yml`); this
local build does not.

The random seeds are every seed in `1..=n`, never a set picked because it passes. Every failure prints its
seed, the step, the broken invariant, a `replay: AGENTFS_MODEL_SEED=<n>` line and the step log. A replay
runs that one seed under every random-run test, so name the test that failed, as above.

### What the ignored tests are expected to show

Each ignored test fails, and its ignore reason (`coherence finding N: <what fails>`) says why. A run where
one passes, or fails for a different reason (a panic, another invariant), is a change to investigate, not
the expected failure.

| Test | Finding | Fails with |
|---|---|---|
| `finding_1_git_merge_base_on_crossing_merges` | 1 | a sync leaves a line "which Main already took" |
| `random_interleavings_git_merge_base` | 1 | 34 of 64 seeds, 1 of them (seed 46) a "harness panic: a model file in git": a sync that leaves a line "which Main already took" or drops a line (with no P3 and the prefix sync; 35 after the fix pass of the review of 2026-11-04, whose generator changes move the random calls; with creates, crashes and the progress rules 36; with the byte calls alone 32; before them 39 of 64, 2 of them a "harness panic: a model file in git", when git's merge leaves one line identity twice) |
| `finding_6_git_hunks_on_neighbouring_lines` | 6 | a sync leaves a line "which Main already took" |
| `finding_27_a_nested_dropped_delete_decided_by_the_history_setting` | 27 | `assertion left == right failed`: under full lineage main's `a` has six lines, under `Squashed` five |

These were the results at seeds 1..64 on 2026-11-04; a change that moves a count says so in its pull
request. Finding 3's two tests went with the per-file setting when the owner chose whole changes (rule 3e); finding
24's test passes since the owner deleted P3 (2026-11-04), as `r9_cooperating_programs_never_deadlock`.

CI runs `cargo test --workspace` (so the scenarios and seeds 1..64; `random_interleavings_history_settings_decide_the_same`
runs each seed eight times, so it takes seeds 1..16 unless a seed variable is set), fmt, clippy with `-D warnings`, and the
mdBook build. It does not run 500 seeds, the ignored tests or planted bugs: those stay local, and a design pull
request says in its body that they were run and what they showed.

## The procedure

1. **Encode the rule in the model first.** Change `src/world.rs` (and `src/content.rs` for what a file holds)
   so the model does what the rule says. Add or change the rule's row in MODEL.md's table: the rule in plain
   words, its source, and the scenario tests that show it. The source is either an owner decision, quoted
   and dated in MODEL.md's list of the owner's words, or a design choice, marked *choice* in the row. Never
   present a choice as an owner decision. MODEL.md's list is the authoritative record of the owner's words.
2. **Let the invariants judge every step.** `src/check.rs` holds the invariants. The scenarios and the random
   runs call them after every step, under every history setting (full lineage and squashed), and the random
   runs also check convergence once writes stop. A rule is only as safe as the invariants that would notice
   it breaking.
3. **Prove the checks can fail, by planting bugs.** A test that passes proves nothing until a wrong model
   makes it fail. See "Planting bugs" below.
4. **Have it reviewed adversarially.** A reviewer who did not take part in the design reads the model and the
   docs and attacks two things: coherence (do the rules contradict each other or the owner's words?) and
   fidelity (do the checks catch what they claim?). The reviewer plants their own bugs, runs the tests in
   the container, and reports findings ranked P0 (a rule or invariant is wrong or unchecked), P1 (a gap that
   can hide a bug) and P2 (clarity), each with file and line. Then make one fix pass. If that fix pass is not
   reviewed again, the pull request says so.
5. **Record what cannot hold, never weaken a check.** When the model shows a rule cannot be kept as stated,
   write a test that asserts the rule's intent and mark it
   `#[ignore = "coherence finding N: <what fails>"]`, and add finding N to MODEL.md. Do not loosen an
   invariant, add an excuse to it, or narrow the random runs so the suite passes. A rejected alternative keeps
   its ignored test as the evidence for rejecting it. When it is a policy setting in `Policies`, the test
   lands with it (findings 1 and 6: `MergeRule::GitMergeBase`). When it replaces
   the model's code, its tests cannot land without that code: push it as a branch that is never merged,
   and name the branch and commit in its finding (finding 13 does this).
6. **Docs follow the model.** In the same pull request, update MODEL.md (rules, invariants, findings, costs)
   and git-model.md (the foundation). Owner decisions made during the work go into MODEL.md's list of the
   owner's words (step 1); the pull request body quotes them too, so a reviewer sees them in one place. CI is the gate: fmt, clippy, the tests and the mdBook build must be green.

## Adding a rule, a scenario or an invariant

- **A scenario** goes in `tests/scenarios.rs`, named `r<N>_<what it shows>`. Use the helpers there: `take`
  steps and checks every invariant, `next` takes one queued change whole, `drain` runs every pending step and
  then checks convergence. Run it under both history settings (`BOTH`) unless the rule is about one of them.
  Assert the outcome you expect (what each tree shows, what the collision names), not only that nothing
  failed. List the scenario in the rule's row.
- **A random-run action** goes in `tests/interleavings.rs`. The generator is seeded and written there, so
  every run replays exactly; keep it that way (no system randomness, no clock). A new kind of call in the
  random runs is how a rule meets every other rule. Make every byte call through `Oracle::write`, register every open
  with `Oracle::opened`, make every unlink and rename through `Oracle::path_call`, close through `close_checked`,
  end a client through `end_checked` and crash through `crash_checked`, so the byte oracle and its object table
  see the same calls the model does;
  after a step a referee takes (a queue step, a transaction's commit) the oracle reads the trees again.
- **An invariant** goes in `src/check.rs`, called from `check` (after any step), `step_checked` (what only the
  step itself shows) or `check_converged` (once writes stop). Read the truth from git or from the harness's own
  record, not from the model's bookkeeping it is meant to check: a check derived from the model's own record
  agrees with the model's bugs. Add it to MODEL.md's "What must always hold" table, then plant a bug it alone
  should catch.

## Planting bugs (mutation probes)

A planted bug is one deliberate mistake in a copy of the model. It shows which tests protect a rule.

1. Copy the working tree, uncommitted changes included, to a scratch directory on a disk with room. The
   crate inherits workspace settings, so copy the repository, not just the crate; and copy the working tree,
   not `HEAD`, or the bugs go into the model as it was before your change:
   `mkdir -p <scratch>/m1 && git ls-files -z -co --exclude-standard | tar --null -T - -cf - | tar -xf - -C <scratch>/m1`.
   (Restore or commit a deleted tracked file first: `tar` stops on a listed file that is gone.)
2. Plant exactly one bug per copy, in the code a rule depends on. Bugs that have proved useful: a sync that
   decides a collision itself; the sandbox winning instead of the parent; dropping a flag a later step reads;
   skipping a lock release when a client dies; an append that ignores where the file ends; skipping the ref
   that keeps a losing commit reachable; applying the lines of a losing save that did not collide; letting a
   transaction's clean lines apply when one of its lines lost; naming a losing save in a squash's graft; judging
   a squashed take as one change; a sync that reads only lineage, not the parent's collision notes; a squash that
   takes a save from behind another entry of the queue; a file-rule loss that does not cost its whole change; a judge that skips one of a take's changes (only the
   `gitledger` check sees it at the step); a landing that locks only the files its plan writes (the random runs
   catch it only at 500 seeds); an append onto a partial last line that adds a line instead of finishing it; a
   hole filled with other bytes than NUL; Replication dropping the grafts; a collision note on the wrong commit;
   placing an incoming insert before the parent's at one place; letting an edit of a file the parent unlinked
   apply; an entry going ahead of an earlier waiting one it shares a file with, or one of its own sandbox (P1); an
   atomic save that
   leaves the replaced file's old object at its path; a crash whose replay saves an open a save already names, or
   drops the journal. From the review of 2026-11-04: a crash replay that re-links each open to its file identity
   (a dead handle saved into the file that replaced it); a waiting take that records only its files open now; a
   crash that drops a transaction's journaled calls; a squash
   not cut at a save sharing a file with a waiting entry ahead; a squash that carries a save touching a file open for
   writing, or whose first save waits with followers. From the owner's decisions of 2026-11-04: P3 reintroduced (a
   new write-open waits behind a waiting take); a sync that delivers the parent's head whatever is open (past an open
   file); a sync that never delivers a prefix (it waits whole); no alarm on an open that keeps a take waiting; and
   each fix of findings 11, 25 and 26 reverted. From the review of `d552b3d`: a prefix sync that reads main's
   decision on a line whose carrying landing it holds back; one that reads the notes of the commits it holds
   back; no alarm on an open granted at once. (No alarm can be due on an open granted at a take's release: the
   model asserts it, `World::grant`, so the review's planted bug there changed no behaviour.)
3. Run `cargo test --locked -p agentfs-model --no-fail-fast` in the copy (and 500 seeds for a bug the
   scenarios miss), with a separate target directory per copy: in the container, mount the copy as `/src`
   and a volume of its own as `/target` (`-v agentfs-target-m1:/target`).
4. Record, per bug, which tests failed and how many seeds.

What counts: a bug is caught when an invariant or a scenario assertion fails because of it. A build error or
a harness panic unrelated to the rule does not count. A check that no planted bug trips is not counted as
protection: either plant a bug it should catch, or say in the pull request that it is unproven. A bug that
nothing catches is a finding: add the missing check, then confirm it now catches the bug.

Planted bugs stay local. CI does not run them; record the results in the pull request body.

## Reading a failure

Start from the printed seed and step log, replay the seed under the test that failed, and decide which of
three things failed. `random_interleavings_commit_dates_decide_nothing` first requires each run to pass on
its own, so a failure there that names an invariant is that invariant failing; only a "left != right"
mismatch between two passing runs says that commit dates changed a decision.

| Kind | Signs | What to do |
|---|---|---|
| Model bug | the model does something the rule's text does not say; a scenario of that rule fails too, or a small scenario built from the log does | fix the model; add the small scenario |
| Harness bug | the random run asked for something no client can do, or the check reads the model's own record and disagrees with git; "harness panic" in the message | fix the harness or the check, and plant a bug to show the fixed check still catches what it should |
| Design finding | the model follows the rule exactly and an invariant still breaks: the rules contradict each other or an owner requirement | record it as a coherence finding with an ignored test (procedure step 5) and ask the owner; do not change the rule on your own |

Never re-run until a seed passes, skip a seed, or lower the seed count to get green.

## Reviewer checklist for a design pull request

- Every changed or new rule has a MODEL.md row with its source: an owner quote with its date, or *choice*.
- Every rule row names scenarios that exist and assert the outcome, under both history settings where it
  applies.
- New invariants read from git or the harness's own record, and appear in MODEL.md's table.
- No invariant was weakened, excused or skipped; any rule that cannot hold is an ignored finding test.
- The body reports: CI seeds green, 500 seeds run (and the result), the ignored tests run, the planted bugs
  and what caught each, and checks that were not run, listed as not run.
- An adversarial review happened; its P0 and P1 findings are fixed or recorded; an unreviewed fix pass says
  so.
- MODEL.md and git-model.md describe the code as committed; owner decisions are in MODEL.md's list of the
  owner's words, and quoted in the body.
- CI is green: fmt, clippy, tests, mdBook.

## Safety

- The model runs `git` in temporary directories only; it never mounts a filesystem. Do not add FUSE mounts,
  root or kernel interfaces to it.
- On a shared or development machine, never write to `/sys/fs/fuse/connections`, never restart the container
  engine to clear a hang, and never run probes meant to deadlock a FUSE mount. The FUSE crates' tests belong
  to CI.
- Build output, scratch copies for planted bugs and long seed logs go on a disk with room, not a small system
  disk.

## Phase 1: the model as the conformance suite (integration still due)

[Phase 1](../../docs/target-architecture/phase-1.md) builds the proposed design so real end-to-end tests
can validate this model. The tree actor and mounted recording scenarios exist. The shared conformance
integration is still due: passing the byte and recording tests alone does not validate landing/sync or
complete phase 1. Run the same supported scenarios and random calls against the implementation, so its
visible bytes, history, collisions and progress are held to these rules. Two integration options:

- **A shared trait.** The calls the tests make (open, write, close, unlink, rename, take a queue step, begin
  and end a transaction, sync, read a tree, read the history) become one trait. The model implements it,
  and so does the real system's domain layer. The scenarios, the random runs and `check.rs` run against
  either.
- **A harness on the real mount.** A driver runs the same scenarios and seeds against a real FUSE mount and
  its git repository, and the invariants read the real repository. Slower, and it needs Linux with
  `/dev/fuse`, so it would run in CI.

Either way, the invariants must keep reading git and the harness's own record, never the implementation's
bookkeeping.
