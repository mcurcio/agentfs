# Configuration reference

Every setting agentfs reads, what it accepts, and what happens when it is
wrong. The design is in [design.md](design.md) (§4.3 settings, §9
identities, §11 remotes); how to run a mount is in the
[operator guide](operator-guide.md).

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

Set them with `git config`:

```sh
git --git-dir /data/state/git config agentfs.pushInterval 30s
```

`mount` and `run` read the settings when they start; changing a key takes a
restart. `push`, `record`, `flush` and `status` read them at every run. The
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
| `agentfs.lfsThreshold` | positive size | unset: no size rule | Content larger than this (strictly `>`) is stored as Git LFS instead of a git blob (§7). Unset, only `agentfs.store=lfs` and `filter=lfs` in `info/attributes` select LFS. Setting it needs an LFS server for every remote (see [Git LFS](#git-lfs)) |

Operator path rules are not config keys: they are `<state>/git/info/exclude`
(git ignore syntax: paths never recorded when new) and
`<state>/git/info/attributes` (`-agentfs` and the other `agentfs.*`
attributes, §4.2).

### `[agentfs "identity"]`

Templates for commit identities (design §9). Variables: `{volume}`,
`{namespace}`, `{pod}`, `{podUid}`, `{node}`, `{replica}`, and in author
templates also `{uid}` and `{gid}`. They come from the pod flags
(`--namespace`/`AGENTFS_NAMESPACE`, `--pod`/`AGENTFS_POD`,
`--pod-uid`/`AGENTFS_POD_UID`, `--node`/`AGENTFS_NODE`,
`--replica`/`AGENTFS_REPLICA`). An unknown variable, an unclosed `{`, or a
rendered identity that is empty or contains `<`, `>` or a newline is an
error.

| Key | Default |
|---|---|
| `agentfs.identity.committerName` | `{replica}` |
| `agentfs.identity.committerEmail` | `{podUid}@{volume}.agentfs.invalid` |
| `agentfs.identity.authorName` | `uid-{uid}` |
| `agentfs.identity.authorEmail` | `uid-{uid}@{volume}.agentfs.invalid` |

## `[lfs]`

git-lfs's own global keys, read from the same config file (never from a
tracked `.lfsconfig`, which is content only). They apply to every remote
and win over the per-remote keys, as in git-lfs; see [Git LFS](#git-lfs).

| Key | Type | Default | Meaning and validation |
|---|---|---|---|
| `lfs.url` | `http(s)://` URL, `file://` URL or path | unset | The LFS endpoint for every remote |
| `lfs.pushurl` | as `lfs.url` | unset | Where LFS uploads go for every remote |

A bad value does not stop the volume: every remote is reported as
misconfigured instead.

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
| `agentfsCredentials` | absolute path | none | HTTP and HTTPS only: a file holding a token or `username=`/`password=` lines (see [HTTPS](#https-and-http-remotes)) |
| `agentfsSshKey` | absolute path | none | SSH only, and required there: the private key (see [SSH](#ssh-remotes)) |
| `agentfsKnownHosts` | absolute path | none | SSH only, and required there: the known_hosts file the server's host key is checked against |
| `lfsurl` | `http(s)://` URL, `file://` URL or path | derived from the remote's URL | The Git LFS endpoint for this remote, as in git-lfs, unless `lfs.url` is set (see [Git LFS](#git-lfs)) |
| `lfspushurl` | as `lfsurl` | `lfsurl` | Where LFS uploads go, unless `lfs.pushurl` or `lfs.url` is set |
| `agentfsLfsCredentials` | absolute path | none | The credential file (same format as `agentfsCredentials`) for an LFS endpoint on another host or port than the push URL; never sent to any other host |

The remote's name must be a valid agentfs name (as for `agentfs.volume`;
git accepts `team/backup`, agentfs does not), because it names the push
state file and appears in metrics.

**A remote is misconfigured** (left out, `RemoteMisconfigured`) when:

| Condition | Message names |
|---|---|
| the name is not a valid agentfs name | `remote.<name>` |
| `agentfsMaxBlobSize` is not a positive size | `remote.<name>.agentfsMaxBlobSize` |
| `agentfsCredentials`, `agentfsSshKey`, `agentfsKnownHosts` or `agentfsLfsCredentials` is not an absolute path | the key |
| the push URL is SSH and `agentfsSshKey` or `agentfsKnownHosts` is not set | the missing key |
| the push URL is SSH and `agentfsCredentials` is set | `agentfsCredentials` |
| the push URL is SSH and carries a password (`ssh://user:pw@host/…`) | `url` or `pushurl` |
| the push URL is SSH and has no host or a non-numeric port | `url` or `pushurl` |
| the push URL is not SSH and `agentfsSshKey` or `agentfsKnownHosts` is set | the key |
| the push URL is SSH and the binary was built without the `ssh` feature | `url` or `pushurl` |
| `lfsurl`, `lfspushurl`, or the global `lfs.url` or `lfs.pushurl`, is not an `http(s)://` URL, a `file://` URL or a path (SSH and `git://` are refused) | the key; a bad global key misconfigures every remote |
| one of those is `https://` and the binary was built without the `https` feature | the key |

Problems with the files themselves (missing, unreadable, malformed) are
not misconfigurations: they fail each push attempt, with the usual alarm
and backoff, so a Secret mounted late or being rotated starts working as
soon as it is right. A missing or bad credential or key fails as `auth`; a
missing or unreadable known_hosts file as `untrusted_host`.

`agentfs mount` additionally refuses to start (an error, not an alarm)
when any remote section, valid or not, names a credentials, key or
known_hosts file under the backing tree or the mount point: the workload
could read the secret, or rewrite known_hosts to trust any host.

### Transports

The transport is chosen from the push URL, as libgit2 does:

| Push URL | Transport | Build |
|---|---|---|
| a path, `file://…` | local | default |
| `http://…` | smart HTTP | default |
| `https://…` | smart HTTP over TLS | `--features https` (OpenSSL) |
| `ssh://…`, `ssh+git://…`, `git+ssh://…` (any case), scp-like `[user@]host:path` | SSH | `--features ssh` (libssh2, OpenSSL) |
| `git://…`, anything else | not supported for pushing | |

scp-like syntax needs a `:` before any `/`, as in git; `[user@host:port]:path`
names a port. A local path with a `:` after a `/` (`/srv/a:b.git`) is
treated as local, but libgit2 built with `ssh` sends it to its SSH
transport, so with that feature its push fails (closed: nothing is
offered and no host is trusted). Avoid `:` in local remote paths.

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
- **The key** is an **unencrypted** private key, OpenSSH format
  (`BEGIN OPENSSH PRIVATE KEY`, what `ssh-keygen` writes by default) or
  PEM (`RSA`, `EC`, PKCS#8 `PRIVATE KEY`). No `.pub` file is needed.
  Encrypted keys are refused (agentfs has no passphrase or ssh-agent
  support); remove a passphrase with `ssh-keygen -p -N '' -f <file>`.
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

  **List every key type the server has**, as `ssh-keyscan` does by
  default: libssh2 chooses the type (ECDSA, then Ed25519, then RSA)
  before agentfs sees it, so a file with only the Ed25519 key of a server
  that also has an ECDSA key is refused with "lists only ssh-ed25519".
- **What is checked:** before any authentication, the server's host key
  is looked up for the push URL's host (`[host]:port` on a port other
  than 22). Plain and hashed names, comma lists, `*` and `?` wildcards,
  `!` negation and `@revoked` lines are honoured; `@cert-authority` lines
  (host certificates) are not supported. An unknown host, a changed key,
  a key type the file does not list for the host, or a revoked key fails
  the push as `untrusted_host`, and the key is never offered to that
  host. The message names the file, the host as looked up, the key type,
  its `SHA256:` fingerprint and the `ssh-keyscan` command to list the
  server's keys. Nothing is ever learned or written back; a changed key
  may be an attack, so confirm it before updating the file.
- **Rotation:** replace either file atomically (write a new file and
  rename it over the old one, or update the Secret); the next push uses
  it, without a restart. Like credentials, a key is given only to the
  push URL's host and port, once per connection; a rejected key fails
  as `auth`.

**In Kubernetes,** keep both files in a Secret mounted outside the backing
tree. The `kubernetes.io/ssh-auth` type uses the `ssh-privatekey` key:

```sh
kubectl create secret generic agentfs-origin-ssh \
  --type=kubernetes.io/ssh-auth \
  --from-file=ssh-privatekey=./ssh-privatekey \
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

## Flags and environment

The settings above are per volume, in its config. The commands also take
flags, most with an environment equivalent:

| Flag | Environment | Default | Commands |
|---|---|---|---|
| `--state DIR` | `AGENTFS_STATE` | `.` | all |
| `--root DIR` | `AGENTFS_ROOT` | | `record`, `flush`, `run`; `restore` and `revert` without a mount; must match the backing tree for `mount`; `init` creates it (mode 0755) when missing |
| `--wait-lock DURATION` | `AGENTFS_WAIT_LOCK` | `0s` | all that take the state-area lock |
| `--namespace`, `--pod`, `--pod-uid`, `--node`, `--replica` | `AGENTFS_NAMESPACE`, `AGENTFS_POD`, `AGENTFS_POD_UID`, `AGENTFS_NODE`, `AGENTFS_REPLICA` | empty; `--replica` defaults to the pod name, else the host name | `mount`, `run`, `record`, `flush`, `push` |
| `--passthrough auto\|on\|off` | `AGENTFS_PASSTHROUGH` | `auto` | `mount` |
| `--threads N` | `AGENTFS_THREADS` | CPUs, 2 to 16 | `mount` |
| `--no-allow-other` | | | `mount` |
| `--unmount-grace DURATION` | | `10s` | `mount` |
| `--push-grace DURATION` | | `5s` | `mount`, `run` |
| `--on-stale-mount refuse\|hold` | `AGENTFS_ON_STALE_MOUNT` | `refuse` | `mount` |
| `--allow-stale` | | | `probe` |
| `--remote NAME` (repeatable) | | every remote | `push` |
| `--volume NAME` | `AGENTFS_VOLUME` | none: required | `init` |
| `--branch NAME` | | `agentfs/<volume>` | `init` |
| `--include FILE` (repeatable) | | keep the existing includes | `init`: exactly these `include.path` entries |

Telemetry uses the standard `OTEL_*` variables; see the
[README](../README.md#telemetry). The [operator guide](operator-guide.md)
§4 describes the mount flags in full.

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
