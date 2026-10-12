//! The CSI v1 protocol (spec v1.13.0), generated at build time from
//! `proto/csi.proto` (design §17.2).

/// `csi.v1`: messages, and the Identity, Controller, Node (and unused
/// GroupController and SnapshotMetadata) services.
#[allow(
    clippy::all,
    clippy::pedantic,
    missing_docs,
    rustdoc::all,
    unreachable_pub
)]
pub mod v1 {
    tonic::include_proto!("csi.v1");
}
