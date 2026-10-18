# Configuration reference

Every setting agentfs reads, what it accepts, and what happens when it is
wrong. The design is in [design.md](design.md) (§4.3 settings, §9
identities, §11 remotes); how to run a mount is in the
[operator guide](operator-guide.md). The Helm charts' values are in their
READMEs, generated from `values.yaml` and checked in CI
([helm.md](helm.md#3-values)).

**This page is checked against the code.** Tests fail when a config key,
operator attribute, flag or environment variable that agentfs reads is
missing here, or when a row names one it no longer has
(`crates/agentfs-core/tests/docs_reference.rs`,
`crates/agentfs/src/reference_tests.rs` and the `reference_tests` module
of `crates/agentfs-csi/src/main.rs`; the `docs` CI job). They rely on
these conventions, so keep them when editing:

- each key or attribute is a table row in its section (`[agentfs]`,
  `[agentfs "identity"]`, `[lfs]`, `[remote "<name>"]`, Includes,
  Operator attributes) whose first cell is exactly the key in backticks
  (remote keys without the `remote.<name>.` prefix);
- each flag is a row of a table under "Flags and environment" whose
  first cell starts with the flag in backticks (`` `--state DIR` ``), with
  its variable in backticks in the same row and, in the Commands column,
  each subcommand that takes it in backticks (`` `patch submit` ``) or
  `all` for the global flags;
- every variable is listed in backticks under "Environment variables".

## Where settings live

Settings are git config keys in the history repository's **own** config
file, `<state>/git/config`. Nothing else is read: libgit2's system, XDG,
global and ProgramData config, and its home directory, are cleared once per
process, so `~/.gitconfig`, `url.<base>.insteadOf`, `http.*` and
`~/.ssh/known_hosts` never affect a volume (design §11.1).

**The one exception: `include.path`.** The repository's config may
include operator-owned fragments, for example a Kubernetes ConfigMap
mounted read-only at `/etc/agentfs/config` (see
[deployment.md](deployment.md)). Their keys count as if they were in
`<state>/git/config`, at the point of the include, so a key after the
include in the own file wins, as in git. `agentfs init --include FILE`
sets the entries (repeatable; a re-run replaces them). agentfs is stricter
than git about includes, and every command that opens the volume fails
when one is wrong:

- the path must be absolute (git would resolve a relative one against the
  config file's directory);
- the file must exist and be a readable regular file (git and libgit2
  silently skip a missing include, which would make the operator's
  remotes and settings vanish without a sign);
- a fragment must not set `agentfs.volume` or `agentfs.branch`: the
  volume's identity belongs to its state area, set by `agentfs init`;
- conditional includes (`includeIf.<condition>.path`) are refused;
- `agentfs mount` refuses an include under the backing tree or the mount
  point, where the workload could rewrite it, like a credentials file.

The `git` CLI reads the same includes, so `git --git-dir <state>/git
config --list --show-origin` shows where each key comes from.

### Includes

| Key | Type | Default | Meaning and validation |
|---|---|---|---|
| `include.path` | absolute path, repeatable | none | An operator config fragment whose keys count as the repository's own, at the point of the include. Must exist, be a readable regular file, not set `agentfs.volume` or `agentfs.branch`, and (for `mount`) be outside the backing tree and the mount point. Set by `agentfs init --include` |
| `includeIf.<condition>.path` | | | Refused: any conditional include fails every command that opens the volume |

Set them with `git config`:

```sh
git --git-dir /data/state/git config agentfs.pushInterval 30s
```

`mount`, `run` and `serve-history` read the settings when they start;
changing a key takes a restart. `push` (when no daemon runs), `record`,
`flush` and `status` read them at every run; `push` against a running
daemon uses the daemon's settings. The
*files* that remote keys point to (credentials, SSH keys, known_hosts) are
different: they are re-read whenever they change, without a restart.

**Two kinds of mistakes.** An invalid `agentfs.*` setting stops the
volume: every command that opens it fails with the key and the reason. An
invalid `[remote "<name>"]` section never does: that remote is left out
(never pushed, its limit does not apply), `RemoteMisconfigured` is raised
when the volume opens (the `agentfs.remote.misconfigured` gauge, an alarm
on stderr), `agentfs status` lists it as `misconfigured, not pushed`, and
`agentfs push` reports it and exits 1. Recording, committing and the other
remotes carry on.

Durations are `<integer><unit>` with unit `ms`, `s`, `m` or `h` (`200ms`,
`60s`, `5m`). Sizes are integers with git's optional `k`, `m` or `g`
suffix (`512k`, `100m`).

## `[agentfs]`

| Key | Type | Default | Meaning and validation |
|---|---|---|---|
| `agentfs.volume` | name | none: required | The volume's name, set by `agentfs init --volume`. Unique within any repository it pushes to; it names the branch and `refs/agentfs/<volume>/*` (§11.4). Letters, digits, `.`, `_`, `-`, starting with a letter or digit, no `..`, not ending in `.lock` or `.` |
| `agentfs.branch` | branch name | `agentfs/<volume>` | The history branch, set by `agentfs init --branch` |
| `agentfs.respectTreeRules` | boolean | `false` | Whether `.gitignore` and `.gitattributes` tracked in the tree take part in path rules (§4). With `false` they are content only |
| `agentfs.nested` | `exclude` or `working-tree` | `exclude` | Nested git repositories: `exclude` records nothing in them, `working-tree` records their working files but never their git directory (§5) |
| `agentfs.commitWindow` | duration | `200ms` | Records this close in time become one commit (§3.4) |
| `agentfs.pushInterval` | duration | `60s` | How often each remote is pushed; also the base of the failure backoff, which doubles up to `max(pushInterval, 15m)` (§11.2) |
| `agentfs.sweepInterval` | duration, at least `1s` | `30s` | How often the mount records files held open and retries failed records (§3.1, §3.5) |
| `agentfs.repackInterval` | duration, at least `1m`, or `0`/`off` | `1h` | How often the commit loop (`mount`, `run`) repacks the history with `git repack -d --geometric=2`, delta-compressing versions of files changed in place (§8). Needs the `git` CLI; skipped while a push runs or when nothing is loose; a failure raises `RepackFailed`. git runs with one thread and bounded windows, about 256 MiB, in a cleared environment; commits wait while it runs, and a stop kills it. `0` or `off` never repacks |
| `agentfs.patchRemote` | remote name, or `none` | the only remote, when exactly one is configured; else none | Where the patch queue (§12) is fetched from at mount, and the only remote that gets the queue push (status ref, `applied/<n>`, deleted `inbox/<n>`). A name that is not a valid remote means none. With none, patches already in the local inbox still apply |
| `agentfs.patchFetchTimeout` | duration, or `0`/`off` | `30s` | How long a mount (or `agentfs patch apply`) waits for the inbox fetch; on timeout or failure it raises `PatchFetchFailed` and applies what is already fetched. `0` or `off` skips the fetch |
| `agentfs.lfsThreshold` | positive size | unset: no size rule | Content larger than this (strictly `>`) is stored as Git LFS instead of a git blob (§7). Unset, only `agentfs.store=lfs` and `filter=lfs` in `info/attributes` select LFS. Setting it needs an LFS server for every remote (see [Git LFS](#git-lfs)) |
| `agentfs.forkRefs` | `branch` or `hidden` | `branch` | The push target new forks of this volume get (design §11.5), written into each fork's identity as `agentfs.fork.pushTarget`: `branch` pushes a fork's history to `refs/heads/agentfs/<fork>` on its fork remote, where CI and pull requests can act on it; `hidden` keeps it at `refs/agentfs/<fork>/history`, never under `refs/heads/`. Read only when a fork is created: changing it moves no fork that exists, so forks already pushing branches keep doing so until they finish or are abandoned. Any other value fails the volume (`BadChoice`). Under CSI, set it in the source's profile config |

Operator path rules are not config keys: they are `<state>/git/info/exclude`
(git ignore syntax: paths never recorded when new) and
`<state>/git/info/attributes` (`-agentfs` and the other `agentfs.*`
attributes, §4.2; see [Operator attributes](#operator-attributes)).

### Remotes modes

A volume's push sections decide how its history leaves the node (design
§11.1), in one of three modes, which `agentfs status` prints as its
`remotes:` line and telemetry reports as the gauge `agentfs.remotes.mode`
(attribute `mode`, value 1). Zero remotes is never an error and needs
nothing declared: remotes can come and go (design §19.1, 2026-10-17).

| Mode (`mode`) | When | What happens |
|---|---|---|
| pushed (`pushed`) | one or more valid `[remote "<name>"]` push sections | each is pushed; invalid sections beside them raise `RemoteMisconfigured` |
| none valid (`none_valid`) | push sections, all invalid | nothing is pushed; each raises `RemoteMisconfigured`; never called local-only |
| local-only (`local_only`) | no push section | nothing is pushed and no alarm is raised: `agentfs push` prints `local-only: nothing to push` and exits 0; DeleteVolume keeps the worktree whatever `onDelete` says (an unlinked fork: `onDelete: delete` removes it unpushed); forks publish into its own history |

A section with `agentfsForkRemote = true` is not a push section. A remote
added later gets all the history, with nothing else to change: the
branch, and after it the refs of every fork the volume received while
local-only, create-only (a remote holding one at another commit fails
the push and keeps its value). DeleteVolume waits until every remote
holds them too. A remote removed again makes the volume local-only
again.

A fork of a local-only source with no `agentfsForkRemote` section
publishes into the source's own history (R4): `refs/heads/agentfs/<fork>`
(or its hidden ref), `refs/agentfs/<fork>/change` and `/finish` in the
source's state area, through the source's running daemon when one holds
its lock, else under that lock. Nothing applies it: whatever reads the
source's history does the gating. The fork records where the source is
in `agentfs.fork.sourceState`. An unlinked fork (`agentfs.fork.link =
unlinked`) of such a source records no path: nothing it does reaches
the source.

An unlinked fork of any source never pushes to the source's remotes,
flagged or not: its remotes mode comes from its own remotes file
(`agentfs.fork.remotes`) alone, and without one it is local-only. A
local-only unlinked fork is the one volume whose history is not held:
DeleteVolume with `onDelete: delete` removes it unpushed (design §19.1
"Unlinked fork remotes").

### `[agentfs "identity"]`

Templates for commit identities (design §9). Variables: `{volume}`,
`{namespace}`, `{pod}`, `{podUid}`, `{node}`, `{replica}`, and in author
templates also `{uid}` and `{gid}`. They come from the pod flags
(`--namespace`/`AGENTFS_NAMESPACE`, `--pod`/`AGENTFS_POD`,
`--pod-uid`/`AGENTFS_POD_UID`, `--node`/`AGENTFS_NODE`,
`--replica`/`AGENTFS_REPLICA`). An unknown variable, an unclosed `{`, or a
rendered identity that is empty or contains `<`, `>` or a newline is an
error.

With several mounts of one volume (design §14), the history service
renders these templates once per front, with that front's pod flags,
and commits each front's changes with its own identity. A template that
does not render for a front's pod flags refuses that front when it
connects. If the templates change across a service restart so that a
record already journaled no longer renders, that record is committed
with the service's own committer, keeping the pod's trailers, and the
`IdentityFallback` alarm says so.

| Key | Default |
|---|---|
| `agentfs.identity.committerName` | `{replica}` |
| `agentfs.identity.committerEmail` | `{podUid}@{volume}.agentfs.invalid` |
| `agentfs.identity.authorName` | `uid-{uid}` |
| `agentfs.identity.authorEmail` | `uid-{uid}@{volume}.agentfs.invalid` |

### `[agentfs "fork"]`

A fork's identity (design §11.5), fixed when the fork is created
(`agentfs fork create`, or CSI CreateVolume with a fork class) and written
in the fork's own state-area config, beside `agentfs.volume`. Never set
them by hand, and never in an include: a fragment that sets any of them
fails with `IncludeSetsIdentity`. `source` and `base` are set together,
or neither, and `pushTarget`, `link`, `sourceState` and `remotes` only with them
(`PartialForkIdentity`); a fork's config must not set `agentfs.branch`.
A fork's history ref is its push target. A linked fork's only push
remote is its fork remote (see `agentfsForkRemote`), or, for a fork of a
local-only source, none: it publishes into the source's history
([Remotes modes](#remotes-modes)). An unlinked fork's push remotes are
the sections of its own remotes file, or none: it is local-only. It has
no patch queue.

| Key | Type | Default | Meaning and validation |
|---|---|---|---|
| `agentfs.fork.source` | name | unset: not a fork | The source volume's name, as `agentfs.volume` |
| `agentfs.fork.base` | full commit id | unset: not a fork | The source branch's head when the fork was created: 40 lowercase hex digits |
| `agentfs.fork.sourceState` | absolute path | unset: publishes to a fork remote | A linked fork of a local-only source (F8 S3): the source's state area, written at create; the fork publishes into its history there, unless a section sets `agentfsForkRemote`. A relative path fails the fork (`RelativePath`), and so does any value in an unlinked fork (`UnlinkedSourceState`) |
| `agentfs.fork.link` | `linked` or `unlinked` | `linked` | Whether the fork delivers back to its source (design §11.5 "Fork link"), from `fork create --link` or a fork class's `link` at create (written only for `unlinked`), never changed after. `linked`: its finish publishes the change to its fork remote, or into a local-only source's history; it waits and alarms while it cannot reach it, and never delivers anywhere else. `unlinked`: an independent fork that pushes its history ref only to its own remotes (`agentfs.fork.remotes`; none: local-only) and delivers nothing: `fork finish` refuses it (`Unlinked`), it has no `fork status`, and DeleteVolume treats it as any worktree. Any other value fails the fork (`BadChoice`) |
| `agentfs.fork.remotes` | absolute path | unset: an unlinked fork is local-only | An unlinked fork's own remotes (design §11.5, §19.1 "Unlinked fork remotes"), from `fork create --remotes FILE` or a fork class's `remotes` profile at create. The file is read alone at every open: its `[remote "<name>"]` sections are the fork's push remotes, with the same keys and modes as any volume's, and its `lfs.url` and `lfs.pushurl` the fork's (the source's are never read); it may hold nothing else (`ForkRemotesKey`, also for `agentfsForkRemote`), must be a readable file (`ForkRemotesUnreadable`) and must not be inside the backing tree or the mount point (`IncludeExposed`). A relative path fails the fork (`RelativePath`), and so does any value in a linked fork (`LinkedForkRemotes`) |
| `agentfs.fork.pushTarget` | `branch` or `hidden` | `branch` | Where the fork's history is kept and pushed, from the source's `agentfs.forkRefs` at create (written only for `hidden`): `branch` is `refs/heads/agentfs/<fork>`, `hidden` is `refs/agentfs/<fork>/history`. Any other value fails the fork (`BadChoice`) |

### `[agentfs "start"]`

A started volume's start point (design §11.3, §14.2): the commit of
another worktree's branch it began from, written once in its own
state-area config when it is created (CSI CreateVolume with a class's
`startFrom`) and never changed. Never set them by hand, and never in an
include (`IncludeSetsIdentity`). All three are set together, or none
(`PartialStartPoint`); they are read as a `startFrom` value is
(`BadStartPoint`). A started volume's own branch is still
`agentfs/<volume>`: it never writes its start branch. At every push to
the start remote, the push lists the start branch and raises
`StartBranchMoved` while it holds commits this volume has not taken in
or accepted ([start-branch-moved runbook](runbooks/start-branch-moved.md)).

| Key | Type | Default | Meaning and validation |
|---|---|---|---|
| `agentfs.start.remote` | name | unset: not started | The start remote: a push section of the volume's own profile |
| `agentfs.start.branch` | branch name | unset: not started | The start branch, without `refs/heads/` (`agentfs/<old volume>`); never a `refs/...` name, never this volume's own branch |
| `agentfs.start.commit` | full commit id | unset: not started | The pinned commit: the start branch's tip or an ancestor of it when the volume was created; 40 lowercase hex digits |

## Operator attributes

`<state>/git/info/attributes` uses git's attribute syntax. `agentfs.store`,
`agentfs.commit`, `agentfs.coalesce`, `agentfs.group` and `agentfs.maxSize`
count only there, never in a tracked `.gitattributes`, whatever
`agentfs.respectTreeRules` says (§4.1 covers `-agentfs` and `filter=lfs`).

| Attribute | Values | Default | Meaning and validation |
|---|---|---|---|
| `-agentfs` | | | Never record this path's content (a removal of a tracked path is still recorded) (§4.2) |
| `agentfs.store` | `blob` or `lfs` | §7 | Force a git blob or Git LFS; `blob` does not override `filter=lfs` ([Git LFS](#git-lfs)) |
| `agentfs.commit` | `close` or `fsync` | `close` | `fsync` puts the path in the `fsync` class for databases (§8): recorded as a whole group, at `fsync`, by a timer, at the group's last close and at unmount |
| `agentfs.coalesce` | duration, at least `1s` | `30s` | For the `fsync` class: the least time between two captures of a group at `fsync`, and the timer's delay, so the most a change waits. A group uses its members' smallest |
| `agentfs.group` | name | unset: the path alone | For the `fsync` class: paths in one directory with the same group and the same stem (`app.db`, `app.db-wal`, `app.db-journal`) are captured together |
| `agentfs.maxSize` | positive size (`k`, `m`, `g`: powers of 1024) | unset | Any path: content larger than this is refused (`PathRefused`, reason `max_size`), whether stored as a blob or LFS. In a group, it refuses the whole capture |

Values of `agentfs.commit`, `agentfs.coalesce` and `agentfs.maxSize` are
checked when the volume opens: a bad one fails `mount`, `run`, `record`
and the other commands with
`info/attributes line N: agentfs.coalesce=100ms: ...`. Attributes of
other names are not checked, as in git. For SQLite:

```text
*.db* agentfs.commit=fsync agentfs.group=sqlite agentfs.coalesce=30s
```

## `[lfs]`

git-lfs's own global keys, read from the same config file (never from a
tracked `.lfsconfig`, which is content only). They apply to every remote
and win over the per-remote keys, as in git-lfs; see [Git LFS](#git-lfs).

| Key | Type | Default | Meaning and validation |
|---|---|---|---|
| `lfs.url` | `http(s)://` URL, `file://` URL or path | unset | The LFS endpoint for every remote |
| `lfs.pushurl` | as `lfs.url` | unset | Where LFS uploads go for every remote |
| `lfs.concurrenttransfers` | integer, 1 to 64 | `8` | How many LFS objects each remote's transfer moves at once (see [Transfers](#transfers)) |

A bad value does not stop the volume: every remote is reported as
misconfigured instead.

## `[http]`

git's own proxy key, read from the same config file only. Proxy
environment variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`,
`NO_PROXY`) are never read; see [Proxies](#proxies).

| Key | Type | Default | Meaning and validation |
|---|---|---|---|
| `http.proxy` | `http://host[:port]` or `host[:port]` | unset: no proxy | The HTTP proxy of every remote that has no `remote.<name>.proxy`. The port defaults to 1080, as in git. A user or password, `https://` and `socks://` proxies and a path are refused, and a bad value makes every remote misconfigured |

## `[remote "<name>"]`

One section per push remote; every section with a `url` is a remote. The
standard git keys are read, plus agentfs's own. `remote.<name>.push`,
`fetch` and every other git key are ignored: pushes send only the volume's
own refs, as explicit refspecs, never forced (§11.4).

| Key | Type | Default | Meaning and validation |
|---|---|---|---|
| `url` | URL or path | none: required | Where the remote is |
| `pushurl` | URL or path | `url` | Where pushes go; wins over `url`, as in git. Everything below is checked against the push URL |
| `agentfsMaxBlobSize` | positive size | no limit | The largest blob this remote accepts. Checked before each commit; with several remotes the smallest limit applies (§3.4, §11.3) |
| `agentfsMaxLfsObjectSize` | positive size | no limit | The largest LFS object this remote's LFS server accepts. With several remotes the smallest applies when files are recorded: larger content stored as LFS is refused (`PathRefused`, `lfs_object_too_large`). A push also checks its own remote's limit before uploading (see [Size limit](#size-limit)) |
| `agentfsMaxPushSize` | positive size | no limit | The largest git pack one push to this remote may send, measured before connecting. A larger backlog is pushed in first-parent steps (`PushLimited`); a single commit over it fails as `too_large`. LFS uploads are not counted (see [Push size](#push-size), §11.3) |
| `agentfsCredentials` | absolute path | none | HTTP and HTTPS only: a file holding a token or `username=`/`password=` lines (see [HTTPS](#https-and-http-remotes)) |
| `agentfsSshKey` | absolute path | none | SSH only: the private key (see [SSH](#ssh-remotes)). An SSH remote needs exactly one of this and `agentfsSshAgent` |
| `agentfsSshKeyPassphrase` | absolute path | none | SSH only, with `agentfsSshKey`: a file holding the key's passphrase (one trailing newline dropped). Needed for an encrypted key; not read for an unencrypted one |
| `agentfsSshAgent` | absolute path | none | SSH only: the Unix socket of an ssh-agent whose identities are offered instead of a key file. `SSH_AUTH_SOCK` is never read |
| `agentfsKnownHosts` | absolute path | none | SSH only, and required there: the known_hosts file the server's host key is checked against |
| `lfsurl` | `http(s)://` URL, `file://` URL or path | derived from the remote's URL | The Git LFS endpoint for this remote, as in git-lfs, unless `lfs.url` is set (see [Git LFS](#git-lfs)); on an `agentfsForkRemote` section, before `lfs.url` |
| `lfspushurl` | as `lfsurl` | `lfsurl` | Where LFS uploads go, unless `lfs.pushurl` or `lfs.url` is set; on an `agentfsForkRemote` section, before them |
| `agentfsLfsCredentials` | absolute path | none | The credential file (same format as `agentfsCredentials`) for an LFS endpoint on another host or port than the push URL; never sent to any other host |
| `proxy` | as `http.proxy`, or empty | `http.proxy` | This remote's HTTP proxy, as in git; an empty value means no proxy for this remote. It carries the remote's `https` git traffic and all its LFS requests, never SSH or plain `http://` git traffic (see [Proxies](#proxies)) |
| `agentfsForkRemote` | boolean | `false` | This section is only for forks of the volume (design §11.5): never one of the volume's own push remotes, never its patch remote, and its limits never apply to the volume's records. With one such section, forks push there; with none, forks push to the volume's patch remote; two or more such sections are all invalid and forks are refused (`NoForkRemote`). While `lfs.url` or `lfs.pushurl` is set (the source's LFS store), such a section must set its own `lfsurl` or `lfspushurl`, which it then uses before the global keys; without one it is invalid (`RemoteMisconfigured`: a fork remote must name its own LFS endpoint), so a fork's LFS objects never reach the source's store |

The remote's name must be a valid agentfs name (as for `agentfs.volume`;
git accepts `team/backup`, agentfs does not), because it names the push
state file and appears in metrics.

**A remote is misconfigured** (left out, `RemoteMisconfigured`) when:

| Condition | Message names |
|---|---|
| the name is not a valid agentfs name | `remote.<name>` |
| `agentfsMaxBlobSize` is not a positive size | `remote.<name>.agentfsMaxBlobSize` |
| `agentfsMaxPushSize` is not a positive size | `remote.<name>.agentfsMaxPushSize` |
| `agentfsMaxLfsObjectSize` is not a positive size | `remote.<name>.agentfsMaxLfsObjectSize` |
| `proxy` is not a plain `http://` proxy without a user or password, or `http.proxy` is not | the key; a bad `http.proxy` misconfigures every remote |
| `lfs.concurrenttransfers` is not 1 to 64 | the key, for every remote |
| `agentfsCredentials`, `agentfsSshKey`, `agentfsSshKeyPassphrase`, `agentfsSshAgent`, `agentfsKnownHosts` or `agentfsLfsCredentials` is not an absolute path | the key |
| the push URL is SSH and `agentfsKnownHosts` is not set | `agentfsKnownHosts` |
| the push URL is SSH and neither or both of `agentfsSshKey` and `agentfsSshAgent` are set | both keys |
| `agentfsSshKeyPassphrase` is set without `agentfsSshKey` | `agentfsSshKeyPassphrase` |
| the push URL is SSH and `agentfsCredentials` is set | `agentfsCredentials` |
| the push URL is SSH and carries a password (`ssh://user:pw@host/…`) | `url` or `pushurl` |
| the push URL is SSH and has no host or a non-numeric port | `url` or `pushurl` |
| the push URL is not SSH and `agentfsSshKey`, `agentfsSshKeyPassphrase`, `agentfsSshAgent` or `agentfsKnownHosts` is set | the key |
| the push URL is SSH and the binary was built without the `ssh` feature | `url` or `pushurl` |
| `lfsurl`, `lfspushurl`, or the global `lfs.url` or `lfs.pushurl`, is not an `http(s)://` URL, a `file://` URL or a path (SSH and `git://` are refused) | the key; a bad global key misconfigures every remote |
| one of those is `https://` and the binary was built without the `https` feature | the key |

Problems with the files themselves (missing, unreadable, malformed) are
not misconfigurations: they fail each push attempt, with the usual alarm
and backoff, so a Secret mounted late or being rotated starts working as
soon as it is right. A missing or bad credential, key or passphrase, or an ssh-agent that cannot
be reached, fails as `auth`; a missing or unreadable known_hosts file as
`untrusted_host`.

`agentfs mount` additionally refuses to start (an error, not an alarm)
when any remote section, valid or not, names a credentials, key or
known_hosts file under the backing tree or the mount point: the workload
could read the secret, or rewrite known_hosts to trust any host.

### Push size

Some hosts refuse pushes over a size (GitHub: 2 GiB per push).
`agentfsMaxPushSize` keeps every push to a remote under its limit:

```sh
git --git-dir /data/state/git config remote.origin.agentfsMaxPushSize 1g
```

Before each attempt, agentfs packs the commits the remote lacks (since
the last commit pushed) with libgit2 and counts the bytes, stopping as
soon as the limit is passed; nothing is sent until it fits.

- **Everything fits:** the push goes out as usual.
- **The backlog does not fit** (it grew during an outage): only the
  longest run of commits along the branch's first-parent chain that fits
  is pushed, with the LFS objects those commits reference. A `PushLimited`
  warning (log `agentfs.push_limited`, metric `agentfs.push.limited`) says
  how many commits are left, and the next step follows at once. Diverged
  refs and the patch queue's applied refs wait for the last step.
  `agentfs push` keeps stepping until the remote is up to date (`pushed
  <a>..<b> (push size limit: N commit(s) left, ...)` per step).
- **One commit alone does not fit:** the push fails as `too_large`
  (`PushFailed`, an error alarm, with backoff) and nothing is sent. Raise
  the limit, or move large files to LFS from now on
  ([stuck pushes](runbooks/stuck-pushes.md)).

Only the git pack counts: LFS objects go to their own server. The pack
libgit2 then sends can differ slightly from the one measured, so leave
some headroom under a hard server limit.

### Transports

The transport is chosen from the push URL, as libgit2 does:

| Push URL | Transport | Build |
|---|---|---|
| a path, `file://…` | local | default |
| `http://…` | smart HTTP | default |
| `https://…` | smart HTTP over TLS | `--features https` (OpenSSL) |
| `ssh://…`, `ssh+git://…`, `git+ssh://…` (any case), scp-like `[user@]host:path` | SSH | `--features ssh` (agentfs's SSH transport on russh; no OpenSSL) |
| `git://…`, anything else | not supported for pushing | |

scp-like syntax needs a `:` before any `/`, as in git; `[user@host:port]:path`
names a port. A local path with a `:` after a `/` (`/srv/a:b.git`) is
treated as local, but libgit2 built with `ssh` sends it to its SSH
transport, so with that feature its push fails (closed: nothing is
offered and no host is trusted). Avoid `:` in local remote paths.

### Proxies

Remotes reached through an egress proxy name it with git's keys, in the
volume's config (or an included fragment):

```ini
[http]
    proxy = http://proxy.internal:3128      # every remote
[remote "mirror"]
    url = https://git.internal/acme/r.git
    proxy = ""                              # this one connects directly
```

- `remote.<name>.proxy` wins over `http.proxy`; an empty value turns the
  proxy off for that remote (agentfs never reads `NO_PROXY`, nor any
  other proxy environment variable).
- A proxy is a plain HTTP proxy. It carries the remote's `https://` git
  traffic (libgit2) and every LFS request of the remote, wherever its
  endpoint is, with `CONNECT`, so certificates are still verified against
  the system trust store. SSH remotes' git traffic never uses it, nor
  does a plain `http://` git remote's: libgit2 connects to such a server
  directly, so only its LFS requests use the proxy (`agentfs status`
  says `LFS only`).
- Not supported, and refused as a misconfiguration: a user or password
  (proxy authentication), `https://` and SOCKS proxies.
- A proxy that is down or refuses the connection fails pushes as
  `unreachable`. `agentfs status` prints `proxy: <url> (<key>)` under each
  remote that has one, with `git and LFS` or `LFS only`.

### HTTPS and HTTP remotes

`agentfsCredentials` names a file holding either:

- git-credential lines, `username=<user>` and `password=<token>` (other
  keys are ignored), or
- a bare token (one trailing newline is dropped), used as the password
  with the URL's user, else `x-access-token`.

The file is read when a server asks for credentials and re-read whenever
its stat stamp (device, inode, size, mtime, ctime) changes; symlinks are
followed, so kubelet's Secret updates and a rename over the file are both
seen. The credential is given only to the push URL's host and port, once
per connection: a rejected one fails the push as `auth`. Without the file
a server that asks for credentials fails the push as `auth`. A TLS
certificate that libgit2's verification refuses (an unknown or
self-signed CA, a host name it does not cover) fails it as
`untrusted_host`; a TLS handshake that fails before the certificate is
checked (a server that drops it) is `unreachable`. libgit2 uses the system's CA store.

### SSH remotes

```ini
[remote "origin"]
    url = ssh://git@git.example.com/acme/agent-history.git
    agentfsSshKey = /run/secrets/agentfs/origin-ssh/ssh-privatekey
    agentfsKnownHosts = /run/secrets/agentfs/origin-ssh/known_hosts
```

- **Build:** `cargo build -p agentfs --features ssh` (add `https` for both:
  `--features https,ssh`). Without it, SSH remotes are reported as
  misconfigured.
- **User:** the URL's (`git@` above), else `git`.
- **The key** is a private key in OpenSSH format
  (`BEGIN OPENSSH PRIVATE KEY`, what `ssh-keygen` writes by default) or
  PEM (`RSA`, `EC`, PKCS#8 `PRIVATE KEY`; not DSA). No `.pub` file is
  needed. Ed25519 is recommended: RSA signing uses the pure-Rust `rsa`
  crate, which has a timing side-channel advisory (RUSTSEC-2023-0071).
- **A key with a passphrase** needs `agentfsSshKeyPassphrase`, a file
  holding the passphrase (one trailing newline is dropped). OpenSSH
  format, PKCS#8 `ENCRYPTED PRIVATE KEY` (`ssh-keygen -m PKCS8`) and
  legacy RSA PEM with AES-128-CBC (`ssh-keygen -m PEM`) are decrypted;
  other legacy PEM encryption is refused with a hint to rewrite the key
  (`ssh-keygen -p -f <file>` writes OpenSSH format). The key is decrypted
  before connecting: without a passphrase file, or with a wrong
  passphrase (`cannot decrypt <key> with <file>`), the push fails as
  `auth` and nothing is offered. Both files are re-read when they change;
  after `ssh-keygen -p`, update the passphrase file too.

  ```ini
  [remote "origin"]
      url = ssh://git@git.example.com/acme/agent-history.git
      agentfsSshKey = /run/secrets/agentfs/origin-ssh/ssh-privatekey
      agentfsSshKeyPassphrase = /run/secrets/agentfs/origin-ssh/passphrase
      agentfsKnownHosts = /run/secrets/agentfs/origin-ssh/known_hosts
  ```

- **An ssh-agent** instead of a key file: `agentfsSshAgent` names its
  socket (an agent the operator runs, for example a sidecar that holds a
  hardware-backed key). It is connected after the host key check; each
  of its identities (keys and certificates) is offered once, in the
  agent's order, until the server accepts one. sshd counts each offer
  against `MaxAuthTries` (default 6), so keep a remote's agent to a few
  identities. A socket that cannot be reached, an empty agent, or an agent
  whose identities are all refused fails the push as `auth`, naming the
  socket and the fingerprints offered. The environment's `SSH_AUTH_SOCK`
  is never read: an agent is used only where it is configured.

  ```ini
  [remote "origin"]
      url = ssh://git@git.example.com/acme/agent-history.git
      agentfsSshAgent = /run/agentfs/ssh-agent.sock
      agentfsKnownHosts = /run/secrets/agentfs/origin-ssh/known_hosts
  ```

  Make one for the volume and add its public half as a deploy key with
  write access:

  ```sh
  ssh-keygen -t ed25519 -N '' -C agentfs-hermes -f ./ssh-privatekey
  cat ./ssh-privatekey.pub   # add as a deploy key with write access
  ```

- **known_hosts** is an OpenSSH known_hosts file. Build it from the
  server's keys and **check the fingerprints out of band** (the hosting
  provider publishes them; `ssh-keygen -lf known_hosts` prints them):

  ```sh
  ssh-keyscan git.example.com > known_hosts              # port 22
  ssh-keyscan -p 2222 git.example.com > known_hosts      # another port: [git.example.com]:2222
  ssh-keyscan -H git.example.com > known_hosts           # hashed names work too
  ssh-keygen -lf known_hosts
  ```

  The key types the file lists for the host are asked for first, in file
  order, as OpenSSH does, so one of the server's keys is enough. A server
  that has none of the listed types is refused with "lists only ...".
- **What is checked:** before any authentication, the server's host key
  is looked up for the push URL's host (`[host]:port` on a port other
  than 22). Plain and hashed names, comma lists, `*` and `?` wildcards,
  `!` negation and `@revoked` lines are honoured. An unknown host, a changed key,
  a key type the file does not list for the host, or a revoked key fails
  the push as `untrusted_host`, and the key is never offered to that
  host. The message names the file, the host as looked up, the key type,
  its `SHA256:` fingerprint and the `ssh-keyscan` command to list the
  server's keys. Nothing is ever learned or written back; a changed key
  may be an attack, so confirm it before updating the file.
- **Host certificates:** an `@cert-authority` line trusts every host
  certificate its CA signed for the hosts it matches. When one matches
  the push URL's host, agentfs asks the server for a certificate first
  (a plain key line for the host still works with a server that has no
  certificate). A certificate is trusted only if it is a host
  certificate, its CA is on a matching `@cert-authority` line, neither the
  CA nor the certified key is `@revoked`, the CA's signature verifies, it
  is within its validity period, its principals include the URL's host
  (without the port, case-insensitively; a certificate with no principals
  is refused), and it has no critical options. Otherwise the push fails as
  `untrusted_host` before any key is offered, with the CA's fingerprint
  and a hint to inspect the certificate with `ssh-keygen -L -f`.

  ```sh
  # The CA's public key, from the server's administrator
  printf '@cert-authority *.git.example.com,[git.example.com]:2222 %s\n' \
    "$(cut -d' ' -f1,2 host_ca.pub)" > known_hosts
  ```

- **Rotation:** replace any of the files atomically (write a new file and
  rename it over the old one, or update the Secret); the next push uses
  it, without a restart. Like credentials, a key is given only to the
  push URL's host and port, once per connection; a rejected key fails
  as `auth`.

**In Kubernetes,** keep the files in a Secret mounted outside the backing
tree. The `kubernetes.io/ssh-auth` type uses the `ssh-privatekey` key; a
passphrase goes in the same Secret:

```sh
kubectl create secret generic agentfs-origin-ssh \
  --type=kubernetes.io/ssh-auth \
  --from-file=ssh-privatekey=./ssh-privatekey \
  --from-file=passphrase=./passphrase \
  --from-file=known_hosts=./known_hosts
```

```yaml
      volumeMounts:
        - { name: origin-ssh, mountPath: /run/secrets/agentfs/origin-ssh, readOnly: true }
  volumes:
    - name: origin-ssh
      secret: { secretName: agentfs-origin-ssh, defaultMode: 0400 }
```

The key file's permissions are not checked (kubelet's mode varies); what
protects it is that it is outside the mount, which `agentfs mount`
enforces.

## Git LFS

Large files can be stored as Git LFS objects: history holds a small
pointer, and the content goes to an LFS server before the commits that
reference it are pushed (design §7). Ordinary clones get the content with
`git lfs pull`. It is off until you choose what goes to LFS.

### What is stored as LFS

In order, for each regular file when it is recorded:

1. **Never LFS:** empty files, `.gitattributes`, `.gitmodules`,
   `.gitignore`, `.mailmap` (any spelling git treats as one), `.lfsconfig`,
   symlinks, and paths holding a tab, CR or LF or too long for a
   `.gitattributes` line.
2. **`filter=lfs`** in `<state>/git/info/attributes` (or in a tracked
   `.gitattributes` when `agentfs.respectTreeRules = true`): LFS.
   `agentfs.store=blob` does not override it.
3. **`agentfs.store=lfs` / `agentfs.store=blob`** in `info/attributes`.
4. **`agentfs.lfsThreshold`:** larger content (strictly `>`) is LFS.
5. Otherwise a git blob.

```sh
git --git-dir /data/state/git config agentfs.lfsThreshold 8m
printf '*.psd filter=lfs\n*.sqlite agentfs.store=blob\n' >> /data/state/git/info/attributes
```

`agentfsMaxBlobSize` does not apply to content stored as LFS. agentfs
keeps a block of `filter=lfs diff=lfs merge=lfs -text` lines for the LFS
paths at the end of the root `.gitattributes` **in history only**; the
backing tree's own `.gitattributes` is never changed. Settings take effect
at the next `mount` or `run`.

### Endpoints

Where each remote's objects go, in git-lfs's order (the first that is set
wins):

| Uploads (push) | Downloads (restore) |
|---|---|
| `lfs.pushurl` | |
| `lfs.url` | `lfs.url` |
| `remote.<name>.lfspushurl` | |
| `remote.<name>.lfsurl` | `remote.<name>.lfsurl` |
| derived from the remote's `pushurl`, else `url` | derived from the remote's `url` |

A section with `agentfsForkRemote` (design §11.5) reads its own
`lfspushurl` and `lfsurl` before `lfs.pushurl` and `lfs.url`, and while a
global key is set it must have one, or it is invalid: the global keys name
the source's store, which a fork's objects must not reach.

| Remote URL | Derived endpoint |
|---|---|
| `https://github.com/acme/r.git` | `https://github.com/acme/r.git/info/lfs` |
| `https://git.example/acme/r` | `https://git.example/acme/r.git/info/lfs` |
| `/srv/r.git`, `file:///srv/r.git` | the files `/srv/r.git/lfs/objects/aa/bb/<oid>` (git-lfs's standalone-file layout) |
| `ssh://…`, scp-like | the endpoint `git-lfs-authenticate` names on the git server (run over agentfs's SSH transport, with the remote's key, known_hosts and user) |
| `git://…` | none: set `lfsurl` |

An explicit URL is used as written (no `/info/lfs` is added). `https`
endpoints need the `https` build feature. A tracked `.lfsconfig` is never
read; it only affects people who clone the history. `agentfs status`
shows each remote's upload endpoint and where its credentials come from,
or why it has none:

```text
remote:    origin https://github.com/acme/agent-history.git: last pushed …
           lfs: https://github.com/acme/agent-history.git/info/lfs (derived from remote.origin.url, credentials from agentfsCredentials)
```

### LFS credentials

A credential is only ever sent to its endpoint's origin (scheme, host and
port):

1. `remote.<name>.agentfsLfsCredentials`, if set (same format as
   `agentfsCredentials`: a token, or `username=`/`password=` lines);
2. else, if the endpoint has the push URL's origin, `agentfsCredentials`
   (GitHub, GitLab and Gitea derived endpoints work this way);
3. else a password in the endpoint URL;
4. else none (anonymous).

For an SSH remote's endpoint found with `git-lfs-authenticate`, the
header the command prints is the credential; `agentfsLfsCredentials` is
used only when it prints none. The answer is reused until it expires
(`expires_in`, `expires_at`), else asked for again at each push attempt.
A server without the command (GitHub, GitLab and Gitea have it; plain
`git-shell` does not) fails the push as `refused` at stage `lfs`: set
`lfsurl` for it. git-lfs's pure SSH protocol (`git-lfs-transfer`) is not
supported.

A separate LFS host never sees the git host's credentials, and an `http`
endpoint never gets those of an `https` remote. Redirects are not
followed; the remote's proxy, if set, carries LFS requests too, and
`HTTP(S)_PROXY` is ignored. The file is re-read when it
changes; `agentfs mount` refuses one under the backing tree or the mount
point.

Two examples:

```ini
# GitHub: the derived endpoint, the same token.
[remote "origin"]
    url = https://github.com/acme/agent-history.git
    agentfsCredentials = /run/secrets/agentfs/origin/token

# A separate LFS server with its own Secret.
[remote "backup"]
    url = https://git.example.com/acme/agent-history.git
    agentfsCredentials = /run/secrets/agentfs/backup/token
    lfsurl = https://lfs.example.com/acme/agent-history
    agentfsLfsCredentials = /run/secrets/agentfs/backup-lfs/credentials
```

### Transfers

Objects go to and come from the batch API in batches of 100, one batch
after another; within a batch up to `lfs.concurrenttransfers` objects
(default 8) move at once, per remote. With several remotes, each pushes
on its own thread with its own transfers.

```sh
git --git-dir /data/state/git config lfs.concurrenttransfers 16
```

- **Uploads** stop starting objects at the first failure; the ones in
  flight finish. The next attempt sends only what the server still lacks,
  since the batch API skips objects it has.
- **Restores** fetch every object they need before writing anything, all
  at once, from each remote in name order; a remote that lacks an object
  leaves it to the next one.
- **Downloads resume by byte.** An object being downloaded is written to
  `<state>/lfs/incomplete/<oid>`; when a download is interrupted (a
  timeout, a killed process, a restart) the next one asks the server for
  the rest with an HTTP `Range` request, and checks the SHA-256 of the
  whole object at the end. A server that ignores `Range` sends everything
  again. Each object is tried 3 times when the server cannot be reached.
- **Uploads resume by object**: a failed push keeps the objects it
  finished, and the next attempt sends only the rest. An object is never
  uploaded halfway and continued later (no LFS server agentfs can test
  supports git-lfs's experimental `tus` uploads).

### Size limit

LFS hosts cap the size of one object (GitHub: 2 GiB, up to 5 GiB on some
plans). `agentfsMaxLfsObjectSize` tells agentfs a remote's cap:

```sh
git --git-dir /data/state/git config remote.origin.agentfsMaxLfsObjectSize 2g
```

- **When a file is recorded**, the smallest cap across remotes applies to
  content stored as LFS (one history goes to every remote): a larger file
  is refused, with a `PathRefused` alarm (reason `lfs_object_too_large`),
  and stays in the backing tree. It is never stored as a git blob instead.
  The size on disk is checked before the file is copied, and the commit
  checks again what was recorded before the limit was lowered.
- **When a remote is pushed**, its own cap is checked on the objects it
  would upload: one over it fails the attempt as `too_large` at stage
  `lfs`, before anything is sent ([stuck pushes](runbooks/stuck-pushes.md#lfs-stage-failures)).

Unlike git pushes there is no per-push byte limit for LFS: objects go one
by one to their own server, and a failed upload resumes per object.

### The local cache

Every LFS object is kept in `<state>/lfs/objects` when it is committed,
and pushes upload from there. `agentfs lfs fetch` downloads objects into
it from the remotes, for example after the cache was lost, or before a
push to a new remote of commits whose objects are no longer cached:

```sh
agentfs lfs fetch                  # the objects the branch head references
agentfs lfs fetch agentfs/demo~5   # those of another commit's tree
agentfs lfs fetch --all            # every object of every commit on the branch
agentfs lfs fetch --all --dry-run  # what is missing, contacting no server
```

Remotes are asked in name order (or only those named with `--remote`),
each for the objects the ones before it could not provide. It takes no
state-area lock, so it runs beside a mount, and exits 1 unless every
object is cached afterwards.

`agentfs lfs prune` frees the cache. It is never run automatically, and
it deletes nothing on any server: a cached object is deleted only when
every remote's LFS endpoints (upload, and download if that is another
endpoint) confirm they hold it. A remote that is misconfigured or has no
reachable endpoint refuses the prune.

```sh
agentfs lfs prune --dry-run         # what would go, and which remote lacks what
agentfs lfs prune --older-than 24h  # keep what was cached in the last day
```

It runs beside a mount: it waits `--wait-lock` (default: not at all) for
restores and fetches using the cache and exits 75 while they still do. A
pruned object is downloaded again when a restore needs it; a remote added
later needs `agentfs lfs fetch --all` before its first push. See
[lfs-storage.md](runbooks/lfs-storage.md).

### When uploads fail

Uploads run before every push attempt of each remote, for the objects the
commits the remote lacks reference. A failure fails that attempt with
**stage `lfs`** (`failed (auth, stage lfs)` in `agentfs push` and the
alarm, `stage="lfs"` on `agentfs.push.failures`), no ref is pushed, and
the usual backoff applies. A remote with no LFS content to push never
contacts an LFS endpoint. See
[stuck-pushes.md](runbooks/stuck-pushes.md#lfs-stage-failures).

## Flags and environment

The settings above are per volume, in its config. The commands also take
flags, most with an environment equivalent (a flag wins over its
variable). `agentfs <command> --help` prints them too.

**Global flags**, accepted by every command:

| Flag | Environment | Default | Commands |
|---|---|---|---|
| `--state DIR` | `AGENTFS_STATE` | `.` | all |
| `--root DIR` | `AGENTFS_ROOT` | the current directory | all: required by `record` and `serve-history`; used by `flush`, `run`, `patch apply`, and `restore` and `revert` without a mount; must match the backing tree for `mount`; `init` creates it (mode 0755) when missing |
| `--wait-lock DURATION` | `AGENTFS_WAIT_LOCK` | `0s`: exit 75 at once | all: those that take the state-area lock wait this long for it; `lfs prune` waits this long for the LFS cache lock that restores and fetches hold |

**Pod flags**, which name where the volume is mounted (design §9; in
Kubernetes they come from the downward API). They fill the identity
templates' variables, the commit trailers and the telemetry resource:

| Flag | Environment | Default | Commands |
|---|---|---|---|
| `--namespace NAME`, `--pod NAME`, `--pod-uid UID`, `--node NAME`, `--replica NAME` | `AGENTFS_NAMESPACE`, `AGENTFS_POD`, `AGENTFS_POD_UID`, `AGENTFS_NODE`, `AGENTFS_REPLICA` | empty; `--pod-uid` defaults to the replica, and `--replica` (the committer's `{replica}`) to the pod name, else the host name | `mount`, `run`, `record`, `flush`, `push`, `serve-history`, `restore`, `revert`, `patch apply`, `fork finish`, `status` (accepted and ignored) |

**Command flags:**

| Flag | Environment | Default | Commands |
|---|---|---|---|
| `--volume NAME` | `AGENTFS_VOLUME` | none: required | `init`: the volume's name; `patch submit`: the volume the patch is for |
| `--branch NAME` | | `agentfs/<volume>` | `init`: the history branch; `patch submit`: the volume's branch |
| `--include FILE` (repeatable) | | keep the existing includes | `init`: exactly these `include.path` entries |
| `--passthrough auto\|on\|off` | `AGENTFS_PASSTHROUGH` | `auto` | `mount` |
| `--threads N` | `AGENTFS_THREADS` | CPUs, 2 to 16 | `mount` |
| `--no-allow-other` | | | `mount`: only the mounting user can use the mount |
| `--unmount-grace DURATION` | | `10s` | `mount`: how long shutdown waits for a busy mount |
| `--on-stale-mount refuse\|hold\|recover` | `AGENTFS_ON_STALE_MOUNT` | `refuse` | `mount`: when the mount point is an agentfs mount whose daemon died, `refuse` exits 1, `hold` waits for SIGTERM and detaches it, `recover` mounts over it (needs `CAP_SYS_ADMIN`; not for fronts) |
| `--seal-mountpoint` | `AGENTFS_SEAL_MOUNTPOINT` | off | `mount`: before mounting, make the bare mount point root's with mode 0000, so a non-root workload that reaches it while no daemon is mounted (a `subPath` bound while a sidecar restarts) gets `EACCES` instead of writing unrecorded; needs root; the shipped manifests set it |
| `--workload-mount PATH[=DIR]` (repeatable) | `AGENTFS_WORKLOAD_MOUNTS` (comma-separated) | the mount point, and its bind mounts | `mount`: where a workload sees the mount when not at the mount point (a container's `mountPath`; `=DIR` when it sees the tree's directory `DIR`, a `subPath`), so absolute `gitdir:` targets that `git worktree add` writes map exactly to nested repositories (design §5); a front sends them to its history service |
| `--push-grace DURATION` | | `5s` | `mount`, `run`, `serve-history`: how long shutdown may spend on a last push of the final commit (one attempt per remote that lacks commits); `0s` skips it |
| `--listen ADDR` | `AGENTFS_HISTORY_LISTEN` | `<state>/meta/history/history.sock` | `serve-history`: `unix:<path>` or an absolute socket path |
| `--shutdown-grace DURATION` | | `10s` | `serve-history`: how long shutdown waits for fronts' requests |
| `--history-service ADDR` | `AGENTFS_HISTORY_SERVICE` | | `mount`, `record`: run as a front of the history service at ADDR |
| `--history-timeout DURATION` | `AGENTFS_HISTORY_TIMEOUT` | `10s` | `mount`, `record` with `--history-service`: how long a boundary waits for the service before `EIO` |
| `--history-wait DURATION` | `AGENTFS_HISTORY_WAIT` | `30s` | `mount`, `record` with `--history-service`: how long to wait for the service at start |
| `--history-slot N` | `AGENTFS_HISTORY_SLOT` | the lowest free slot | `mount`, `record` with `--history-service`: lease pending slot N (1 to 1023) when it is free, else the lowest free slot; a front restarted after a crash passes its predecessor's slot to drain that slot's repairs |
| `--allow-stale` | | | `probe`: exit 0 for a stale agentfs mount too |
| `--remote NAME` (repeatable) | | every remote | `push`: push only these remotes; `lfs fetch`: fetch only from these (in name order) |
| `--timeout DURATION` | | `120s` | `push` through a running daemon: how long to wait for its pushes (clamped to 1s..10m); a push still running then carries on in the background, and the command exits 1 (also when a push stepped under `agentfsMaxPushSize` still has commits left). Pushing directly ignores it and keeps stepping until up to date or a failure |
| `--force-with-lease OID` | | | `push`: replace this volume's own branch on one remote (`--remote NAME`, or the volume's only remote) with the local head, if the remote's branch is OID (a full commit id), OID shares a commit with the head, and the commits it would drop hold no history an agentfs recorder wrote that the head lacks (divergence case B is refused; design §15); nothing else is pushed. Always direct (never through a running daemon), under that remote's push lock only. Exit 0 pushed or up to date; 1 lease broken, unrelated history, recorded history or failure; 75 while another push to that remote runs. For the divergence and history-rewrite runbooks (design §15) |
| `--force` | | | `restore`: write even files open for writing through the mount; `revert`: also revert paths changed again since the commit, and write files open for writing (§15) |
| `--dry-run` | | | `restore`, `revert`: print the plan and write nothing; `lfs fetch`: report the objects (and bytes) missing from the cache, contacting no server; `lfs prune`: report what would be deleted, deleting nothing |
| `--older-than DURATION` | | every object | `lfs prune`: keep objects cached less than DURATION ago |
| `--all` | | the branch head | `lfs fetch`: every object any commit on the branch references, instead of the head's (or the revisions given) |
| `--no-fetch` | | | `patch apply`: apply what is already fetched, without contacting the patch remote |
| `--json` | | | `patch status`: the status document and the local cache as JSON |
| `--remote NAME\|URL` | | `origin` | `patch submit`: the remote the volume pushes to |
| `--repo DIR` | | `.` | `patch submit`: the producer's git repository |
| `--put PATH=FILE` (repeatable) | | | `patch submit`: write PATH with FILE's content, in a patch built on the remote branch head |
| `--delete PATH` (repeatable) | | | `patch submit`: remove PATH, in a built patch |
| `-m`, `--message TEXT` | | empty | `patch submit`: the built patch's commit message |
| `--author 'NAME <EMAIL>'` | | git's user | `patch submit`: the built patch's author |
| `--replace N` | | | `patch submit`: replace queued patch N (it must still be what was listed) |
| `--skip N` | | | `patch submit`: replace queued patch N with an empty patch, so the queue moves on |
| `--depth N` | | full history | `patch submit`: fetch only this many commits (shallow producers) |
| `--attempts N` | | `10` | `patch submit`: inbox numbers to try when other submitters race |
| `--source-state DIR` | | none: required | `fork create`: the source volume's state area, read without its lock; its settings must be in included files, which the fork includes too |
| `--name NAME` | | the source's name | `fork create`: the fork's name before its random suffix (`<name>-<8 hex digits>`) |
| `--link linked\|unlinked` | | `linked` | `fork create`: whether the fork delivers back to its source (`agentfs.fork.link`, fixed at create); `unlinked` makes an independent fork, which `fork finish` refuses and which has no `fork status` |
| `--remotes FILE` | | none: local-only | `fork create`: with `--link unlinked`, the fork's own remotes, a config file of `[remote "<name>"]` sections (`agentfs.fork.remotes`, fixed at create, the file read at every open); refused for a linked fork. Without it the fork pushes nothing, and removing its state area loses its history |
| `--timeout DURATION` | | until published | `fork finish`: stop publishing after this long and exit 1 (the CSI node plugin's finisher runs with `300000ms`, so the fork's lock is free between its scans) |
| `--retry` | | | `fork finish`: clear a refused publish (the `refused` mark in `meta/fork.json`) and try again; without it a refused fork exits 1 at once, pushing nothing |
| `--json` | | | `fork status`: the status document, version 1 (`version`, `fork`, `source`, `base`, `state`, `change`, `finish`, `remote`, `published_at_ms`, `refused`, `abandoned`); `fork list`: the listing (`changes`, `skipped`) |
| `--reason TEXT` | | none: required, not empty | `fork abandon`: why the fork is discarded, kept in `meta/fork.json` with the time, the operator's uid and the state it was in |
| `--remote NAME\|URL` | | `origin` | `fork list`: the fork remote, as the receiver's repository names it |
| `--source VOLUME` | | every source | `fork list`: only the changes of forks of this source volume |
| `--repo DIR` | | `.` | `fork list`: the receiver's git repository, whose `git` and credentials are used; no state area is read |
| `--from NAME\|URL` | | `origin` | `fork deliver`: the fork remote the published change is read from (checked as `fork list` checks it), as the receiver's repository names it; the payload's `fetch.remote` carries it as given |
| `--to inbox\|branch\|command\|webhook` | | none: required | `fork deliver`: the delivery method (design §11.5 "Delivery"); a flag of another method is refused |
| `--volume VOLUME` | | none: required by `inbox` | `fork deliver`: inbox, the volume whose inbox the change is queued in (the runner's choice, never the change's `Agentfs-Source`) |
| `--remote NAME\|URL` | | `--from` | `fork deliver`: inbox and branch, where to push (the volume's patch remote, or the repository the pull request is opened in) |
| `--prefix PREFIX` | | `agentfs-review/` | `fork deliver`: branch, the branch is `<prefix><fork>`, created only if absent |
| `--shape squash\|history` | | `squash` | `fork deliver`: branch, one commit with the change's tree on base (author and committer of the change, message `change of fork <fork>` with `Agentfs-Change`), or the fork's history up to its finish commit |
| `--onto BRANCH` | | base | `fork deliver`: branch, squash only, the squash commit sits on BRANCH's head after a three-way merge against base; a conflict is refused with its paths (git 2.40 or later) |
| `--url URL` | | none: required by `webhook` | `fork deliver`: webhook, the `http://` or `https://` URL (`https` with the `https` feature) the payload is POSTed to; redirects are never followed, and no proxy from the environment is used |
| `--credential-file FILE` | | none | `fork deliver`: webhook, an absolute path whose contents (trimmed) are sent as `Authorization: Bearer`, read at each delivery |
| `--timeout DURATION` | | `60s` | `fork deliver`: command and webhook, how long the receiver may take; a program still running is killed with its process group and counts unavailable (exit 75) |
| `--repo DIR` | | `.` | `fork deliver`: the receiver's git repository, whose `git` and credentials are used; no state area is read |
| `--tip OID` | | none: required | `start accept`: a full commit id of the branch the volume started from (the one `status` names on its `start branch moved:` line), saved as `refs/agentfs/<volume>/accepted/<OID>` after fetching it from the start remote; the next push pushes it and no longer alarms for the commits it reaches. Takes no lock; exit 0 accepted or already, 1 refused (not a full id, not on the start branch, not a started volume) |

`fork deliver ... --to command -- PROGRAM [ARGS...]` runs PROGRAM
without a shell, with the payload (design §11.5, schema 1) as JSON on
stdin and an environment of `PATH` alone; its stdout goes to `fork
deliver`'s stderr. Exit 0 accepts, 75 means try again, anything else
refuses. `fork deliver` exits 0 delivered or already delivered, 1 refused
(or not published), 75 unavailable: run it again.

The [operator guide](operator-guide.md) §4 describes the mount flags in
full, and §16 the history service and fronts; the
[patch queue runbook](runbooks/patch-queue.md) the patch commands.

### `agentfs init`

`init` writes `agentfs.volume` (and `agentfs.branch` with `--branch`) and
creates the state area. It is safe to run at every start, as a
Kubernetes init container does:

- **New state area** (no `<state>/git/config`, or one without
  `agentfs.volume`): created, printing `initialised volume V in S`. Rule
  files already in `<state>/git/info/` are kept.
- **Same volume** (and the same branch, if `--branch` is given): exit 0,
  printing `volume V already initialised in S`. It takes the state-area
  lock (exit 75 while a mount holds it, or wait with `--wait-lock`),
  re-creates missing directories, and with `--include` makes those files
  exactly the config's `include.path` entries. Nothing else changes.
- **Another volume or branch**: exit 1, nothing changed. A volume's name
  and branch cannot be changed by `init`.
- `--root DIR` (or `AGENTFS_ROOT`) also creates the backing tree directory
  when it is missing.
- The result is opened once before `init` exits, so a bad setting or
  include fails `init`, not the first mount.
- **No remote is fine**: a result with no push section is a local-only
  volume ([Remotes modes](#remotes-modes)); a remote added later is
  pushed from its first push.

## Environment variables

Every variable agentfs reads. The `AGENTFS_*` variables are the flags'
equivalents above; a flag given on the command line wins.

| Variable | Flag | Read by |
|---|---|---|
| `AGENTFS_STATE` | `--state` | all commands |
| `AGENTFS_ROOT` | `--root` | all commands |
| `AGENTFS_WAIT_LOCK` | `--wait-lock` | all commands |
| `AGENTFS_NAMESPACE`, `AGENTFS_POD`, `AGENTFS_POD_UID`, `AGENTFS_NODE`, `AGENTFS_REPLICA` | `--namespace`, `--pod`, `--pod-uid`, `--node`, `--replica` | the commands with pod flags |
| `AGENTFS_VOLUME` | `--volume` | `init`, `patch submit` |
| `AGENTFS_PASSTHROUGH` | `--passthrough` | `mount` |
| `AGENTFS_THREADS` | `--threads` | `mount` |
| `AGENTFS_ON_STALE_MOUNT` | `--on-stale-mount` | `mount` |
| `AGENTFS_SEAL_MOUNTPOINT` | `--seal-mountpoint` | `mount` |
| `AGENTFS_WORKLOAD_MOUNTS` | `--workload-mount` | `mount` |
| `AGENTFS_HISTORY_LISTEN` | `--listen` | `serve-history` |
| `AGENTFS_HISTORY_SERVICE`, `AGENTFS_HISTORY_TIMEOUT`, `AGENTFS_HISTORY_WAIT`, `AGENTFS_HISTORY_SLOT` | `--history-service`, `--history-timeout`, `--history-wait`, `--history-slot` | `mount`, `record` |
| `CSI_ENDPOINT`, `NODE_NAME`, `AGENTFS_CSI_POD`, `AGENTFS_CSI_NAMESPACE`, `AGENTFS_CSI_RELEASE_SCAN_INTERVAL` | `--endpoint`, `--node-id`, `--pod`, `--namespace`, `--release-scan-interval` | `agentfs-csi` ([below](#the-csi-driver-agentfs-csi)) |

**Test builds only.** `AGENTFS_FAILPOINTS` sets failpoints on the durable
path (`name=kill|error|panic|sleep(ms)[@n]`, joined with `;`; design §18
test 3, [architecture.md](architecture.md#crash-model)). Only a binary
built with `--features agentfs/failpoints` reads it (its `--version` ends
in `+failpoints`, and it warns at start when the variable is set); release
builds and the image have no failpoints and ignore it. Never set it in
production.

**Telemetry** (design §13). Every command with pod flags but `status` (`restore` and `revert` only
without a mount) exports OpenTelemetry metrics, traces
and logs over OTLP when an endpoint is set; with none, telemetry is off
and costs nothing. agentfs itself reads these to decide what to export:

| Variable | Meaning |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Enables every signal; over HTTP at `/v1/traces`, `/v1/metrics` and `/v1/logs` under it |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`, `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | A full URL: enables only that signal, and wins over the general endpoint |
| `OTEL_EXPORTER_OTLP_PROTOCOL`, `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL`, `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL`, `OTEL_EXPORTER_OTLP_LOGS_PROTOCOL` | `http/protobuf` (the default), `http/json` or `grpc`; the per-signal form wins. Another value turns that signal off with a `warning: telemetry:` line. With `grpc` the endpoint is the collector's gRPC address (`http://otel-collector:4317`), with no `/v1/...` path |
| `OTEL_TRACES_EXPORTER`, `OTEL_METRICS_EXPORTER`, `OTEL_LOGS_EXPORTER` | `none` turns that signal off; only `otlp` is built |
| `OTEL_SDK_DISABLED` | `true` turns everything off |
| `OTEL_EXPORTER_OTLP_CERTIFICATE`, `OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE`, `OTEL_EXPORTER_OTLP_METRICS_CERTIFICATE`, `OTEL_EXPORTER_OTLP_LOGS_CERTIFICATE` | For an `https://` endpoint: a PEM file of CA certificates, the only ones trusted for the collector. Unset: the system store (`SSL_CERT_FILE`, `SSL_CERT_DIR` move it) |
| `OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE`, `OTEL_EXPORTER_OTLP_TRACES_CLIENT_CERTIFICATE`, `OTEL_EXPORTER_OTLP_METRICS_CLIENT_CERTIFICATE`, `OTEL_EXPORTER_OTLP_LOGS_CLIENT_CERTIFICATE` | For an `https://` endpoint: a PEM client certificate (chain) for mutual TLS. Needs the key; one without the other turns that signal off with a warning |
| `OTEL_EXPORTER_OTLP_CLIENT_KEY`, `OTEL_EXPORTER_OTLP_TRACES_CLIENT_KEY`, `OTEL_EXPORTER_OTLP_METRICS_CLIENT_KEY`, `OTEL_EXPORTER_OTLP_LOGS_CLIENT_KEY` | The client certificate's PEM private key (PKCS#8, PKCS#1 or SEC1) |
| `OTEL_EXPORTER_OTLP_TIMEOUT`, `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT`, `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT`, `OTEL_EXPORTER_OTLP_LOGS_TIMEOUT` | Milliseconds an export may take (default 10000). agentfs applies it to its own HTTPS client; the plain HTTP and gRPC exporters read it themselves. Not a number (`10s`, say): that signal is off, with a warning, whatever the endpoint |

An `https://` endpoint needs a binary built with the `otlp-tls` cargo
feature (the image is; rustls on ring, no OpenSSL); without it the
endpoint is reported the same way. Every certificate and key file is read
once, at start: a file that cannot be read or parsed turns its signal off
with a `warning: telemetry:` line naming it, and a rotated certificate
takes a restart. A collector that refuses the handshake or the token
shows as failed exports (below). For a bearer token, set
`OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<token>` (sent as a
header over HTTP and as metadata over gRPC). Telemetry problems never stop
a command.

The OpenTelemetry SDK and exporter read the rest themselves:
`OTEL_SERVICE_NAME` (default `agentfs`), `OTEL_RESOURCE_ATTRIBUTES`,
`OTEL_EXPORTER_OTLP_HEADERS` (and its per-signal forms; values are
percent-decoded), `OTEL_METRIC_EXPORT_INTERVAL` (default 60s), the
batch settings `OTEL_BSP_*` and `OTEL_BLRP_*`, and sampling:
`OTEL_TRACES_SAMPLER` and `OTEL_TRACES_SAMPLER_ARG` (every record makes an
`agentfs.record` span, so on a busy volume use
`parentbased_traceidratio` with `0.01`). [telemetry.md](telemetry.md) has
the resource attributes, and design §13 the metrics and spans.

## The CSI driver (`agentfs-csi`)

On Kubernetes with the CSI driver ([csi.md](csi.md)), a volume's settings,
remotes and rules come from a **profile** in the driver's
`agentfs-csi-profiles` ConfigMap: `<profile>.config` is a fragment with
the keys of this page, included by the volume's state area like
`init --include` does; `<profile>.exclude` and `<profile>.attributes`
become its `info/exclude` and `info/attributes`. Credentials files are
under `/etc/agentfs-csi/credentials/` in the driver's pods; with the
chart's git server on (the `server.*` values: `enabled`, `image`,
`storage`, `credentials.existingSecret`, `service.port` and the pod's
scheduling, listed in the [chart's README](../charts/agentfs-csi/README.md)),
`agentfs-server` there is its credentials file, for a profile's
`agentfsCredentials` ([helm.md](helm.md#the-git-server)). Each claim's
worktree (its files and its branch `agentfs/<volumeName>`) lives on the
disk of the node that created it, for good.

**StorageClass parameters** (or a static PV's `volumeAttributes`, where unknown keys are refused too):

| Parameter | Default | Meaning |
|---|---|---|
| `profile` | required | the profile, a DNS label |
| `volumeName` | `{pvcNamespace}.{pvcName}.{pvName}`, else the PV name | the agentfs volume name; a template over `{pvcNamespace}`, `{pvcName}`, `{pvName}` that must contain `{pvName}` (`INVALID_ARGUMENT` otherwise): each claim is a worktree with its own branch, `agentfs/<volumeName>`, and a claim deleted and created again is a new one. There is no `branch` parameter. In a static PV's attributes, a literal name (`volume` is the same key), default the volume handle |
| `onDelete` | `retain` | what DeleteVolume does once nothing would be lost: `retain` keeps the worktree on its node (`csi.retained.json`), `delete` removes it. Either way it waits (`UNAVAILABLE`, retried) while a remote lacks the branch's head, and a worktree with no remote section (local-only) is kept whatever this says, except an unlinked fork with no remote of its own, which `delete` removes unpushed |
| `startFrom` | none | `<remote>:<branch>@<commit>`: a start class. Each claim on it is a new worktree whose history begins at `<commit>` of another worktree's `<branch>`, fetched once from `<remote>` (a push section of the profile) at CreateVolume, and kept in its identity (`[agentfs "start"]`). `<branch>` is a branch name such as `agentfs/<old volume>`, never a `refs/...` ref (a `diverged/*` ref is refused: "not a branch"), never the new volume's own branch; `<commit>` is a full id on it. Moving a volume to another node: [runbooks/moving.md](runbooks/moving.md). Not a static PV attribute |
| `workloadMounts` | none | where pods mount the volume: comma-separated `PATH` or `PATH=DIR`, passed to every front as `--workload-mount` so worktree `gitdir:` paths map exactly (design §5, docs/csi.md §5) |
| `fork` | `"false"` | `"true"` makes a fork class (design §11.5): a claim on it with a `dataSource` claim is a fork of that claim's volume, with the source's profile, made on the source's node (give the fork's pod affinity to its source's pods; elsewhere CreateVolume answers `FAILED_PRECONDITION`). A fork class must not set `profile`, `branch` or `startFrom`, its `onDelete` defaults to `delete`. Not a static PV attribute |
| `link` | `linked` | a fork class only (`INVALID_ARGUMENT` in any other): `linked` forks deliver their change back to the source when their claim is deleted; `unlinked` forks are independent, never finished, never push to the source's remotes (only to their `remotes`), and DeleteVolume applies `onDelete` once their own remotes hold the head, as for any worktree; with no remote of their own they are local-only and `onDelete: delete` removes them unpushed. Written into each fork's identity (`agentfs.fork.link`) at CreateVolume; changing the class moves no fork that exists. Not a static PV attribute |
| `remotes` | none: local-only | a fork class with `link: unlinked` only (`INVALID_ARGUMENT` in any other): a profile name whose `<remotes>.config` holds the fork's own remotes ([`agentfs.fork.remotes`](#agentfs-fork)), written into each fork's identity at CreateVolume; the config must exist. Not a static PV attribute |

Every class of the driver must have `reclaimPolicy: Delete`, so that
DeleteVolume always runs and the release decider answers; the chart's
admission policy (`classAdmission.enabled`, [helm.md](helm.md)) refuses
any other.

**`agentfs-csi` flags:**

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--endpoint ADDR` | `CSI_ENDPOINT` | `unix:///csi/csi.sock` | the CSI socket (`unix://`, `unix:` or an absolute path) |
| `--node-id NAME` | `NODE_NAME` | required | this node's name (`spec.nodeName`), its topology (`topology.agentfs.mcurcio.github.io/node`) |
| `--base-path DIR` | | `/var/lib/agentfs-csi/volumes` | where this node's worktrees live: a directory on the node's own disk, ideally a disk of its own (a full disk makes the kubelet evict every pod on the node). A network filesystem (NFS, SMB, CephFS, GlusterFS, Lustre, GPFS, AFS) is refused before anything is served, naming its type |
| `--node-state DIR` | | `/var/lib/agentfs-csi/node` | the node's staged and published records and the services' sockets; must survive plugin restarts |
| `--mount-dir DIR` | | `/run/agentfs-csi/mounts` | where fronts mount before the bind onto the kubelet's target |
| `--profiles-dir DIR` | | `/etc/agentfs-csi/profiles` | the profiles |
| `--agentfs-bin PATH` | | `/usr/local/bin/agentfs` | the `agentfs` binary the node plugin runs |
| `--passthrough auto\|on\|off` | | `auto` | the fronts' FUSE passthrough |
| `--front-threads N` | | `4` | FUSE threads per front |
| `--stage-wait DURATION` | | `60s` | how long NodeStage waits for the history service before answering `UNAVAILABLE` |
| `--publish-wait DURATION` | | `60s` | how long NodePublish waits for the front's mount |
| `--unpublish-grace DURATION` | | `20s` | how long NodeUnpublish waits for a front to stop before SIGKILL |
| `--unstage-grace DURATION` | | `30s` | how long NodeUnstage waits for a service to stop before SIGKILL |
| `--history-wait DURATION` | | `30s` | the fronts' `--history-wait` |
| `--backoff-max DURATION` | | `30s` | the longest delay between restarts of a crashed service or front |
| `--pod NAME`, `--namespace NAME` | `AGENTFS_CSI_POD`, `AGENTFS_CSI_NAMESPACE` | empty | the plugin pod's name and namespace, passed to the history services (their own identity in telemetry) |
| `--release-scan-interval DURATION` | `AGENTFS_CSI_RELEASE_SCAN_INTERVAL` | `30s` | how often the release scan runs `agentfs push` for every worktree no pod stages on this node, until a push exits 0 at its head, and `agentfs fork finish` for every fork whose deletion asked for it (design §14.2, §11.5); also the first delay after a failed push, doubling up to `--backoff-max` (at least this). It runs once at start and whenever DeleteVolume waits |

The node plugin passes its `OTEL_*` variables on to every service and
front; it removes inherited `AGENTFS_*` variables, so the per-volume and
per-pod flags it sets are the only ones.
