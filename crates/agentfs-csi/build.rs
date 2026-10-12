//! Generates the CSI gRPC code from the vendored specification
//! (`proto/csi.proto`, v1.13.0) with protox, so no `protoc` is needed
//! (design §17.3).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let fds = protox::compile(["csi.proto"], ["proto"])?;
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .generate_default_stubs(true)
        .compile_fds(fds)?;
    Ok(())
}
