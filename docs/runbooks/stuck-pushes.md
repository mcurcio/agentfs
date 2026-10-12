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
  `agentfs.push.failures{remote,reason}` counts attempts.
- `agentfs --state "$STATE" status` (safe while mounted):

  ```
  remote:    origin https://github.com/acme/agent-history.git: last pushed 4f0c2e1..., 37 unpushed, 5 consecutive failure(s), last error (auth): ...
  ```

- `RemoteMisconfigured` (gauge `agentfs.remote.misconfigured`): that
  remote is not pushed at all (below).

## Diagnose by kind

| Kind | What it means | Check |
|---|---|---|
| `unreachable` | No answer: DNS, connection refused, TLS failure (a certificate that does not match or is not trusted), or no reply within the timeouts (30 s to connect, 5 min for a stalled transfer) | From the same pod or host: `git ls-remote <url>`. Proxies and network policies. For `https://` the binary must be built with the `https` feature; system CA certificates must be present |
| `auth` | The remote refused the credential | The file named by `remote.<name>.agentfsCredentials`: `username=` and `password=` lines, or a bare token (used as the password, with the URL's username or `x-access-token`). It is re-read when it changes, so a rotated Secret is picked up without a restart. A credential is only sent to the host (and port) of the push URL: check `pushurl` and `url` agree with where the token is valid. Token scopes: it must be allowed to push to the repository (GitHub: contents write) |
| `refused` | Connected and authenticated, but the push was rejected | The message says why: branch protection or rules on `agentfs/*`, a server-side hook, `receive.fsckObjects` (agentfs checks git's fsck rules before committing, §3.4, so this points at another policy), a file size limit lower than `remote.<name>.agentfsMaxBlobSize`, a quota |
| `non_fast_forward` | Someone else moved the branch | [divergence.md](divergence.md) |
| `local` | A problem on this side: the per-remote lock, reading the branch, or saving `meta/push/<remote>.json` | Disk space and permissions of the state area |

**`RemoteMisconfigured`**: the `[remote "<name>"]` section is invalid (a
name that is not a valid agentfs name, a relative `agentfsCredentials`,
an `agentfsMaxBlobSize` that is not a size), so it is left out. Fix the
section in `<state>/git/config`; the change takes effect at the next
start of the mount (or the next `agentfs push`).

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

After fixing credentials alone, nothing is needed: they are re-read at
the next attempt.

## At shutdown

Stopping never waits long for a remote: shutdown waits up to
`--push-grace` (5 s by default) for pushes in progress and then leaves
them (`origin: push still running at shutdown; abandoned`). Nothing is
lost; the commits are pushed by the next start.
