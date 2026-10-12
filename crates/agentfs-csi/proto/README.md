# Vendored CSI specification

`csi.proto` is the Container Storage Interface specification, unchanged:

- Source: <https://github.com/container-storage-interface/spec/blob/v1.13.0/csi.proto>
- Tag: `v1.13.0` (commit `b516e92e3d6c0d27e6c8ffd0a956e3918b50f619`), the
  version `csi-test` v5.6.0 (`csi-sanity`) is built against
- sha256: `8c5604cb76fefff19c01cf88ebd229e4f8c88c419898b07e4914318d25d84af3`
- Licence: Apache License 2.0, copyright the CSI authors
  (<https://github.com/container-storage-interface/spec/blob/v1.13.0/LICENSE>)

`build.rs` compiles it with `protox` (a pure-Rust protobuf compiler that
bundles the well-known types, so no `protoc` is needed) and generates the
tonic client and server with `tonic-prost-build` (design §17.3).

To update: fetch the new tag's `csi.proto` here, update the tag, commit and
sha256 above, and check that `csi-sanity` in CI uses a matching
`csi-test` release.
