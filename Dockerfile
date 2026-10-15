# syntax=docker/dockerfile:1
#
# The agentfs image: the `agentfs` binary built with the `https` and `ssh`
# features and the `agentfs-csi` CSI driver, on Debian trixie with fuse3,
# git, tini and the CA certificates. It runs as root, deliberately: the
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
COPY crates/agentfs-core/Cargo.toml crates/agentfs-core/
COPY crates/agentfs-fuse/Cargo.toml crates/agentfs-fuse/
COPY crates/agentfs/Cargo.toml crates/agentfs/
# The CSI crate's build script and the CSI spec it compiles (tonic, prost,
# protox) are dependencies too.
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

FROM ${RUNTIME_IMAGE}
# fuse3: fusermount3 and /etc/fuse.conf (a non-root daemon); libssl3t64:
# the `https` and `ssh` transports link OpenSSL; ca-certificates: the TLS
# trust store; git: the daemon repacks the history with it (design §8),
# and operators follow the runbooks (git log, show) with kubectl exec;
# tini: PID 1 of the CSI node plugin, reaping the orphans its children
# leave (design §14.2).
RUN apt-get update \
 && apt-get install -y --no-install-recommends fuse3 libssl3t64 ca-certificates git tini \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/agentfs /usr/local/bin/agentfs-csi /usr/local/bin/
# Fails the build if a shared library is missing, or a stub (which exits
# 1) was shipped.
RUN agentfs --version && agentfs probe --help >/dev/null \
 && agentfs-csi --version && tini --version

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
