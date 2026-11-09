# Remotes and pushing

agentfs pushes a volume's history to git remotes, over HTTPS or SSH, or to local
paths. This page says how remotes are configured, how and when pushes run, what
happens when one fails, and how several volumes share a repository. Every key is
in [configuration.md](configuration.md); the operator's view of failures is in
[runbooks/stuck-pushes.md](runbooks/stuck-pushes.md) and
[runbooks/divergence.md](runbooks/divergence.md).

## Which remotes

- **Any git remote.** GitHub, Gitea or a bare repository on a server all work the
  same way. Local paths and `file://` URLs work too.
- **Several push remotes.** Each has its own push state and alarms, and each is
  pushed from its own thread, so a slow or hung remote cannot delay another.
- **Zero remotes is never an error.** A volume with no push section is
  **local-only**: it raises no alarm and pushes nothing (`agentfs push` prints
  `local-only: nothing to push` and exits 0). A remote added later gets all the
  history with nothing else changed, and one removed again leaves the volume
  local-only. A local-only volume dies with its disk unless a remote is added.
  `agentfs status` shows the mode (`remotes:`), and so does the gauge
  `agentfs.remotes.mode`. The three modes are **pushed** (one or more valid
  push sections), **none valid** (push sections, all invalid: each alarms) and
  **local-only**.
- A section with `agentfsForkRemote = true` is never a push section
  ([forks.md](forks.md)).

## Configuration

Remotes are the standard `[remote "<name>"]` sections (`url`, and `pushurl`,
which wins as in git) plus agentfs's own keys, read only from the history
repository's own config file (and its operator includes). libgit2's system, XDG,
global and ProgramData config and its home directory are never read. Pushes use
anonymous remotes with explicit `<oid>:<ref>` refspecs, so `remote.<name>.push`
and `fetch`, any global `url.<base>.insteadOf` or `http.*` setting, and
`~/.ssh/known_hosts` are never consulted.

**An invalid remote section never stops the volume.** The section is left out
(never pushed, and its limit does not apply), `RemoteMisconfigured` is raised
when the volume opens, `agentfs status` lists it and `agentfs push` reports it
and exits 1. Recording, committing and the other remotes carry on. A section is
invalid when:

- its name is not a valid agentfs name (see [Names](#names));
- a credential, key, passphrase, agent or known_hosts path is not absolute, or a
  size limit is not a positive size;
- its push URL is SSH and `agentfsKnownHosts` is missing, not exactly one of
  `agentfsSshKey` and `agentfsSshAgent` is set, `agentfsSshKeyPassphrase` is set
  without a key, `agentfsCredentials` is set, the URL carries a password, or the
  URL has no host or a bad port;
- its push URL is not SSH and an SSH key, passphrase, agent or known_hosts key
  is set;
- its push URL is SSH and the binary was built without the `ssh` feature;
- an LFS endpoint is not `http(s)`, `file://` or a path, or is `https` in a
  build without the `https` feature (a bad global `lfs.url` invalidates every
  remote);
- `lfs.concurrenttransfers` is not 1 to 64 (every remote);
- a proxy is not one agentfs can use (below).

Credentials or keys that are missing or unreadable **at push time** are push
failures instead (`auth` for a credential or key, `untrusted_host` for
known_hosts), so a Secret mounted late or being rotated works once it appears.

## When pushes run

- **Every `pushInterval`** (default 60 s), independent of the workload's
  lifecycle. Nothing waits for a pod to stop. The first push of each remote
  happens as soon as the daemon starts.
- **Fast-forward only.** Unpushed commits stay safe in the state area and go out
  on the next successful push.
- **Backoff.** After a failure the next attempt waits `pushInterval` doubled for
  each consecutive failure after the first, up to `max(pushInterval, 15m)`. A
  success resets it.
- **Push now.** `agentfs push` against a running daemon commits what is recorded
  and wakes each chosen remote's loop at once, past its interval and backoff. It
  waits up to `--timeout` (default 120 s, 1 s to 10 m), prints the same lines as a
  direct push, and exits 1 unless every chosen remote succeeded in time; a push
  still running then carries on. With no daemon it takes the state lock and
  pushes directly.
- **Network timeouts.** A connection gives up after 30 s and a stalled read or
  write after 5 min, for libgit2 transports and agentfs's SSH transport alike. A
  remote that accepts the connection and stops responding ends as an
  `unreachable` failure with the usual alarm and backoff. `agentfs push` pushes
  remotes in turn and moves on to the next.
- **Push state** is kept per remote in `meta/push/<remote>.json`: the last pushed
  commit, the last attempt and success times, consecutive failures, the last
  error and any divergence. It is bookkeeping only; a missing or unreadable file
  is treated as empty. A per-remote lock keeps two attempts on one remote from
  overlapping.

**At shutdown**, after the final commit, `mount`, `run` and `serve-history` give
each remote that lacks commits one more attempt, all within `--push-grace`
(default 5 s), never retrying. At the deadline every attempt is cancelled: one
that has not finished negotiating sends nothing and is reported as cancelled,
not as a failure. A transfer may be dropped, but a ref update completes or never
starts. A push to a local path or `file://` remote that is past negotiation at
the deadline is waited for, up to 10 s more, because exiting inside it leaves a
ref lock ([runbooks/stuck-pushes.md](runbooks/stuck-pushes.md#at-shutdown)).
Pushes take at most `--push-grace` plus 10 s from the stop signal, not counting
the final commit. A pod needs a `terminationGracePeriodSeconds` above that sum
plus the commit; under the CSI driver the node plugin kills the service 30 s
after it asks it to stop, so the sum must stay under 30 s (the driver sets
`--push-grace 5s`). `--push-grace 0s` skips the final push. Each remote's
result is printed (`final push: <remote>: ...`).

## Limits

- **Blob size.** `remote.<name>.agentfsMaxBlobSize`: commits are checked against
  it before they are made, and one history goes to every remote, so the smallest
  limit applies. LFS content is exempt.
- **LFS object size.** `remote.<name>.agentfsMaxLfsObjectSize`: the smallest
  limit applies when a file is recorded, and each remote also checks it on the
  objects it would upload (failure kind `too_large`, stage `lfs`) before any
  request.
- **Maximum push size.** `remote.<name>.agentfsMaxPushSize` keeps each push under
  a host's limit. Before pushing, the attempt lists the remote's branch, packs
  the commits since it and counts the bytes. Only the git pack counts; LFS
  uploads go to their own servers. A backlog that fits is pushed as usual. A
  backlog that does not fit is pushed in **steps**: the longest prefix of the
  first-parent chain that fits, found by bisection. A step pushes the branch
  alone, uploads only its commits' LFS objects, holds back `diverged` and
  `applied/<n>` refs for the full push, raises `PushLimited` (a warning), and
  makes the next attempt follow at once. `agentfs push` keeps stepping until the
  remote is up to date. A single commit over the limit, or a backlog whose last
  pushed commit is not on the head's first-parent chain, fails as `too_large`
  with nothing sent; the operator raises the limit, or sends large files to LFS
  from then on. A commit already made can go out only by rewriting history.
  Measuring costs one more connection and a second pack computation per
  attempt, and the pack libgit2 sends can differ slightly from the measured one.
  Remotes without a limit measure nothing.
- **git's fsck rules** are always enforced, not per remote: history may later
  go to a stricter remote, and a commit no remote accepts can never be pushed.

## When a push is rejected

A rejected push (refused, non-fast-forward, authentication failure, untrusted
host, unreachable remote, or any of these while uploading LFS objects) **never
affects the filesystem**. Recording, committing and other remotes carry on. The
recorder raises `PushFailed` with the reason and the commit, retries with backoff,
and keeps the remote's last successful commit so the backlog is visible.

Failure kinds: `unreachable` (connections, timeouts, HTTP 429 and 5xx), `auth`
(401, 403, a missing or rejected credential), `untrusted_host` (an SSH host key
or TLS certificate that fails verification), `non_fast_forward`, `too_large`,
`refused` (any other status or error) and `local` (a missing cache object, no
endpoint, no `https` feature).

**One branch per worktree.** A worktree creates or fast-forwards its own branch,
as the remote shows it. In the push negotiation, from the old value the remote
reports for the branch, agentfs decides: no such branch is a create; the head
itself is up to date; a commit known here and an ancestor of the head is a
fast-forward, leased on that value; anything else is someone else's, and the
push stops before any ref or object is sent. The push record is never an input.
Smart HTTP and SSH servers compare and swap; libgit2's local and `file://`
transports do not, so a writer racing between the negotiation and the update
there can be overwritten.

### Divergence

A non-fast-forward on the branch means someone else wrote it.

- The recorder pushes its head to
  `refs/agentfs/<volume>/diverged/<UTC timestamp>` (`YYYYMMDDTHHMMSSZ`), created
  once per divergence, raises `Diverged`, then keeps raising `PushFailed`
  (`non_fast_forward`) at every attempt, with the usual backoff.
- The save follows the head. At each attempt whose head moved past the one saved,
  the head is pushed to the same ref again: a fast-forward moves it, a ref the
  remote lacks is re-created, and one another writer moved is kept, never
  overwritten. A diverged worktree's later commits thus reach the remote under
  its own name.
- The name is chosen and saved as pending before the push and confirmed after,
  so a crash in between reuses it.
- It never force-pushes.
- The branch push carries only the branch and the `diverged/*` and `accepted/*`
  refs, created where the remote lacks them and otherwise left.
- The divergence clears when a branch push next succeeds. Resolve it with
  [runbooks/divergence.md](runbooks/divergence.md).

### The operator's lease push

`agentfs push --remote <name> --force-with-lease <oid>` replaces this worktree's
own branch, and nothing else, on one remote, with the local head. It runs
directly, never through a daemon, under that remote's push lock and not the state
lock, so it works while the volume is mounted. It lists the remote's branch: at
the head is up to date; not at `<oid>` is a broken lease, and nothing is
written. A lease commit not in the state area is fetched, with the remote's
branch, into a scratch repository beside the push lock, so the state area never
takes in the remote's objects. Exit status is 0 for pushed and up to date, 1 for
a broken lease, an unrelated history, recorded history or a failure, and 75 while
another push to that remote runs.

It refuses to replace history agentfs wrote:

- A branch that **shares no commit** with the head is never replaced; the volume
  continues under a new name.
- A branch whose commits beyond the head hold history an agentfs recorder wrote
  is refused. The push may drop only commits no recorder made (no `Agentfs-Seq`
  trailer: a foreign writer), and recorded commits the head holds rewritten (the
  same `Agentfs-Seq`, author and committer, names, emails and times, under
  another id, as a history rewrite keeps). A recorded commit the head lacks is
  this volume's newer history from another copy of its state area, or another
  worktree's under the same name, so the branch is not this state area's to
  replace.

Limits: a branch a person built on this volume's pushed commits, under the name
`agentfs.branch` gives, is replaced, because every commit dropped lacks a trailer
and git cannot tell it from a foreign writer. The operator checks whose commits
the branch holds first (the divergence runbook's inspect step). This volume's
commits rewritten by a person (amended, rebased, cherry-picked, squashed with
trailers kept) carry `Agentfs-Seq` but are new records, so the branch is refused;
the operator continues under a new name. Forged trailers fool the check.

### The start-branch alarm

A volume started from another worktree's branch (a start class, see
[csi.md](csi.md)) keeps its start point in its identity
(`agentfs.start.remote`, `.branch`, `.commit`). At every push attempt to the
start remote it lists the start branch first. If the branch holds commits this
volume does not have (the tip unknown here, or not an ancestor of the head or of
one of its `refs/agentfs/<volume>/accepted/*` refs), it raises `StartBranchMoved`
at every such attempt, naming the branch, the pinned commit and the tip. The
alarm is kept in the remote's push state for `agentfs status` and the gauge
`agentfs.start.moved`. It never stops a push.

This is how an old node's late commits, pushed to the old branch after a volume
moved, are saved and alarmed, never overwritten. The operator takes the commits
in (a patch, or by hand) or decides against them, then runs
`agentfs start accept --tip <tip>`, which fetches the start branch, checks the tip
is on it and creates `refs/agentfs/<volume>/accepted/<tip>` (never moved), pushed
create-only with the branch ([runbooks/start-branch-moved.md](runbooks/start-branch-moved.md)).
The alarm waits for the new volume's next push and watches only the start remote;
nothing is fetched to raise it.

### The patch queue's push

The patch queue ([patches.md](patches.md)) has its own push, to the patch remote
only, after every branch push attempt, failed ones included, so the status reaches
the remote even while the branch cannot be pushed. It writes only by the branch
rule: it lists the branch and the volume's refs, and writes nothing if the branch
is not this worktree's (reported as `queue: nothing written: ...`, not counted as
a queue failure, and retried at the next attempt). Otherwise it pushes:

1. the status ref, forced (the only ref agentfs ever force-pushes: it is a
   snapshot), and, only when the branch push succeeded, every `applied/<n>` the
   remote lacks, never forced;
2. then, only when the branch push succeeded, the deletion of each remote
   `inbox/<n>` whose number is applied, leased on the value listed, so a ref a
   producer wrote meanwhile stays.

Each push also carries the branch as a no-op update leased on the value listed, so
the push stops if the branch moved. A queue push failure raises `PushFailed` with
stage `queue`, is kept as the remote's `queue_error` and makes `agentfs push` exit
1, but never counts toward the branch's failures, backoff or divergence.

## Shared repositories

Several volumes may share one repository. Each volume has a name (`{volume}`, set
by the operator) and owns a branch and a ref namespace in it:

| Ref | Purpose |
|---|---|
| `refs/heads/agentfs/<volume>` | The volume's history (configurable: `[agentfs] branch`) |
| `refs/agentfs/<volume>/inbox/<n>` | A queued patch: `<n>` is ten zero-padded digits from `0000000001`; written by producers, never pushed by the recorder |
| `refs/agentfs/<volume>/applied/<n>` | The patch commit applied as `<n>`; pushed before `inbox/<n>` is deleted on the remote |
| `refs/agentfs/<volume>/status` | The queue's status for iac: a parentless commit with `status.json`, force-pushed |
| `refs/agentfs/<volume>/held/<n>` | Local only, never pushed: marks a set-aside patch number so its inbox entry is not treated as stale |
| `refs/agentfs/<volume>/approved/<n>` | Written by `agentfs patch approve` on the patch remote: names the tip of patch `n`, for the `approval` acceptance rule |
| `refs/agentfs/<volume>/change-line` | The checkpoints `agentfs checkpoint` cuts and the patches they merged; pushed in place of the branch to a remote with `agentfsPush = changeLine` |
| `refs/agentfs/<volume>/diverged/<timestamp>` | A saved divergence |
| `refs/agentfs/<volume>/accepted/<tip>` | A start-branch tip the operator accepted; created once, never moved |
| `refs/heads/agentfs/<fork>` | A fork's history, on its fork remote, fixed at create |
| `refs/agentfs/<fork>/history` | A fork's history when the fork's push target is `hidden`: never under `refs/heads/` |
| `refs/agentfs/<fork>/change` | A fork's change, one commit on its base; pushed create-only, never moved |
| `refs/agentfs/<fork>/finish` | A fork's finish commit; pushed create-only, never moved |

**Pushing.** A recorder pushes only its own volume's refs, with explicit
refspecs, so pushes from different volumes never contend: the branch, `diverged/*`
and `accepted/*`, and to the patch remote the status and `applied/*`, then
deletions of `inbox/*`. It never pushes an inbox ref. The one exception is a
source that received forks while local-only: those forks' refs are its history,
and it pushes them create-only ([forks.md](forks.md)).

**Fetching.** At mount a recorder fetches only its own volume's `inbox/*` from the
patch remote, and deletes local inbox refs the remote no longer has.

**Single writer.** Each recorder is the only intended writer of its volume's refs.
Remotes cannot always enforce that: GitHub can restrict pushers only on
organisation-owned repositories, and tokens are scoped to repositories, not
branches. Every volume sharing a repository can therefore write every other
volume's refs. agentfs **detects** other writers (divergence) rather than relying
on enforcement.

### Names

Volume and remote names must match `[A-Za-z0-9][A-Za-z0-9._-]*`, without `..` and
not ending in `.` or `.lock`. They become ref components and state file names; a
`/` in a volume name would let one volume's `refs/agentfs/<volume>/` namespace
contain another's. Volume names must be unique within a repository: two recorders
with the same name see each other as divergence.

## Credentials and transports

### HTTP and HTTPS

`remote.<name>.agentfsCredentials` is an absolute path to a file holding either
git-credential `key=value` lines (`username=`, `password=`; other keys are
ignored) or a bare token, used as the password with the username from the URL,
else `x-access-token`.

- Every credential file is re-read when its stat stamp (device, inode, size,
  mtime, ctime) changes. The stamp follows symlinks, so kubelet's atomic `..data`
  swap for Secret volumes and a rename over the file are both seen. An in-place
  rewrite of the same size within the filesystem's timestamp granularity is
  missed.
- A credential or key is bound to the host and port of the remote's push URL and
  is never given to any other host.
- It gets one attempt per connection: a rejected credential is an authentication
  failure, not a retry loop.
- `mount` refuses a credential file under the backing tree or the mount point.
- HTTPS needs the `https` cargo feature (OpenSSL).

### Proxies

Proxies come from git's own keys in the state area's config only: `http.proxy` for
every remote, and `remote.<name>.proxy`, which overrides it for one remote (an
empty value: no proxy for that remote). Proxy environment variables
(`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`) are never read. A proxy is
a plain HTTP proxy, `http://host[:port]` (a bare `host[:port]` too; port 1080 by
default). It carries libgit2 `https` traffic and all LFS requests, through
`CONNECT`, so TLS is still verified end to end.

- SSH git traffic never goes through a proxy, and neither does a plain `http://`
  git remote's: `agentfs status` says `LFS only` for such a remote.
- Refused, so no proxy secret ever sits in config: a user or password (proxy
  authentication is not supported), `https://` and SOCKS proxies, a path.
- A bad `http.proxy` invalidates every remote; a bad `remote.<name>.proxy`, that
  remote. A proxy that cannot be reached fails the push as `unreachable`.

### SSH

`ssh://`, `ssh+git://`, `git+ssh://` and scp-like `[user@]host:path` remotes need
the `ssh` cargo feature. agentfs runs `git-upload-pack` or `git-receive-pack`
over its own SSH session (pure Rust, no OpenSSL or libssh2). A connection to a
host or port other than the push URL's fails closed. A local path containing a
`:` after a `/` is treated as local, as git does, and with the `ssh` feature its
push fails closed.

- **Authentication** uses exactly one of a key file and an ssh-agent. The user is
  the URL's, else `git`.
- `remote.<name>.agentfsSshKey` is an absolute path to a private key, OpenSSH or
  PEM (`RSA`, `EC`, PKCS#8; not DSA). No public key file is needed; a `.pub` file
  is refused with an `auth` failure that says why. A key that cannot be decoded
  is an `auth` failure with nothing sent. A refusal is an `auth` failure naming
  the key's SHA256 fingerprint and the user.
- An encrypted key (OpenSSH, PKCS#8 encrypted, or legacy RSA PEM with AES-128-CBC)
  needs `remote.<name>.agentfsSshKeyPassphrase`, an absolute path to a file
  holding the passphrase (one trailing newline dropped; 64 KiB at most; re-read
  when it changes). A missing or wrong passphrase is an `auth` failure and the
  key is never offered. Other legacy PEM encryption is refused with a hint to
  rewrite the key in OpenSSH format.
- `remote.<name>.agentfsSshAgent` is an absolute path to an ssh-agent's Unix
  socket. `SSH_AUTH_SOCK` is never read: an agent is per remote and explicit.
  Each identity is offered once, in the agent's order, until the server accepts
  one, so sshd's `MaxAuthTries` bounds how many are useful. An unreachable
  socket, an empty agent or every identity refused is an `auth` failure.
- **Host keys** are verified before any authentication against
  `remote.<name>.agentfsKnownHosts`, an absolute path to an OpenSSH known_hosts
  file, required for every SSH remote. The host is looked up as the push URL's
  host, or `[host]:port` on a port other than 22. Plain and hashed names, comma
  lists, `*` and `?` wildcards, `!` negation and `@revoked` are honoured. An
  unknown host, a changed key, a key type the file does not list for the host and
  a revoked key are all refused, the key is never offered, and the push fails as
  `untrusted_host` with the file, the name looked up, the key's SHA256 fingerprint
  and the `ssh-keyscan` command that lists the server's keys. Nothing is ever
  accepted on first use or written back.
- **Host certificates.** An `@cert-authority` line trusts host certificates its
  CA signed for the hosts the line matches. A presented certificate is trusted
  only if it is a host certificate, its CA is on a matching `@cert-authority`
  line and not revoked, the certified key is not revoked, the signature verifies,
  it is within its validity, its principals include the push URL's host (an empty
  list is refused), and it has no critical options. A plain key line never trusts
  a certificate, and an `@cert-authority` line never trusts a plain key. A
  refusal is `untrusted_host` naming the CA's fingerprint.
- Host key types known_hosts lists for the host are asked for first, then every
  other type supported, so known_hosts needs only one of a server's keys.
- Connecting times out after 30 s, and the handshake and every read or write of
  the git command after 5 min without progress. A command that fails is reported
  with its exit status and stderr.

RSA keys depend on a pure-Rust RSA crate with a known timing side-channel advisory
on private-key operations (RUSTSEC-2023-0071); Ed25519 keys are recommended.

## Git LFS transfers

What is stored as LFS, the pointer format and the cache are in
[recording.md](recording.md#large-files-and-lfs). This section covers moving the
objects.

### Endpoints

Endpoints come only from operator config (a tracked `.lfsconfig` is never read),
with git-lfs's precedence:

| Uploads (push) | Downloads (restore) |
|---|---|
| `lfs.pushurl` | |
| `lfs.url` | `lfs.url` |
| `remote.<name>.lfspushurl` | |
| `remote.<name>.lfsurl` | `remote.<name>.lfsurl` |
| derived from `pushurl`, else `url` | derived from `url` |

- An explicit `http(s)` URL is the endpoint as written. A remote URL derives
  `<url>/info/lfs` when it ends in `.git`, else `<url>.git/info/lfs`, as git-lfs
  does (GitHub, GitLab and Gitea serve LFS there).
- A local path or `file://` URL uses git-lfs's standalone-file layout:
  `<git dir>/lfs/objects/aa/bb/<oid>`, which `git lfs pull` in a clone of it
  reads.
- An SSH remote with no explicit endpoint uses the one `git-lfs-authenticate`
  names: agentfs runs it on the git server over its own SSH transport, bound to
  the push URL's host and port, with the remote's known_hosts, key or agent and
  user. Uploads use the push URL's path, downloads `url`'s, which must be on the
  same host and port (else set `lfsurl`). The answer's `href` is the endpoint and
  its `header` goes to batch requests; it is reused until shortly before it
  expires. A refused host is `untrusted_host`, a refused key `auth`, no connection
  `unreachable`, and a non-zero exit or JSON agentfs cannot read `refused`, with a
  hint to set `lfsurl`. There is no fallback to a guessed https URL.
- `git://` remotes derive nothing and need an explicit `lfsurl`.

### Credentials

Credentials are bound to the endpoint's origin (scheme, host and port) and never
sent anywhere else:

1. `remote.<name>.agentfsLfsCredentials` (an absolute path, same format as
   `agentfsCredentials`), if set;
2. else, when the endpoint has the push URL's origin (as GitHub's derived
   endpoint does), `remote.<name>.agentfsCredentials`;
3. else a password in the endpoint URL;
4. else none.

A separate LFS host never sees the git host's credentials, and an `http` endpoint
never gets those of an `https` push URL. For an endpoint `git-lfs-authenticate`
names, its `Authorization` header is the credential. agentfs's `Authorization`
goes to batch requests, and to an action's `href` only when it has the endpoint's
origin and the server gave the action none of its own. Redirects are never
followed. Files are re-read when they change.

### Client

agentfs has its own LFS client (pure Rust HTTP, TLS through OpenSSL and the system
trust store with the `https` feature). It speaks the batch API with the `basic`
transfer only: batches of 100 objects, in sequence, a `PUT` of each object the
server lacks, then `verify` when the server asks; a `GET` for downloads, hashed
while it is written and kept only if it matches. Within a batch up to
`lfs.concurrenttransfers` objects (default 8) move at once. An upload starts no
new object after its first failure and reports that failure; a download keeps
going past an object one server lacks (404) and stops at a failure of the server
itself. Timeouts: 30 s to connect, 5 minutes for a batch or verify request, and
for an object 5 minutes plus its size at 256 KiB/s.

### Resuming

Downloads resume by byte. An object is written into
`<state>/lfs/incomplete/<oid>` under an exclusive lock, so one download of an
object runs at a time. The partial survives failures, commands and restarts; the
next download hashes what it holds and sends `Range: bytes=<n>-`. A `200` starts
again from the first byte; a `416` or a `206` starting elsewhere drops the partial
and asks once for the whole object. A finished object is checked (size and
SHA-256), fsynced and renamed into the cache; a mismatch deletes it (`refused`).
An object is tried up to 3 times when the server cannot be reached, 1 s then 2 s
apart.

Uploads resume by object: an attempt that fails keeps what it finished on the
server, and the batch API skips those objects at the next attempt. Byte-level
upload resume is not supported.

### Upload before push

Before every push attempt, each remote uploads the LFS objects the commits it
lacks reference: blobs a commit adds or modifies at paths in that commit's managed
block that are canonical pointers. Pointer text a workload wrote at an unmanaged
path is ordinary content. Every object must be in the local cache. Only when every
upload succeeds does the git push run; any failure fails the attempt with stage
`lfs` and pushes no ref, with the usual alarm and backoff. A remote whose backlog
references no LFS object never contacts an endpoint, so a `git://` remote without
`lfsurl` keeps pushing until LFS content appears.

There is no per-push budget for LFS bytes: objects are uploaded one by one,
outside the git pack, and an interrupted upload resumes per object.

## Where the tests are

The remote transports and LFS are tested end to end in CI; see
[development.md](development.md).
