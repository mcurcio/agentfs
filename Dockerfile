# syntax=docker/dockerfile:1
#
# The agentfs image: the `agentfs` binary built with the `https` and `ssh`
# features and the `agentfs-csi` CSI driver, on Debian trixie with fuse3,
# git, tini, mergiraf and the CA certificates. It runs as root, deliberately: the
# phase 1 sidecar must be root and privileged to mount FUSE with
# Bidirectional propagation (design §17.1, docs/deployment.md "Security
# trade-offs"). The CSI node plugin runs it as `tini -- agentfs-csi`, root
# and privileged in the driver's namespace only (docs/csi.md).
#
#   docker build -t agentfs .
#
# Base images are pinned by digest; update both together with the Rust
# toolchain (README "Building").

ARG RUST_IMAGE=rust:1.99-trixie@sha256:6ff07edce8775d0f64be7aba9197229407301bddf2054d62c27b541a6238a181
ARG RUNTIME_IMAGE=debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f

FROM ${RUST_IMAGE} AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends libssl-dev pkg-config \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src

# Dependencies first, against stub crates, so a source change does not
# rebuild libgit2, OpenSSL bindings, russh, ring and OpenTelemetry.
COPY Cargo.toml Cargo.lock ./
COPY crates/agentfs-fuse/Cargo.toml crates/agentfs-fuse/
COPY crates/agentfs/Cargo.toml crates/agentfs/
# The build scripts and the protos they compile (tonic, prost, protox) are
# dependencies too: the core's history service and the CSI spec.
COPY crates/agentfs-core/Cargo.toml crates/agentfs-core/build.rs crates/agentfs-core/
COPY crates/agentfs-core/proto crates/agentfs-core/proto
COPY crates/agentfs-csi/Cargo.toml crates/agentfs-csi/build.rs crates/agentfs-csi/
COPY crates/agentfs-csi/proto crates/agentfs-csi/proto
# Every target a manifest declares must exist, so the record_step bench
# gets a stub too.
RUN mkdir -p crates/agentfs-core/src crates/agentfs-core/benches crates/agentfs-fuse/src crates/agentfs/src crates/agentfs-csi/src \
 && echo "fn main() {}" > crates/agentfs-core/benches/record_step.rs \
 && touch crates/agentfs-core/src/lib.rs crates/agentfs-fuse/src/lib.rs crates/agentfs-csi/src/lib.rs \
 && echo 'fn main() { std::process::exit(1) }' > crates/agentfs/src/main.rs \
 && echo 'fn main() { std::process::exit(1) }' > crates/agentfs-csi/src/main.rs \
 && cargo build --release --locked -p agentfs -p agentfs-csi --features agentfs/https,agentfs/ssh,agentfs/otlp-tls

COPY crates crates
# COPY keeps the sources' old mtimes, older than the stub artifacts: touch
# them so cargo rebuilds the workspace crates instead of shipping the stub.
RUN find crates -name '*.rs' -exec touch {} + \
 && cargo build --release --locked -p agentfs -p agentfs-csi --features agentfs/https,agentfs/ssh,agentfs/otlp-tls \
 && install -m 0755 target/release/agentfs target/release/agentfs-csi /usr/local/bin/

# mergiraf, a syntax-aware git merge driver an operator may name in a
# volume's `$GIT_DIR/info/attributes` (design §12, §17.3): its own
# program, run by `git merge-tree` as `merge.<name>.driver`, never linked
# (GPL-3.0-only). A pinned release's static (musl) binary and the licence
# of its tag, both checked by sha256.
FROM ${RUST_IMAGE} AS mergiraf
ARG TARGETARCH
RUN set -eu; version=0.20.0; \
    case "${TARGETARCH:-amd64}" in \
      amd64) arch=x86_64 sum=fc77469be936b02ab9021b984a3aebc9250a21ae3d264f2ef2198eb8b31b2a7b ;; \
      arm64) arch=aarch64 sum=0f153dfa85a799c0d05ee7a21834d049b3595e367ed411a56f007bdb5de8c19d ;; \
      *) echo "no mergiraf release for ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    base=https://codeberg.org/mergiraf/mergiraf; \
    curl -fsSL -o /tmp/mergiraf.tar.gz "$base/releases/download/v$version/mergiraf_$arch-unknown-linux-musl.tar.gz"; \
    echo "$sum  /tmp/mergiraf.tar.gz" | sha256sum -c -; \
    mkdir -p /out/bin /out/doc; \
    tar -xzf /tmp/mergiraf.tar.gz -C /out/bin mergiraf; \
    curl -fsSL -o /out/doc/LICENSE.txt "$base/raw/tag/v$version/LICENSE.txt"; \
    echo "e57f1c320b8cf8798a7d2ff83a6f9e06a33a03585f6e065fea97f1d86db84052  /out/doc/LICENSE.txt" | sha256sum -c -; \
    printf 'mergiraf %s, GPL-3.0-only (LICENSE.txt).\nSource: %s/src/tag/v%s\n' "$version" "$base" "$version" >/out/doc/SOURCE

FROM ${RUNTIME_IMAGE}
# fuse3: fusermount3 and /etc/fuse.conf (a non-root daemon); libssl3t64:
# the `https` and `ssh` transports link OpenSSL; ca-certificates: the TLS
# trust store; git: the daemon repacks the history with it (design §8),
# and operators follow the runbooks (git log, show) with kubectl exec;
# tini: PID 1 of the CSI node plugin, reaping the orphans its children
# leave (design §14.2); mergiraf: see its stage above.
RUN apt-get update \
 && apt-get install -y --no-install-recommends fuse3 libssl3t64 ca-certificates git tini \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/agentfs /usr/local/bin/agentfs-csi /usr/local/bin/
COPY --from=mergiraf /out/bin/mergiraf /usr/local/bin/
COPY --from=mergiraf /out/doc/ /usr/share/doc/mergiraf/
# Fails the build if a shared library is missing, or a stub (which exits
# 1) was shipped.
RUN agentfs --version && agentfs probe --help >/dev/null \
 && agentfs-csi --version && tini --version && mergiraf --version

LABEL org.opencontainers.image.source="https://github.com/mcurcio/agentfs" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.title="agentfs" \
      org.opencontainers.image.description="Records every change an agent makes to a directory, in git history the agent cannot see"

# Root on purpose: the sidecar mounts FUSE (mount(2), /dev/fuse, kernel
# passthrough: CAP_SYS_ADMIN in the initial user namespace) with
# Bidirectional propagation, which Kubernetes allows only in privileged
# containers; it gives new files to their creators (CAP_CHOWN, CAP_FOWNER);
# and libgit2's ownership check wants every process touching the state
# area to be its owner. Workload containers do not use this image.
USER 0
ENTRYPOINT ["/usr/local/bin/agentfs"]
CMD ["--help"]
