# Security policy

## Reporting a vulnerability

Report vulnerabilities privately, through GitHub's private vulnerability
reporting: open the repository's
[Security tab](https://github.com/mcurcio/agentfs/security) and choose
**Report a vulnerability**
([direct link](https://github.com/mcurcio/agentfs/security/advisories/new)).
Only the maintainers can read the report.

Please do not open a public issue, pull request or discussion for a
vulnerability before a fix is available.

A useful report names:

- the version or commit (`agentfs --version`, or the image digest);
- how agentfs runs: the CLI, the sidecar, the CSI driver, Docker or Helm;
- what an attacker can do, and the steps to reproduce it.

We acknowledge a report, keep you informed while we fix it, and credit you
in the advisory unless you ask us not to.

## Supported versions

agentfs has no releases yet. Fixes land on `main` and in the images built
from it (`ghcr.io/mcurcio/agentfs:latest`); there are no maintained
branches.

## Scope

agentfs's threat model is in [design §1.1](docs/design.md#11-threat-model):
it protects against a workload's mistakes, not against a workload actively
trying to defeat it, except that what a sandbox writes into a fork is
treated as hostile by agentfs itself. In scope, for example:

- history, the journal, state or a credential becoming reachable through a
  mount;
- a change that reached the filesystem going unrecorded without an error;
- forked-volume content making agentfs act on it unsafely;
- credentials leaking into logs, telemetry, history or remotes.

Out of scope: secrets a workload writes into its own tree. agentfs records
them like any other file and does not scan content
([design §10](docs/design.md#10-secrets-not-in-scope)).
