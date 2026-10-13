# Runbook: pushes that keep failing

A remote stopped accepting the volume's history. Recording and
committing are unaffected: commits stay safe in the state area and go out
with the next successful push. Design:
[§11](../design.md#11-remotes-and-pushing), alarms and metrics:
[§13](../design.md#13-observability-opentelemetry).

## Symptoms

- `PushFailed` alarms (stderr, and log event `agentfs.push_failed`) with
  the remote, a **kind**, the head, the last pushed commit, the number of
  failures in a row and the next retry.
- `agentfs.push.oldest_unpushed_age{remote}` (seconds since the oldest
  unpushed commit) keeps growing: the main alarm to page on.
  `agentfs.push.unpushed_commits` grows with it, and
  `agentfs.push.failures{remote,reason,stage}` counts attempts (`stage` is
  `lfs` for [LFS upload failures](#lfs-stage-failures), else `git`).
- `agentfs --state "$STATE" status` (safe while mounted):

  ```
  remote:    origin https://github.com/acme/agent-history.git: last pushed 4f0c2e1..., 37 unpushed, 5 consecutive failure(s), last error (auth): ...
  ```

- `RemoteMisconfigured` (gauge `agentfs.remote.misconfigured`): that
  remote is not pushed at all (below).

## Diagnose by kind

| Kind | What it means | Check |
|---|---|---|
| `unreachable` | No answer: DNS, connection refused, a TLS handshake that fails before the certificate is checked, or no reply within the timeouts (30 s to connect, 5 min for a stalled transfer) | From the same pod or host: `git ls-remote <url>`. Proxies and network policies. For `https://` the binary must be built with the `https` feature |
| `untrusted_host` | The remote host is not trusted. SSH: its host key is not in the remote's `agentfsKnownHosts` for that host and port (unknown host), differs from the one listed (changed key), is of a type the file does not list, or is marked `@revoked`; or the file is missing or unreadable. HTTPS: libgit2 refused its TLS certificate (a host name it does not cover, an untrusted, self-signed or expired certificate). The key or credential was **not** sent | The message names the file, the host as looked up (`[host]:port` on a port other than 22), the key type and its `SHA256:` fingerprint. Compare with the provider's published fingerprints or `ssh-keyscan -p <port> <host> \| ssh-keygen -lf -`. **A changed key may be an attack:** confirm the new fingerprint out of band before replacing known_hosts. "Lists only ..." means known_hosts lacks the key type the server offered: add every type `ssh-keyscan` prints. For HTTPS: the system CA certificates, and that the URL's host is the certificate's |
| `auth` | The remote refused the credential or key, or none could be read | HTTP(S): the file named by `remote.<name>.agentfsCredentials`: `username=` and `password=` lines, or a bare token (used as the password, with the URL's username or `x-access-token`). Token scopes: it must be allowed to push to the repository (GitHub: contents write). SSH: the file named by `remote.<name>.agentfsSshKey` must be an unencrypted private key (the message says if it is encrypted, a `.pub` file, or unreadable) whose public half is authorized for the URL's user (else `git`) with write access (GitHub: a deploy key with write access). Either file is re-read when it changes, so a rotated Secret is picked up without a restart. A credential or key is only sent to the host (and port) of the push URL: check `pushurl` and `url` agree with where it is valid |
| `refused` | Connected and authenticated, but the push was rejected | The message says why: branch protection or rules on `agentfs/*`, a server-side hook, `receive.fsckObjects` (agentfs checks git's fsck rules before committing, §3.4, so this points at another policy), a file size limit lower than `remote.<name>.agentfsMaxBlobSize`, a quota |
| `non_fast_forward` | Someone else moved the branch | [divergence.md](divergence.md) |
| `local` | A problem on this side: the per-remote lock, reading the branch, or saving `meta/push/<remote>.json` | Disk space and permissions of the state area |

**`RemoteMisconfigured`**: the `[remote "<name>"]` section is invalid, so
it is left out: a name that is not a valid agentfs name, a relative
`agentfsCredentials`, `agentfsSshKey`, `agentfsKnownHosts` or
`agentfsLfsCredentials`, an LFS endpoint (`lfsurl`, `lfspushurl`,
`lfs.url`, `lfs.pushurl`) that is not `http(s)`, `file://` or a path, an
`agentfsMaxBlobSize` that is not a size; for an SSH push URL, a missing
`agentfsSshKey` or `agentfsKnownHosts`, an `agentfsCredentials`, a
password in the URL, or a binary built without the `ssh` feature; for
any other push URL, an `agentfsSshKey` or `agentfsKnownHosts`. The alarm
names the key; the full list is in the
[configuration reference](../configuration.md#remote-name). Fix the
section in `<state>/git/config`; the change takes effect at the next
start of the mount (or the next `agentfs push`).

## LFS stage failures

A failure with **stage `lfs`** (`failed (auth, stage lfs)` in
`agentfs push` and the alarm, `stage="lfs"` on
`agentfs.push.failures`, `agentfs.push.stage` on the log record, `last
error (…, stage lfs)` in `status`) happened while uploading the Git LFS
objects the unpushed commits reference (design §7). **No ref was pushed**:
the git push runs only after every upload succeeded. First find the
endpoint and where its credentials come from:

```sh
agentfs --state "$STATE" status
# remote:    sep https://git.example.com/acme/r.git: …, last error (auth, stage lfs): …
#            lfs: https://lfs.example.com/acme/r (remote.sep.lfsurl, credentials from agentfsLfsCredentials)
```

| Kind | Check |
|---|---|
| `auth` | The credentials named on the `lfs:` line. `agentfsLfsCredentials` is used wherever it is set; without it, `agentfsCredentials` only for an endpoint with the push URL's scheme, host and port; else a password in the URL; else none ("no credentials are configured for it"). A separate LFS host never gets the git credentials: give it `remote.<name>.agentfsLfsCredentials` (same format). The token needs LFS write access (GitHub: contents write). The file is re-read at the next attempt |
| `local` | `lfs: none (…)` explains a missing endpoint: an SSH or `git://` remote needs `remote.<name>.lfsurl` (agentfs has no `git-lfs-authenticate`). "built without the `https` cargo feature": use an image built with `--features https`. "missing from the local cache": an object the commits reference is not in `<state>/lfs/objects` (state-area damage, or the directory was deleted): put it back (`git lfs fetch` in a clone that has it, then copy `.git/lfs/objects/aa/bb/<oid>` to `<state>/lfs/objects/aa/bb/<oid>`); there is no `agentfs lfs fetch` yet |
| `unreachable` | The LFS server: DNS, connection, a 5xx or 429, or a transfer slower than 256 KiB/s past its 5-minute base timeout. `curl -sI <endpoint>/objects/batch` from the same pod |
| `untrusted_host` | The LFS server's TLS certificate failed verification against the system trust store (recognised from OpenSSL's message; an unrecognised one shows as `unreachable`) |
| `refused` | Any other answer: the message has the HTTP status or the per-object error (`422` for an object over the server's size limit, `404` for a wrong endpoint, a redirect, which agentfs never follows: use the final URL as `lfsurl`) |

Remotes whose unpushed commits reference no LFS object never contact an
LFS endpoint, so one remote's LFS problem never blocks another remote.

## Backoff

After a failure the next attempt waits `pushInterval` doubled for each
failure in a row after the first, up to `max(pushInterval, 15m)`. With
the default `pushInterval = 60s`: 1, 2, 4, 8, then 15 minutes between
attempts. A success resets it. The alarm's `next retry` says when.

## Retry now

- **No mount or `run` is running**: `agentfs --state "$STATE" push`
  pushes every remote once (or `--remote <name>`), prints the outcome and
  exits 1 unless all succeeded.
- **A mount is running**: `agentfs push` exits 75 (the mount holds the
  state area). Wait for the next attempt, or restart the mount (the first
  push of each remote happens at start). Asking a running daemon to push
  now is a follow-up.

After fixing a credentials, key or known_hosts file alone, nothing is
needed: they are re-read at the next attempt.

## At shutdown

Stopping never waits long for a remote: after the final commit, shutdown
gives each remote that lacks commits one more push, all within
`--push-grace` (5 s by default), and then leaves them (`final push:
origin: still running at the push grace's end; abandoned`). A failed
final push prints its reason (`final push: origin: failed (...)`), and
`--push-grace 0s` skips it (`final push: origin: skipped, ...`). Nothing
is lost; the commits are pushed by the next start.
