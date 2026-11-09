# Verifying a design change with the model

This page says how agentfs's landing rules are checked before anything is built on them: what to run, when,
and how to tell whether a passing run means anything. It is for maintainers changing the rules and for
reviewers of a design pull request. Which rules exist, and whether the owner approved each, is the
[rules ledger](../../docs/target-architecture/rules-ledger.md). The rules' full wording lives in [MODEL.md](MODEL.md) (the rule table, the
invariants, the findings) and [agentfs in git terms](../../docs/target-architecture/git-model.md) (the
foundation the implementation follows); this page does not restate them.

## What the model is for

`crates/agentfs-model` runs the target design on the real `git` program: main and each fork are branches,
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
# The model: every scenario, and the random runs at seeds 1..64, rounds at 1..16 (what CI runs).
cargo test --locked -p agentfs-model

# One rule's scenarios (they are named r<N>_... after the rule they show).
cargo test --locked -p agentfs-model --test scenarios r9_

# More random runs: seeds 1..500. Run this before calling a rule change done.
AGENTFS_MODEL_SEEDS=500 cargo test --locked -p agentfs-model --test interleavings

# Replay one failing seed under the test that failed; the step log prints only on a failure.
AGENTFS_MODEL_SEED=37 cargo test --locked -p agentfs-model --test interleavings \
  random_interleavings_full_lineage -- --nocapture

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
| `random_interleavings_git_merge_base` | 1 | 15 of 64 seeds since the one-step take of finding 49, whose generator draws differently (2026-11-08, none a harness panic); 38 since the name locks of finding 46 (2026-11-07; 40 before them), 2 of them (seeds 32 and 46) a "harness panic: a model file in git": a sync that leaves a line "which Main already took" or drops a line (with the calls of programs outside a transaction, 2026-11-05; 34 with no P3 and the prefix sync; 35 after the fix pass of the review of 2026-11-04, whose generator changes move the random calls; with creates, crashes and the progress rules 36; with the byte calls alone 32; before them 39 of 64, 2 of them a "harness panic: a model file in git", when git's merge leaves one line identity twice) |
| `finding_6_git_hunks_on_neighbouring_lines` | 6 | a sync leaves a line "which Main already took" |
| `finding_30_a_transactions_line_after_the_forks_unfinished_line` | 30 | two lines read as one, `}n9` |

Findings 36 to 49 pass in `crates/agentfs-conformance/tests/replay.rs` since the owner's answers (48 and 49 on 2026-11-08). These
were the results at seeds 1..64 on 2026-11-05, unchanged on 2026-11-06; a change that moves a count says so in
its pull request. Findings 29, 31, 32 and 33 pass since the owner's rules R10.6, R10.3, R10.8 and R10.9, under
`r10_*` names (MODEL.md); finding 35 passes since the owner's answer of 2026-11-06 under R10.6, under its own name. Finding 27's and finding 28's tests pass since the owner's decisions of 2026-11-05 (one of them went
with squashing; finding 28's are `r10_two_deletes_of_one_line_agree_over_an_unseen_edit` and
`r10_a_later_delete_agrees_with_the_winning_delete`). Finding 3's two tests went with the per-file setting when the owner chose whole changes (rule 3e); finding
24's test passes since the owner deleted P3 (2026-11-04), as `r9_cooperating_programs_never_deadlock`.

CI runs `cargo test --workspace` (so the scenarios, seeds 1..64 and rounds at 1..16), fmt, clippy with `-D warnings`, and the
mdBook build. It does not run 500 seeds, the ignored tests or planted bugs: those stay local, and a design pull
request says in its body that they were run and what they showed.

## The procedure

1. **Encode the rule in the model first.** Change `src/world.rs` (and `src/content.rs` for what a file holds)
   so the model does what the rule says. The rule must already be in the
   [rules ledger](../../docs/target-architecture/rules-ledger.md) as `owner` or `approved`. A rule that is not
   there goes to the owner as a question first, with a recommendation, and is built only after he answers.
   Then add or change the rule's row in MODEL.md's table: the rule in plain words, its ledger ID, and the
   scenario tests that show it. Never present a choice as an owner decision.
2. **Let the invariants judge every step.** `src/check.rs` holds the invariants. The scenarios and the random
   runs call them after every step, and the random
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
   invariant, add an excuse to it, or narrow the random runs so the suite passes. Calling a check "stricter
   than the rule" or "a check bug" does not change this. If the rule the check enforces is in the ledger
   as `owner` or `approved`, the model is wrong. If it is not, the question goes to the owner (§"Fix passes"). A rejected alternative keeps
   its ignored test as the evidence for rejecting it. When it is a policy setting in `Policies`, the test
   lands with it (findings 1 and 6: `MergeRule::GitMergeBase`). When it replaces
   the model's code, its tests cannot land without that code: push it as a branch that is never merged,
   and name the branch and commit in its finding (finding 13 does this).
6. **Docs follow the model.** In the same pull request, update MODEL.md (rules, invariants, findings, costs)
   and git-model.md (the foundation). Owner decisions made during the work go into MODEL.md's list of the
   owner's words (step 1); the pull request body quotes them too, so a reviewer sees them in one place. CI is the gate: fmt, clippy, the tests and the mdBook build must be green.

## Fix passes

A fix pass answers a review or a failing seed. It is where agents have invented rules: an exception added to
make one seed pass, a "cheap" shortcut, a check taught to excuse a case. Each one was a design decision nobody
asked the owner about, and several later had to be torn out (P3, held states, the dropped agreeing delete).
So a fix pass may only do two things:

- **delete** code, a check exception or a mechanism;
- **correct** the model, a check or a test toward a rule the ledger marks `owner` or `approved`.

It may not add a rule, an exception (in the model or in a check), a refusal, a wait, or a setting. It may not
loosen, narrow or excuse a check, and it may not reword a rule. Any of these goes back as a question for the
owner, in the fix pass's result, with:

1. the failing seed or scenario;
2. the ledger rule or rules involved, or "none";
3. two or three options in plain words, with a recommendation.

The fix pass then stops on that item. It records the item as an open coherence finding with an ignored
test (procedure step 5); it does not patch around it.

A rule is binding only until a blocking consideration is found (the owner, 2026-11-05, quoted at the top of the
ledger). Code that bends to keep a rule (a hack, an exception, a special case) is a code smell: escalate it to the
owner as a reason to reopen the architecture, never build it.

Before fixing, ask **which ledger rule caused this?** A defect that traces to a `proposed` rule, or to no rule,
is a question for the owner, not a fix. When the same mechanism has needed two exceptions, or draws a third
finding, the mechanism's premise is wrong: stop and bring the owner options for removing it.

Every exception in `src/check.rs` is a line marked `// excuse: R<id> <why>`, citing the ledger rule that
requires it. The ledger check fails on an excuse with no rule, or one whose rule is not `owner` or `approved`.

## Adding a rule, a scenario or an invariant

- **A scenario** goes in `tests/scenarios.rs`, named `r<N>_<what it shows>`. Use the helpers there: `take`
  steps and checks every invariant, `next` takes one queued change whole, `drain` runs every pending step and
  then checks convergence.
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
   decides a collision itself; the fork winning instead of the upstream; dropping a flag a later step reads;
   skipping a lock release when a client dies; an append that ignores where the file ends; skipping the ref
   that keeps a losing commit reachable; applying the lines of a losing save that did not collide; letting a
   transaction's clean lines apply when one of its lines lost; a file-rule loss that does not cost its whole change; a judge that skips one of a take's changes (only the
   `gitledger` check sees it at the step); a landing that locks only the files its plan writes; an append onto a partial last line that adds a line instead of finishing it; a landing that lets a line follow the
   upstream's unfinished last line; a
   hole filled with other bytes than NUL; a collision record in the wrong commit, or none;
   placing an incoming insert before the upstream's at one place; letting an edit of a file the upstream unlinked
   apply; an entry going ahead of an earlier waiting one it shares a file with, or one of its own fork (P1); an
   atomic save that
   leaves the replaced file's old object at its path; a crash whose replay saves an open a save already names, or
   drops the journal. From the review of 2026-11-04: a crash replay that re-links each open to its file identity
   (a dead handle saved into the file that replaced it); a waiting take that records only its files open now; a
   crash that drops a transaction's journaled calls. From the owner's decisions of 2026-11-04: P3 reintroduced (a
   new write-open waits behind a waiting take); a sync that delivers the upstream's head whatever is open (past an open
   file); a sync that never delivers a prefix (it waits whole); and
   each fix of findings 11, 25 and 26 reverted. From R5.3: a close of a transaction's file that saves at once; deferred
   calls applied out of order; a rollback that drops them; a crash that drops them.
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

Review provenance before behaviour. For every finding, name the ledger rule that caused it. If that rule is
`proposed`, or there is none, the finding goes to the owner as a question, not to a fix pass.

- Every changed or new rule is in the [rules ledger](../../docs/target-architecture/rules-ledger.md), and
  every outcome-deciding change traces to an `owner` or `approved` entry. Look in particular for a new
  shortcut, exception, refusal, wait or setting with no ledger entry: an unapproved rule.
- No check got weaker: diff `src/check.rs` for new `excuse:` lines, removed assertions, narrowed loops or
  conditions. A weaker check is a finding, whatever reason the commit gives.
- Every rule row names scenarios that exist and assert the outcome.
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

## Phase 1: the model as the conformance suite

[Phase 1](../../docs/target-architecture/phase-1.md) builds the proposed design so real end-to-end tests can
validate this model. The scenario bodies assert on the model's internals, so they are not ported. **The model runs
them unchanged and records each call plus what each tree showed (`src/calls.rs`); the same calls replay against the
real library and real mounts (`crates/agentfs-conformance`); the real run must show the same, and passes invariants
of its own.** Those read `git`, the crate's own parse of `metadata.json` and `namespace.json` and its own byte
oracle, never the library's bookkeeping. Assertions reach the real system only through that equality, so only for
what a trace's `Shown` exposes: each tree's files, and the lost lines of each collision.

```sh
# Record (the same run CI makes), then replay. Without Linux and /dev/fuse, mounts stay NOT RUN.
AGENTFS_MODEL_TRACE_DIR=$T cargo test --locked -p agentfs-model --test scenarios --test interleavings
cargo run --locked -p agentfs-conformance --bin phase1-replay -- model   "$T" --min-scenarios 72 --min-seeds 94
cargo run --locked -p agentfs-conformance --bin phase1-replay -- library "$T" --min-scenarios 72 --min-seeds 94
# One trace, its state kept to read with git:
PHASE1_REPLAY_KEEP=/tmp/k phase1-replay library "$T" --min-scenarios 0 --min-seeds 0 --only NAME
```

What a replay decides, each reported rather than hidden:

- **Scope.** A trace is out if it used transactions, `Client::Outside`, nested forks, `Step::Drop` or another policy
  (`scope_of`). `random_interleavings_phase1_scope` and `random_interleavings_rounds_phase1_scope` make none of the
  first four, every invariant on.
- **A take.** A take is one atomic step, at `take_next`, in the model as in the real system (the owner, 2026-11-08,
  finding 49: "Without any other information, B sounds reasonable. Obviously atomic operations are more reliable"): it
  chooses its entry, locks its keys and writes, with no call in between. So the trace is the order the calls ran in, a
  take is recorded once, after its write, and the trees are compared after every call. One ordering is left in
  `world.rs`'s recording: a queue step that only marks a wait while another entry is free is not a call, since a real
  take goes on to the free entry. No take is in progress at a crash. Until 2026-11-08 the model's take was two steps,
  and a take recorded later than it began could pick another entry at its recorded place (finding 49).
- **Mounts.** `mountable` takes at most one fork, made before any client call, and no open that both appends and
  writes at an offset: Linux has no such descriptor. The take gate (`AGENTFS_TAKE_GATE`, a failpoints build) runs each
  take at the trace's point and answers by a fifo, and a synchronous request after `close(2)` proves its `RELEASE`
  ran, so the replay has no poll and no timer but the wait for a mount to appear.

| Rule | Replay | Where else |
|---|---|---|
| R1.4 mappings | no mmap call | page-cache job |
| R9, R9.7: a client call waiting on a take | none in the model, whose take is one step; the replay order stands for it, and only `Waiting` of takes is compared | mounted P1/P2 cases, finite, no seeds |
| R12 mid-step or with unsynced loss | crash between calls only | recording-faults, power-loss jobs |
| ENOSPC, EIO | no injected failure; `errno` compared only library against mount | enospc job |
| Trace with two forks | library only (§18 asks one fork) | - |
| Automatic takes (R1) | a mount takes only through the gate | mounted P1/P2 cases |
| R1.3 an open's own read, R1.1 another reader | no calls: the replay makes neither read, so nothing compares them | byte and page-cache jobs |

The in-scope traces at CI's seeds are 72 from scenarios and 94 from seeds (64 and 16 of the phase 1 generators, and 14 of
the ordinary ones that happened to stay in scope). The rest are excluded by the reasons above, and the replay prints the
count of each. The floors
in `.github/workflows/phase-1.yml` are what passes: 74 and 94 on the library, and 71 and 16 on mounts, where 81 in-scope traces are
not mountable. Library and model replay every in-scope trace. `[phase1-seeds]` in a pushed commit message, or a manual run, replays
the phase 1 generators at seeds 1 to 500 on the library, in four shards (`ci/replay-seeds.sh`): too slow for every push, and fast
enough for a job.

A mismatch is a finding (MODEL.md), never a change to either side without a ledger rule. Its traces
go in `crates/agentfs-conformance/open-findings.txt` with its number and must keep failing, as an ignored test must, and
its small reproduction is an ignored test in `crates/agentfs-conformance/tests/replay.rs`. CI's seeds list none, and since findings
48 and 49 closed (2026-11-08) neither do seeds 1 to 500. A listed trace that passes fails the job.

The model and the library share the alignment's call (`agentfs_align`): one environment and one program lookup, so a replay runs one
release of `git` on both sides. Identities depend on the release, so `ci/git-release` pins one for the image and for CI, and `ci/check-tree-git.sh` fails on another (R10.10).
