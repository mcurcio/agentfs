# Contributing to agentfs

Thanks for helping. agentfs is a FUSE filesystem that records every change
a workload makes into git history the workload cannot see; start with the
[README](README.md) for what it does and [docs/design.md](docs/design.md)
for why.

By contributing you agree that your work is dual-licensed under the
[MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE) licences, as the
project is, and you follow the [Code of Conduct](CODE_OF_CONDUCT.md).
Report security problems privately, as [SECURITY.md](SECURITY.md) says,
not in an issue.

## Before you start

- **Read the design.** [docs/design.md](docs/design.md) is authoritative:
  behaviour and guarantees, decisions (§19.1, dated) and open questions
  (§19.2). A change that alters behaviour updates it in the same commit.
- **Open an issue first** for anything larger than a fix, so the approach
  can be agreed before you build it.
- **Every fact has one home.** [AGENTS.md](AGENTS.md) lists where each kind
  of fact lives (README, design, configuration and architecture
  references, runbooks); link to it rather than copying it.

## Building and testing

You need stable Rust and a C compiler (libgit2 is built from source); the
`https` feature also needs OpenSSL headers (`libssl-dev` and `pkg-config`
on Debian and Ubuntu). [docs/development.md](docs/development.md) covers
the cargo features, the container image and what each CI job runs.

Every commit passes these, as CI's `lint` and `test` jobs run them:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The documentation is tested against the code:
[docs/configuration.md](docs/configuration.md) must list every key, flag
and environment variable, [docs/architecture.md](docs/architecture.md)
every module, and every relative link in the README and docs must resolve
(`cargo test -p agentfs-core --test docs_reference`).

Mounts, root, kind, Helm, sshd and the other end-to-end tests need Linux
with `/dev/fuse` and usually root, so they run as scripts under `ci/` in
GitHub Actions. Run what you can locally and let CI run the rest.

### A local Linux gate in a container

On macOS, or without the toolchain installed, run the gate in a container.
Build a development image once with Docker (Podman takes the same commands),
from the same Rust image the [Dockerfile](Dockerfile) builds with:

```sh
docker build -t agentfs-dev -f - . <<'EOF'
FROM docker.io/library/rust:1.99-trixie
RUN apt-get update \
 && apt-get install -y --no-install-recommends libssl-dev pkg-config git-lfs \
 && rm -rf /var/lib/apt/lists/* \
 && rustup component add rustfmt clippy
EOF
```

Then run any cargo command against your checkout, with the build and the
registry in named volumes so they survive between runs:

```sh
docker run --rm -v "$PWD":/src -v agentfs-target:/target \
  -v agentfs-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target \
  -w /src agentfs-dev cargo test --workspace
```

The build output and the registry stay in the container engine's storage, so
put that storage on a disk with room (a full Rust build is several GB).
FUSE mounts do not work in this container; CI runs them.

### The logo

The logo files in [docs/assets/](docs/assets/) are SVGs drawn on an
integer pixel grid. Each PNG beside them is rendered from its SVG: after
editing an SVG, run `ci/render-assets.sh` (it needs
[resvg](https://github.com/linebender/resvg); `ci/install-resvg.sh <dir>`
installs the pinned version) and commit both. CI fails when a PNG does
not match its SVG.

## Continuous integration

Every push and pull request runs the workflows in
[.github/workflows](.github/workflows). The table in
[AGENTS.md, "What only CI can test"](AGENTS.md#what-only-ci-can-test)
lists each workflow, when it runs, how long it takes and what it covers.
A pull request is ready to merge when its `ci`, `image`, `helm` and `docs`
runs are green.

## Commits and pull requests

- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/):
  `type(scope): subject`, for example `fix(csi): ...` or `docs(helm): ...`.
  Use an imperative subject of at most 72 characters and a short body that
  says why.
- Keep each commit to one change, with its tests and documentation in the
  same commit.
- Add or update tests that prove the behaviour through the real binary or
  mount where possible; end-to-end scripts go under `ci/` with a job in a
  workflow.
- Note user-visible changes under **Unreleased** in
  [CHANGELOG.md](CHANGELOG.md).
- Pin third-party GitHub Actions by full commit SHA and runners to
  `ubuntu-24.04`; a test checks the runners.
