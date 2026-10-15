//! Generates the history service's gRPC code (`agentfs.history.v2`,
//! design §14) from `proto/` with protox, so no `protoc` is needed (design
//! §17.3). Client and server only: the transport is the crate's own, over
//! the node-local Unix socket.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let fds = protox::compile(["agentfs/history/v2/history.proto"], ["proto"])?;
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .build_transport(false)
        .compile_fds(fds)?;
    Ok(())
}
