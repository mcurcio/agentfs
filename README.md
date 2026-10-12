# agentfs

A filesystem that records every change an agent makes.

agentfs presents a git branch as an ordinary directory. Any process, root or
not, reads and writes it with normal file calls and never sees git. Every
completed change made through the mount is durably recorded before the call
returns, then committed to the branch and pushed to a remote such as GitHub.
The history lives outside the mount, where the agent cannot see or reach it.

It is built in two layers:

1. **The FUSE layer**: a Rust library and daemon (`agentfs mount`) that runs
   anywhere FUSE does, with no Kubernetes dependency.
2. **The CSI layer**: a Kubernetes CSI driver that gives any pod an agentfs
   volume declared like any other volume.

Configuration uses git's own files: `.gitignore`, `.gitattributes`,
`.lfsconfig`, `$GIT_DIR/info/*` and git-config syntax.

## Status

Design. See [docs/design.md](docs/design.md). No code yet.
