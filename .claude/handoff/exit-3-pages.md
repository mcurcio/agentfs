# Exit 3 frees the volume: frozen pages (revision 2)

Base 8cfa8be, branch claude/bold-hopper-72ixu2, PR mcurcio/agentfs#48. Three slices, run one
after another in one checkout: `inherited-lock` (the primitive, agentfs-os only), `exit-3-reap`
(the tracer: engine, service, host, harness), `csi-lease-wait`. The owner's input is guidance,
not a requirement (2026-11-26). Asked whether a host that exits 3 should reap its tree git
children first, he said "Probably" (OWNER-GUIDANCE.md). Nothing here needs a new answer.

The skeleton commit holds these as compiling code with `todo!()` bodies: `agentfs_os::inherited_lock`
(`InheritedLock::{new, spawn_with, end_children, ended}`, `exited`), `Recording::end_git_children`,
`Domains::abandon(self)`, `TreeStateArea::wait_for_lease`, the variant `tree_actor::Error::LeaseHeld`
(its errno arm included), and the module-map row. Its `#[expect(dead_code | unused_variables)]`
attributes fail clippy once a slice's bodies use what they cover, so the slice deletes them.

## Page inherited-lock

**Goal.** A lock that only registered children share, and that no grandchild can reach, in
agentfs-os, the lowest layer: a wrapper over system calls with no git knowledge (altitude tree 3).
No consumer yet. Green alone.
**Frozen** (skeleton doc comments are the contract): `InheritedLock { lock: FileLock, ended:
RwLock<bool>, live: Mutex<Vec<OwnedFd>> }`, `#[derive(Debug)]`, with no accessor to the lock or its file.
`new(FileLock) -> Self`; `spawn_with(&self, &mut Command, impl FnOnce(&mut Command) ->
io::Result<Child>) -> io::Result<Child>`; `end_children(&self) -> io::Result<()>`; `ended(&self) -> bool`;
`impl Drop` (calls `end_children`, ignores its error, then the `FileLock` field unlocks);
`pub fn exited(pid: u32) -> io::Result<()>`.
**Bodies.**
- `spawn_with`: `ended.read()` held across the whole start; when `true`, return
  `io::Error::from_raw_os_error(libc::ENOTCONN)` without calling `spawn`. Add one `pre_exec`. It
  must not allocate, so the filter is a `static [libc::sock_filter; N]`. In order: (1)
  `prctl(PR_SET_PDEATHSIG, SIGKILL)`; (2) `getppid()` differs from the parent's pid: `ECHILD`;
  (3) `prctl(PR_SET_NO_NEW_PRIVS, 1)`; (4) `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &FPROG)`;
  (5) clear `FD_CLOEXEC` on the lock's fd (`rustix::io::fcntl_getfd`/`fcntl_setfd`). Then call
  `spawn` and `rustix::process::pidfd_open(child pid)`, which is race-free because nothing has
  waited yet. If the pidfd fails, `child.kill()`, `child.wait()` and return the error. Otherwise
  drop the pidfds already exited (`waitid(PidFd, EXITED|NOWAIT|NOHANG)`), then push the new one.
- The filter, after `samples/seccomp/bpf-direct.c` in the kernel tree. Load `arch` (offset 4);
  another arch gets `ERRNO(ENOSYS)`. Load `nr` (offset 0). On x86_64, `nr >= 0x4000_0000` (x32)
  gets `ENOSYS`. `clone3` gets `ENOSYS`: glibc then falls back to `clone`, Docker's default-profile
  idiom. `clone` loads `args[0]` (offset 16, the low word): with `CLONE_THREAD` set it is `ALLOW`,
  else `ERRNO(EPERM)`; Firecracker's thread-only clone rule. `fork` and `vfork` (x86_64 only) get
  `EPERM`. Everything else is `ALLOW`. Local consts: `AUDIT_ARCH_X86_64 = 0xC000_003E`,
  `AUDIT_ARCH_AARCH64 = 0xC000_00B7`. Any other `target_arch`: `compile_error!`.
- `end_children`: set `*ended.write() = true` and keep the guard. For each pidfd send
  `pidfd_send_signal(KILL)` (ESRCH is fine), then wait with the shared private
  `fn wait_exit(BorrowedFd)`: `waitid(PidFd, EXITED|NOWAIT)`, EINTR retried, ECHILD fine.
  `exited(pid)` is `pidfd_open` + `wait_exit`. That loop has this one home.
- `#[cfg(test)]` only: a death signal of SIGSTOP instead of SIGKILL, so that a child outlives its holder.
- `Cargo.toml`: rustix features `["fs", "process"]`.
**Tests** (`inherited_lock::tests`). Helper-process style: the test binary re-runs itself as the
holder; children `cat` a pipe the driver keeps open.
(a) A holder that ends its children and calls `process::exit` (no drop) leaves the lock free
at its `waitpid`. (b) A killed holder's child keeps the lock until it exits. (c) After the
end, a spawn is `ENOTCONN` and `spawn` is not called. (d) The starter's `wait` still reaps a
killed child (signal 9, not ECHILD). (e) A drop ends the children before it unlocks.
(f) Children that have exited leave `live` (a `#[cfg(test)]` count). (g) A holder makes a thread
but cannot fork: the re-run binary spawns a `std::thread` (works), then `Command::spawn` is EPERM.
(h) `exited` returns after the exit, and `wait` still reaps.
**Recorded red:** first write (a) and (g) over the skeleton with the probe bodies, see them fail,
then fill them in. **Probes:** `end_children` returning `Ok(())` at once fails (a). Leaving
step (4) out fails (g).
**Docs.** core.md "Locks and sockets": the `InheritedLock` paragraph (what it guarantees, the
no-fork filter, the fork-to-exec share of a start made elsewhere).
**Gates.** fmt; clippy and nextest for `-p agentfs-os`; `-p agentfs-repo`.
**Files owned:** crates/agentfs-os/{Cargo.toml,src/inherited_lock.rs}, Cargo.lock, docs/library/core.md.

## Page exit-3-reap

**Goal.** Every exit status the host makes itself (0, 1, 2, 3) leaves the lease free when the
parent's `waitpid` returns. At exit 3 (or 2 with a view open) the host ends the engine's
processes, and every one holds the lease. Packages: agentfs-engine, agentfs-service,
agentfs-cli, agentfs-repo (tests). Depends on inherited-lock.
**Engine.** Every lease `Arc<FileLock>` becomes `Arc<InheritedLock>`: tree_git.rs 5,
history/mod.rs 2 (`init_store`, `open_store`), tree_actor/mod.rs 4 (`_lock`, `lock_area`, and
the two `lock` parameters near 853 and 1075). `agentfs verify`'s `lock_area` is one of them.
- `TreeGit { lease: Arc<InheritedLock> }` is no longer an `Option`. The test-only `init`/`open`
  hold a shared `FileLock` on the sibling `<path>.lease`.
  `pub(crate) fn start(&self, cmd: &mut Command) -> io::Result<Child>` is
  `self.lease.spawn_with(cmd, agentfs_git::git_cli::counted)` and makes every TreeGit start
  (367, 492, 651, `output` 972, `run_command` 1030). The two free fns take the started `Child`.
  `command()` attaches nothing. Delete `attach_lease`, `attach_lease_signal` and their tests
  (`tests::lease_child_process`, `killed_parent_leaves_lease_until_git_exits` and the helpers
  between them), which inherited-lock replaced.
- A failure while `lease.ended()` (a refused start, or a git the end killed) is
  `Error::Io(ENOTCONN)` raw, so the workload's errno is the one a dead server gives.
- builder.rs: `align` and `saved_lines` take `repo: &TreeGit` (call sites 851, 950, 1050, 1063).
  The alignment's `git diff` is `histogram_files_with(&a, &b, |cmd| repo.start(cmd))`:
  it now holds the lease, and the end gates it.
- repack.rs: `command()` takes `lease: &InheritedLock`. Its one start is
  `lease.spawn_with(&mut cmd, Command::spawn)` under the slot's lock. Delete its `pre_exec` and
  `libc::prctl`, `process_group(0)`, `kill_group` and `fn exited`. One run is one process
  (holders cannot fork), so a stop is `child.kill()` under the slot (unreaped, so pid-safe), and
  the wait is `agentfs_os::inherited_lock::exited(pid)`. Signature:
  `Repacker::start(history, git_dir, lease: Arc<InheritedLock>)` (its call in tree_actor/mod.rs, near 1163).
  Module doc: its git holds the lease like every git child, and "a stop kills ... its own
  process group" is reworded.
- call_path.rs: `Recording { lease: Arc<InheritedLock> }` is taken from the actor at `serve`.
  `end_git_children(&self)` is `self.lease.end_children()` and takes no actor lock.
  The frozen doc fixes the behaviour after it: ENOTCONN, and a drop that ends one batch as failed
  (built.rs `next_work`: failed + stopping breaks) and then unlocks.
**Service.** `Domains::abandon(self) -> io::Result<()>` (frozen):
`let r = self.sessions.recording().end_git_children(); std::mem::forget(self); r`.
**CLI.** `Serving::abandon(self) -> usize`: `let Serving { domains, host, views, triggers, .. } = self;`
then `domains.abandon()`. On `Err(e)` log `tree AbandonIncomplete: {e}: the volume's lease is
free once each of its git children has exited`, then `std::mem::forget((host, triggers))` and
return `views.len()`. host.rs: the docs of `EXIT_LEFT_OPEN` and `host()` say so.
**Checks.** (1) `clippy.toml` disallows the calls that share a descriptor: `rustix::io::fcntl_setfd`,
`rustix::io::dup2`, `rustix::io::dup3`, `libc::dup2`, `libc::dup3` (`allow-invalid`), with the
reason "a child shares a file lock only through agentfs_os::inherited_lock::InheritedLock::spawn_with".
The parent-death signal stays free for other spawners. (2) agentfs-repo
`the_tree_engine_starts_git_only_through_counted` becomes `the_tree_engine_starts_processes_only_as_lease_holders`.
It scans every engine file, repack.rs included, plus lines and referee. First it removes each
`spawn_with(<arg>, agentfs_git::git_cli::counted)` and `spawn_with(<arg>, Command::spawn)`. In what
is left, `starts_in` finds nothing and no `git_cli::counted` appears. Before removal, the counted
form appears exactly once (TreeGit::start) and the `Command::spawn` form once (repack).
`a_process_started_outside_counted_is_found` gains these cases.
**Tests.** Engine, `tests/it/tree_actor.rs`: write and record, `end_git_children`, then a read
that needs git answers ENOTCONN and `git_cli::children()` stays put. Dropping and reopening
builds history that holds the write (principle 1). The Recording's drop after the end returns.
repack's tests build an `InheritedLock` over a temp file.
CLI, `tests/launcher_host.rs`: `Host::wait`, after `child.wait()`, asserts for 0, 1, 2 and 3 that
`FileLock::try_take(lease, Exclusive)` is `Some` at once ("the host exited {status} with its
lease held"). 75 holds nothing. A signal or 101 waits for the lease as now. Move the docs of
`Volume::lease` and `Host::wait` to this rule.
**Red test (e):** `a_stopped_hosts_lease_is_free_when_its_end_returns`, on ONE volume, 10 rounds.
Each round: start a host and a root view at `r{round}/`, then write `f{round}-{i}` until a holder
is found. A holder is a child of the host from `/proc/<host>/task/*/children`. Take a pidfd
first, then confirm that an fd in `/proc/<pid>/fd` links to `<state>/actor.lock`. SIGSTOP it
through the pidfd, send EOF, and expect exit 3 (the harness asserts the lease free). Detach the
stale layer. Last, a host drains with exit 0, and `v.messages("f{round}-0")` is non-empty for
every round.
**Docs.** filesystem.md "Exit status" and "The control channel" (3: views left stale, the volume
not released, its processes ended, so the lease is free at exit; 75: another process holds it,
a killed host's git children among them until each exits). configuration.md `host` bullet
(897-912): its statuses become a link to that section. trees.md ~203. engine.md: the lease
paragraph, `end_git_children`, and holders cannot fork. service.md: `abandon`. operator-guide.md
requirements: seccomp filter mode (`CONFIG_SECCOMP_FILTER`). architecture.md rows for tree_git.rs,
repack.rs, domains.rs and serving.rs. runbooks/recording-failures.md row 75.
**Gates.** fmt. clippy and nextest for engine (also with failpoints), service, cli and repo.
The FUSE tier: root and /dev/fuse are present here, so try `ci/fuse-tests.sh`, else CI.
**Probes.** An `fcntl_setfd` in tree_git.rs fails clippy. `histogram_files_with(&a, &b,
agentfs_git::git_cli::counted)` restored fails the repo test, and so does `cmd.spawn()` in
repack.rs. `abandon` without `end_git_children` fails (e). `start` calling `counted` directly
fails the engine's ENOTCONN test.
**Files owned:** crates/agentfs-engine/src/{tree_git.rs,tree_actor/mod.rs,tree_actor/call_path.rs,
tree_actor/history/{mod.rs,repack.rs,builder.rs,verify.rs}}, crates/agentfs-engine/tests/it/tree_actor.rs,
crates/agentfs-service/src/domains.rs, crates/agentfs-cli/src/launcher/{serving.rs,host.rs},
crates/agentfs-cli/tests/launcher_host.rs, crates/agentfs-repo/tests/repo/crates.rs, clippy.toml,
and the docs above.

## Page csi-lease-wait

**Goal.** After a start exits 75, the supervisor waits on the lease, not a timer. That 75 means
the lease, by construction. Packages: agentfs-engine, agentfs-csi, agentfs-conformance (one call).
**Engine.** `Error::LeaseHeld` (frozen) is raised only by `lock_area`. Move the doc
stranded above `read_rules` ("The state area's exclusive lock") onto `lock_area`. `state_area::volume_error` maps only `LeaseHeld` to
`VolumeError::Busy`; `Busy` joins the `other` arm. Sites that become `LeaseHeld`: the docs of
`TreeActor::create` and `verify_flagged`, cli launcher/verify.rs:58, tests it/tree_actor.rs:491 and
it/tree_verify.rs:366, engine.md:48. `TreeStateArea::wait_for_lease(&self)` (frozen) takes the
body of `tree_actor::wait_for_lease`, which is deleted. Its users move to
`TreeStateArea::at(dir).wait_for_lease()`: conformance mounted.rs:600, engine
tests/support/trees.rs:189, and the unit test that calls it in tree_actor/mod.rs. engine.md:507 drops it from test-util.
**agentfs-csi, `src/supervisor.rs`.**
```rust
/// What a host that did not start waits for before its next start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// Nothing: trying again cannot help (another protocol version).
    Never,
    /// The volume's lease free: the start exited 75, so another process held it.
    Lease,
    /// A backoff delay: a failure no event ends (a crash, a full disk, PID pressure).
    Backoff,
}
struct NotStarted { error: Error, next: Next }   // `permanent` goes; From<Error> is Backoff
async fn restart(self: Arc<Self>, volume: String, next: Next, delay: Duration);
/// `TreeStateArea::wait_for_lease` on a std::thread of its own, answered over a tokio
/// oneshot: never `spawn_blocking`, whose pool the runtime's shutdown waits for.
async fn lease_free(state: PathBuf) -> io::Result<()>;
```
- `start_host`: an `Exit::Code(75)` before the hello gives `Lease`; the version refusal gives `Never`.
- The `restart` loop: `Never` returns. `Backoff` sleeps `delay`. `Lease` awaits `lease_free`
  outside the start lock (an unpublish never waits on another process), and if it fails it
  logs and sleeps `delay`. Then it takes the lock and checks `shutting_down`, `hosts` and `idle`
  as now. After a failed start it takes the start's `next`; `Backoff` doubles `delay`
  (`next_backoff`). Log `... trying again once its state area's lease is free` or
  `... trying again in {delay:?}`.
- `exited` (a crash) calls `restart(v, Backoff, next_backoff(..))`. Reconcile's failed start
  calls `restart(v, ns.next, cfg.backoff.initial)`.
- Timers kept, each for its own reason: the crash-restart backoff (no event says a crashed
  host would start now; it throttles a crash loop), the backoff of a start that failed
  otherwise, and the launcher's bounded wait for a child's last log lines. Publish, ensure and
  release still answer `UNAVAILABLE` on 75: the CSI idiom, in which the kubelet retries.
- Module doc (22-30) and the comment at 1211: exit 3 now comes after the host ends its git children.
**Tests** (`tests/supervisor.rs`, paused time). World knob `busy_starts: usize`: that many next
hosts exit 75 before their hello. New `a_busy_restart_waits_for_the_lease`: a served host
crashes while the test holds `<state>/actor.lock`, and the restart's host exits 75. After
`tokio::time::advance(10 * Backoff::max)`, nothing more has been spawned; dropping the lock
spawns the next host, which replays the view. A reconcile start that exits 75 waits the same way.
**Probe.** `Next::Lease` treated as `Backoff` fails the new test.
**Docs.** csi.md §8: the crash and plugin-restart bullets (~335-350). runbooks/csi-recovery.md:
the crashed host (41-53: the lease wait's log line) and the upgrade (61-64: "exits at once with
status 3"). library/kubernetes.md (160-170). engine.md (48, 507).
**Gates.** fmt; clippy and nextest for engine, csi, conformance and repo.
**Files owned:** crates/agentfs-csi/{src/supervisor.rs,tests/supervisor.rs}, crates/agentfs-engine/src/
{state_area.rs,tree_actor/mod.rs,tree_actor/history/verify.rs}, crates/agentfs-engine/tests/{it/tree_actor.rs,
it/tree_verify.rs,support/trees.rs}, crates/agentfs-cli/src/launcher/verify.rs,
crates/agentfs-conformance/src/mounted.rs, docs/csi.md, docs/runbooks/csi-recovery.md,
docs/library/{kubernetes.md,engine.md}.

## Decisions

- **Where the reaping lives.** The engine owns its children: `Recording::end_git_children`
  ("git children" is engine vocabulary and stays there). The service's word is the existing
  stop that leaves views stale: `Domains::abandon(self)` consumes and forgets the domains, so
  neither use nor drop after it can be written. The launcher calls only that.
- **The guarantee and its two halves.** (1) Every process the engine starts is a lease holder
  through one gate: the repo check, plus the type's privacy (no accessor to the lock's file).
  (2) A holder cannot start a process: the kernel's seccomp filter, not a sentence. Together
  they give "none of the engine's processes holds the lease after the end". What is left, and
  stated: a process started outside the engine shares every open file of the process from its
  fork to its exec. In `agentfs host` that means `fusermount3`. The view host's mounts and
  unmounts run on the serving thread (`&mut ViewHost`), which is in `abandon`. A tree-view thread
  starts one only when its view ends on its own during the stop, in a host without the privilege
  to unmount. That start then sees 75, and slice 3 waits for the lease.
- **Not chosen.** An unlock at abandon (as FileLock does) would free even a stray fork's share.
  But it lets a next host take the volume while this one's threads still append to its journal,
  trading recording for availability (principle 1). A gate inside `git_cli::counted` would be
  process-wide: one process can serve several volumes, and the repack is uncounted. Process
  groups give no event for a group's last exit without a subreaper. Cgroups (systemd's
  `cgroup.kill`, `populated 0`) need a delegated writable cgroup, which a pod rarely has. A
  no-fork filter is smaller than any of these.
- **Recording stays complete (principle 1).** The end does to each holder what PDEATHSIG does at
  a crash. The next open's `clean_packs`, `remove_stale_locks` and `repair` clean what it leaves,
  and history is rebuilt from the journal. It is proved by the engine reopen test and by (e)'s
  last host.
- **Costs.** Holders cannot fork. Today none of their commands does: log, cat-file, rev-parse,
  check-ref-format, fast-import, diff --no-index, multi-pack-index, pack-objects. A future one
  that did fails loudly, with git's "cannot fork". Hosts need seccomp filter mode, which
  distribution kernels have and Kubernetes' RuntimeDefault profile allows. After a SIGKILL, the
  lease also waits for the alignment's and the repack's git to die. A read whose git the end
  killed answers ENOTCONN. Exit 3 waits for each holder to die, longer when one is in an
  uninterruptible fsync. In CSI, a wait for the lease that never ends keeps one thread and one task.
- **CSI (design-it-twice).** The chosen design is that the supervisor waits behind `TreeStateArea`,
  the engine adapter it already uses (supervisor.rs:78, 1273), so the lease file's name stays the
  engine's. Alternative: the host waits itself (`--wait-lock`). Tree commands refuse a nonzero
  one by design ("a busy tree volume exits 75 at once", configuration.md). A waiting host would
  also hold the start lock while the supervisor awaits its hello, so an unpublish would wait on
  another process. Cost of the choice: the supervisor takes `actor.lock` for an instant, and a
  start by another process at that instant sees 75.
- **Deviations from the brief.** (1) The brief covers every exit but SIGKILL; this covers the
  statuses the host makes itself (0, 1, 2, 3). A death by any signal (SIGSEGV, SIGABRT, an
  unhandled SIGHUP or SIGQUIT) or a panic's 101 runs no end: a signal handler cannot take the
  registry's lock safely, and a lock-free registry is machinery for exits that are bugs or an
  operator's kill. Their children die by PDEATHSIG, and the harness and the supervisor wait for
  the lease then. (2) The page template (new crates, move maps, cycle cuts, Dockerfile stubs,
  check-deps) does not apply: no crate or dependency edge is added. (3) The repack's git and the
  alignment's `git diff` become lease holders, beyond "every tree git child that holds the
  lease". A next host started at the lease event must not run beside them: `remove_stale_locks`
  assumes no other writer.
- **Review outcome (revision 2).** Findings and their answers are in the structured result. The
  slices are re-cut into three. They were checked on paper against the goals: scope (1)
  inherited-lock and exit-3-reap, (2) csi-lease-wait, (3) and (5) exit-3-reap, (4) all three.
  Each slice is green alone: inherited-lock has no consumer, and exit-3-reap holds every
  consumer of the lease type.

## Errata (fix round, 2026-11-27)

The full entries are in `.claude/errata.md` ("exit 3 frees the volume, review fix round").

1. Spec defect in "Red test (e)": SIGSTOP does not keep a holder alive past the host's exit, since the
   parent-death signal is SIGKILL. The test now checks, at the host's `stopped with` log line, that the
   holder has exited already.
2. "`#[cfg(test)]` only: a death signal of SIGSTOP": replaced by a run-time choice in the tests'
   holder process, so the production SIGKILL is tested.
3. The frozen doc of `end_git_children` expected `tree HistoryBuildFailed`: the end is now a stop for the
   builder and the repack (`TreeGit::ended`), with no alarm and no index drop.
4. `output` is a method, not a free fn. ECHILD: `wait_exit` returns it, as the frozen `exited` doc says;
   only `end_children` accepts it. `end_children` signals all before waiting. Test (e) uses a label and pod
   per round and a detach guard. Reconcile passes `Never` through.
