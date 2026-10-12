//! Serving the CSI services on a Unix socket (design §14.2, §17.2).

use std::fs;
use std::future::Future;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;

use crate::controller::ControllerService;
use crate::csi::v1::controller_server::ControllerServer;
use crate::csi::v1::identity_server::IdentityServer;
use crate::csi::v1::node_server::{Node, NodeServer};
use crate::identity::IdentityService;

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("endpoint {0:?} is not a Unix socket (unix:///path/to/csi.sock)")]
    NotUnix(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("{what} {}: {source}", path.display())]
    Io {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("serving gRPC: {0}")]
    Transport(#[from] tonic::transport::Error),
}

/// `unix:///csi/csi.sock`, `unix:/csi/csi.sock` or `/csi/csi.sock`.
pub fn parse_endpoint(endpoint: &str) -> Result<PathBuf, EndpointError> {
    let path = endpoint
        .strip_prefix("unix://")
        .or_else(|| endpoint.strip_prefix("unix:"))
        .unwrap_or(endpoint);
    if path.starts_with('/') && path.len() > 1 {
        Ok(PathBuf::from(path))
    } else {
        Err(EndpointError::NotUnix(endpoint.to_owned()))
    }
}

/// Binds `socket` (replacing a stale one, mode 0660) and serves Identity,
/// plus Controller and Node when given, until `shutdown` completes; then
/// stops accepting calls, lets running ones finish, and removes the
/// socket.
pub async fn serve<N: Node>(
    socket: &Path,
    identity: IdentityService,
    controller: Option<ControllerService>,
    node: Option<N>,
    shutdown: impl Future<Output = ()>,
) -> Result<(), ServerError> {
    let io_err = |what, source| ServerError::Io {
        what,
        path: socket.to_path_buf(),
        source,
    };
    if let Some(dir) = socket.parent() {
        fs::create_dir_all(dir).map_err(|e| io_err("creating the directory of", e))?;
    }
    match fs::symlink_metadata(socket) {
        Ok(m) if m.file_type().is_socket() => {
            fs::remove_file(socket).map_err(|e| io_err("removing the stale socket", e))?;
        }
        Ok(_) => {
            return Err(io_err(
                "refusing to replace",
                io::Error::new(io::ErrorKind::AlreadyExists, "not a socket"),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err("inspecting", e)),
    }
    let listener = UnixListener::bind(socket).map_err(|e| io_err("binding", e))?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o660))
        .map_err(|e| io_err("setting the mode of", e))?;
    let result = tonic::transport::Server::builder()
        .add_service(IdentityServer::new(identity))
        .add_optional_service(controller.map(ControllerServer::new))
        .add_optional_service(node.map(NodeServer::new))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), shutdown)
        .await;
    let _ = fs::remove_file(socket);
    result.map_err(ServerError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        for (s, p) in [
            ("unix:///csi/csi.sock", "/csi/csi.sock"),
            ("unix:/csi/csi.sock", "/csi/csi.sock"),
            ("/csi/csi.sock", "/csi/csi.sock"),
        ] {
            assert_eq!(parse_endpoint(s).unwrap(), Path::new(p));
        }
        for bad in ["tcp://127.0.0.1:10000", "unix://", "csi.sock", ""] {
            assert!(parse_endpoint(bad).is_err(), "{bad}");
        }
    }
}
