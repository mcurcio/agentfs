# Exit 3 frees the volume: frozen pages

Base 8cfa8be, branch claude/bold-hopper-72ixu2, PR mcurcio/agentfs#48. Two slices, in
order: `exit-3-reap` (the tracer), then `csi-lease-wait`. They are not concurrent: B's
build compiles agentfs-engine, which A edits in the shared checkout. Owner input: guidance,
2026-11-26, asked whether a host that exits 3 should reap its tree git children first:
"Probably" (OWNER-GUIDANCE.md). No new owner answer is needed.

The skeleton commit holds, as compiling code with `todo!()` bodies, the new
`agentfs_os::inherited_lock` (with its docs/architecture.md row), `Recording::end_git_children`
and `Domains::end_git_children`. Its `#[expect(dead_code | unused_variables)]` attributes
fail clippy once A's bodies use what they cover: A deletes them.

## Page exit-3-reap

**Goal.** Every exit `agentfs host` makes itself (an exit status, not a death by signal)
leaves the volume's lease free when the parent's `waitpid` returns. The exit-3 path (and
exit 2 with a view open) ends every git child that holds the lease before the process exits.
Packages: agentfs-os, agentfs-engine, agentfs-service, agentfs-cli, agentfs-repo (tests
only). No new crate. The tracer may span them (implementation-workflow §3.4).

**agentfs-os, `src/inherited_lock.rs`** (skeleton signatures frozen; bodies here):
- `pub struct InheritedLock { lock: FileLock, ended: RwLock<bool>, live: Mutex<Vec<OwnedFd>> }`, `#[derive(Debug)]`.
- `pub fn new(lock: FileLock) -> Self`.
- `pub fn spawn_with(&self, command: &mut Command, spawn: impl FnOnce(&mut Command) -> io::Result<Child>) -> io::Result<Child>`:
  takes `ended.read()` across the start (refused, `spawn` not called, once `true`); adds the
  `pre_exec` today's `tree_git::attach_lease_signal` has (PDEATHSIG, `getppid` check, clear
  `FD_CLOEXEC` on the lock's fd), through rustix: `set_parent_process_death_signal`,
  `getppid`, `fcntl_getfd/setfd`; calls `spawn`; `pidfd_open(child pid)` (race-free: the
  caller has not waited yet); on its failure `child.kill()` + `child.wait()` and the error;
  pushes the pidfd after dropping the ones already exited (`waitid(PidFd, EXITED|NOWAIT|NOHANG)`).
- `pub fn end_children(&self) -> io::Result<()>`: `*ended.write() = true`; for each pidfd
  `pidfd_send_signal(KILL)` (ESRCH is fine), then `waitid(PidFd, EXITED|NOWAIT)`, EINTR
  retried, ECHILD (already reaped) fine. Never reaps: the starter's `Child::wait` still works.
- `impl Drop for InheritedLock`: `end_children()` (error ignored), then the field's
  `FileLock` drop unlocks. So no unlock can ever happen under a live child.
- `#[cfg(test)]` only: a death signal other than SIGKILL (SIGSTOP), so a test can hold a
  child alive past its holder. Module carries `#![allow(clippy::disallowed_methods, reason = "...")]`.
- `Cargo.toml`: rustix features `["fs", "process"]`.
- Why it is here: a mechanical wrapper over platform calls with no git knowledge (altitude
  tree 3); `file_lock` is its neighbour. Kernel: pidfd and `waitid(P_PIDFD)` need Linux 5.4,
  below the 5.6 agentfs needs already (docs/operator-guide.md).

**agentfs-engine.** The lease type `Arc<FileLock>` becomes `Arc<InheritedLock>` at every
site (grep `Arc<FileLock>`: tree_git.rs 5, history/mod.rs 3, tree_actor/mod.rs 4):
`TreeGit { lease: Option<Arc<InheritedLock>> }`, `init_with_lease(path, Arc<InheritedLock>)`,
`open_with_lease(path, Arc<InheritedLock>)` (visibility unchanged), `init_store`/`open_store`,
`lock_area(root) -> Result<Arc<InheritedLock>>` (`agentfs verify` gets it too), `TreeActor.lease`.
- `tree_git.rs`: `command()` no longer touches the lease; one private
  `fn start(&self, cmd: &mut Command) -> io::Result<Child>` (`lease.spawn_with(cmd, agentfs_git::git_cli::counted)`,
  or `counted(cmd)` without a lease) is every spawn: the 5 `counted` sites (367, 492, 651,
  `output` 972, `run_command` 1030; the two free fns take the started child or `&TreeGit`).
  Delete `attach_lease`, `attach_lease_signal`; their tests move (below).
- `history/repack.rs`: its git becomes a lease holder: the one `cmd.spawn()` (755) is
  `lease.spawn_with(&mut cmd, Command::spawn)` under the slot's lock as now; its own
  `pre_exec`/`libc::prctl` (746) goes; `process_group(0)` and `kill_group` stay.
  `Repacker::start(history, git_dir, lease: Arc<InheritedLock>)` (call: tree_actor/mod.rs 1157).
  Module doc: "never under the builder's lease" is reworded: it never waits for the builder,
  and its git holds the volume's lease like every git child.
- `call_path.rs`: `Recording { lease: Arc<InheritedLock> }` from the actor at `serve`;
  `pub fn end_git_children(&self) -> std::io::Result<()>` (skeleton doc frozen) is
  `self.lease.end_children()`. No actor lock taken: it works while a call holds it.

**agentfs-service.** `Domains::end_git_children(&self) -> std::io::Result<()>` (frozen):
`self.sessions.recording().end_git_children()`.

**agentfs-cli.** `Serving::abandon`: before `std::mem::forget(self)`,
`self.domains.end_git_children()`; on `Err(e)` log
`tree GitChildrenNotEnded: {e}: the volume's lease is free only once each has exited` and go
on (exit 3 still). host.rs: `EXIT_LEFT_OPEN` and `host()` docs say the lease is free at exit.

**Checks.** (1) `clippy.toml` disallows `rustix::io::fcntl_setfd`,
`rustix::process::set_parent_process_death_signal` and `libc::prctl` (`allow-invalid`),
reason "a child shares a file lock only through agentfs_os::inherited_lock::InheritedLock::spawn_with".
(2) agentfs-repo `the_tree_engine_starts_git_only_through_counted` also asserts: tree_git.rs's
production text names `git_cli::counted` only inside `fn start`'s body, and repack.rs's has
no `.spawn()` and names `spawn_with`.

**Tests.** Red first: write the os test (a) and the FUSE loop (e), see them red on base code.
- agentfs-os `inherited_lock::tests`, helper-process style as tree_git's today (the test binary
  re-run as the holder, children `cat` on a pipe the driver keeps): (a) a holder that ends its
  children and exits (`process::exit`, no drop) leaves the lock free at its `waitpid`, with the
  SIGSTOP death signal so a child would outlive it (red without `end_children`: deterministic);
  (b) moved `killed_parent_leaves_lease_until_git_exits`: a killed holder's child keeps the lock
  until it exits; (c) no child starts after `end_children` and `spawn` is not called;
  (d) the starter's `wait` still reaps a child `end_children` killed (signal 9, not ECHILD);
  (e2) dropping the lock ends its children before it unlocks; (f) exited children are dropped
  from `live` (a `#[cfg(test)]` count).
- agentfs-engine `tests/it/tree_actor.rs`: a recorded write, `end_git_children`, then a call that
  needs git fails and `git_cli::children()` does not move; the Recording dropped and reopened
  builds history holding the write (principle 1 at engine level). repack.rs's tests build an
  `InheritedLock` over a temp lock file.
- agentfs-cli `tests/launcher_host.rs`: `Host::wait` after `child.wait()`: exit 75 skips; a death
  by signal waits for the lease as now; any other exit asserts `FileLock::try_take(lease,
  Exclusive)` is `Some` at once ("the host exited {status} with its lease held"). `Volume::lease`
  and `Host::wait` docs move to the new rule. (e) `a_stopped_hosts_lease_is_free_when_its_end_returns`
  becomes, on ONE volume, 10 rounds: host, root view at `r{round}/`, write files `f{round}-{i}` until a /proc scan
  (ppid = host, comm `git`) finds a git child, SIGSTOP it, EOF, exit 3 (the harness asserts the
  lease free), detach the stale layer; then a last host drains (exit 0) and `v.messages("f{round}-0")`
  is non-empty for every round: history rebuilt after each kill mid-batch.

**Docs.** filesystem.md ("The control channel", "Exit status": 3 = views left stale, not
released, every git child ended so the lease is free at exit; 75 = another process holds it,
including a killed host's git children until each has exited); trees.md (line ~203);
engine.md (lease paragraph, `Recording::end_git_children`); core.md "Locks and sockets"
(`InheritedLock`); service.md (calls); architecture.md (tree_git.rs, repack.rs, domains.rs,
serving.rs rows); runbooks/recording-failures.md (row 75, log line `tree GitChildrenNotEnded`).

**Acceptance.** Gates: fmt; clippy and nextest for agentfs-os, agentfs-engine (also
`--features failpoints`), agentfs-service, agentfs-cli, agentfs-repo; the FUSE tier
(`ci/fuse-tests.sh`, root and /dev/fuse are present in this container: try it, else CI).
**Probes.** clippy: an `rustix::io::fcntl_setfd` call in tree_git.rs fails clippy. Repo: a
`git_cli::counted(&mut cmd)` call in `fast_import` fails the repo test; `cmd.spawn()` back in
repack.rs fails it. Behaviour: `end_children` returning `Ok(())` at once fails (a) and the FUSE
loop; `abandon` without `end_git_children` fails the FUSE loop.
**Files owned:** crates/agentfs-os/{Cargo.toml,src/inherited_lock.rs,src/lib.rs},
crates/agentfs-engine/src/{tree_git.rs,tree_actor/mod.rs,tree_actor/call_path.rs,tree_actor/history/{mod.rs,repack.rs,verify.rs}},
crates/agentfs-engine/tests/it/tree_actor.rs, crates/agentfs-service/src/domains.rs,
crates/agentfs-cli/src/launcher/{serving.rs,host.rs}, crates/agentfs-cli/tests/launcher_host.rs,
crates/agentfs-repo/tests/repo/crates.rs, clippy.toml, Cargo.lock, the docs above.

## Page csi-lease-wait

**Goal.** The node plugin's supervisor waits on the lease event, not a timer, where a start
exited 75. Packages: agentfs-csi, agentfs-engine (one item). Depends on exit-3-reap landed.

**agentfs-engine.** `tree_actor::wait_for_lease(root: &Path) -> io::Result<()>` loses its
`#[cfg(any(test, feature = "test-util"))]`: a production call now. Doc: what a launcher waits
on before it starts a server on a volume another process held (exit 75), and what a harness
waits on after a kill. engine.md "Test builds" drops it from the test-util list.

**agentfs-csi, `src/supervisor.rs`.**
```rust
/// What a host that did not start waits for before it is tried again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Never: trying again cannot help (another protocol version).
    Never,
    /// The volume's lease: another process held it (exit 75).
    Lease,
    /// A backoff: a failure no event ends (a full disk, PID pressure).
    Backoff,
}
struct NotStarted { error: Error, retry: Retry }   // `permanent: bool` goes; From<Error> is Backoff

/// What a restart waits for before its next try.
enum Wait { Delay(Duration), Lease }

async fn restart(self: Arc<Self>, volume: String, wait: Wait);
/// Returns once no process holds the volume's lease (`wait_for_lease` on a
/// `std::thread` of its own, answered over a tokio oneshot: never
/// `spawn_blocking`, whose pool the runtime's shutdown waits for).
async fn lease_free(state: PathBuf) -> io::Result<()>;
```
- `start_host`: `Exit::Code(75)` before the hello is `Retry::Lease`; the version refusal `Never`.
- `restart` loop: `Wait::Delay(d)` sleeps; `Wait::Lease` awaits `lease_free`, NOT under the
  start lock (an unpublish must not wait on an operator's process), then takes it and checks
  `shutting_down`/`hosts`/`idle` as now. After a failed try: `Lease` keeps the last delay,
  `Backoff` doubles it (`next_backoff`), `Never` returns. A failed `lease_free` (the state
  directory gone) is logged and becomes `Wait::Delay(next_backoff(..))`.
- `exited` (a crash) passes `Wait::Delay(next_backoff(..))`; reconcile's failed start passes
  `Wait::Lease` or `Wait::Delay(cfg.backoff.initial)` by its `Retry`.
- Kept timers, each with its own reason: the crash-restart backoff (no event says a host that
  crashed will start now: it throttles a crash loop); the retry backoff of a start that failed
  otherwise; the launcher's bounded wait for a child's last log lines. Publish, ensure and
  release still answer `UNAVAILABLE` on 75 (the CSI idiom: the kubelet retries).
- Module doc (lines 22-30) and the Cargo.toml comment on agentfs-engine (it now takes the lease
  briefly) say so.

**Tests** (`crates/agentfs-csi/tests/supervisor.rs`, paused time). World knob `busy_starts: usize`:
that many next hosts exit 75 before their hello. New `a_busy_restart_waits_for_the_lease`: a
served host crashes while the test holds `<state>/actor.lock` (FileLock, the state dir made);
the restart's host exits 75; `tokio::time::advance(10 * Backoff::max)` spawns nothing more;
dropping the lock spawns the next host, which replays the view. Also a reconcile start that
exits 75 waits the same way. Existing restart tests stay green.
**Probe.** `Retry::Lease` mapped to `Wait::Delay` fails the new test (a spawn under the lease).

**Docs.** csi.md §8 ("A host crashes": a start the lease blocks waits for it; "The node plugin
restarts": each host ends its git children before exit 3, so the new hosts start at once);
library/kubernetes.md (supervisor bullets at ~160-170); engine.md (test-util list).
**Acceptance.** fmt; clippy and nextest for agentfs-csi and agentfs-engine; agentfs-repo.
**Files owned:** crates/agentfs-csi/src/supervisor.rs, crates/agentfs-csi/Cargo.toml (comment),
crates/agentfs-csi/tests/supervisor.rs, crates/agentfs-engine/src/tree_actor/mod.rs (the
`wait_for_lease` item only), docs/csi.md, docs/library/kubernetes.md, docs/library/engine.md.

## Decisions

- **Where the reaping lives.** The engine owns its children, so the call is the engine's:
  `Recording::end_git_children`, reached by the launcher through
  `Domains::end_git_children`, beside `unstage`, the service's other stop. The mechanism is
  one primitive in agentfs-os, `InheritedLock`: the lease and the registry of children holding
  it in one type. Prior art: pidfd process supervision (systemd, tokio's pidfd reaper; this
  repo's launcher/command.rs signals through a pidfd); `waitid(WNOWAIT)` waits without
  reaping, so the threads that started each git still reap it and no pid is ever reused
  under a signal.
- **The class made impossible, not the instance.** The lock's fd reaches a child only
  through `spawn_with`, which registers it (clippy bans the two calls that could do it
  elsewhere); dropping the lock ends its children before it unlocks, so the old comment's
  rule ("the Arc keeps the FileLock alive through synchronous waits") becomes a
  construction-time guarantee. The harness's rule becomes a check on the product: every exit
  status asserts the lease free.
- **Interaction with the drain and abandon.** The drain (exit 0) is unchanged: it joins the
  workers, which wait their children. `abandon` ends the children, then forgets as now; it
  never unlocks, so the process's exit is the release (no second host can take the volume
  while this one's threads still run).
- **The repack's git becomes a lease holder (beyond the brief's literal scope).** Today it
  does not hold the lease, so a next host started at the lease event can run beside a dying
  repack and delete its `multi-pack-index.lock` (history/mod.rs `remove_stale_locks` assumes
  "no other writer can own these"). Slice B removes the backoff that used to hide that, so
  "lease free" must mean "no git child of the last host alive". Cost: after a host is
  SIGKILLed, the lease stays until the repack's git has died too (its parent-death signal).
- **Equivalence with a crash (principle 1).** `end_children` does to each child what
  PDEATHSIG does at a crash (SIGKILL); what a killed fast-import, `git log` or repack leaves
  is what a crash leaves, which the next open already cleans (`clean_packs`,
  `remove_stale_locks`, `repair`) and rebuilds from the journal. A builder that sees its
  packer die records a failed batch in memory only (built.rs `failed_event`); nothing is
  written past the journal. Proof: the engine test (reopen builds the write) and the FUSE
  loop's last host (history holds every round's file).
- **Costs, stated.** Between the kill and the exit the host's threads still run: a view's
  read in flight that needed a git child answers `EIO` rather than the `ENOTCONN` a crash
  gives, and the log may show one `tree HistoryBuildFailed` line. A git child's own children
  are not waited for (none of today's commands start any). The host's exit 3 now waits for
  each git child to die, which a git in an uninterruptible fsync delays.
- **Spawns stay concurrent.** A read lock across each start, a write lock for the end: no
  spawn waits for another, and the end never misses one mid-fork.
- **The red test.** The deterministic red is the agentfs-os helper-process test (a SIGSTOP
  death signal holds a child alive past its holder). The FUSE loop guarantees a git child is
  alive at EOF (found in /proc and SIGSTOPped); without the fix it is red with high
  probability, not certainty: no product failpoint is added for it (the FUSE tier builds
  without failpoints).
- **The brief's page template** (new crates, move maps, cycle cuts, Dockerfile stubs,
  check-deps) does not apply: no crate is added or moved, no dependency edge is added
  (agentfs-csi already depends on agentfs-engine and agentfs-os).
- **Design review (self-check, implementation-workflow §2.3).** Checked against the goals:
  scope (1) A, (2) B, (3) A, (4) A and B, (5) A. Checked on paper: green-alone (A has every
  consumer of the lease type; B needs only the un-gated `wait_for_lease`); the kernel order
  (a child's files are closed before its pidfd reports the exit, and the host's before its
  parent's `waitpid` returns); the cross-thread `waitid(WNOWAIT)` on children another thread
  waits on. No finding blocks the cut.
