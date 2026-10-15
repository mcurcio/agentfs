//! Unix sockets in the state area, shared by the control socket of a mount
//! (design §15) and the history service (§14).
//!
//! Both ends address a socket as `/proc/self/fd/<dir fd>/<name>`, so a long
//! state-area path never hits the 108-byte `sun_path` limit. A private
//! socket's directory is `0700` and the socket `0600`, and a server accepts
//! only peers whose `SO_PEERCRED` uid is root or its own effective uid
//! ([`allowed`]).

use std::fs::DirBuilder;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use rustix::fs::{AtFlags, Mode, OFlags};

/// Who is on the other end of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub uid: u32,
    pub gid: u32,
    /// 0 when unknown (the peer is outside this process's pid namespace).
    pub pid: u32,
}

/// Whether a peer with `uid` may use a server running as `server_euid`.
pub fn allowed(uid: u32, server_euid: u32) -> bool {
    uid == 0 || uid == server_euid
}

/// Opens `dir` as a path descriptor for [`socket_address`].
pub fn open_dir(dir: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        dir,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// `/proc/self/fd/<dir>/<name>`: the socket `name` in the directory `dir`
/// is open on, whatever the length of its path.
pub fn socket_address(dir: &OwnedFd, name: &str) -> String {
    format!("/proc/self/fd/{}/{name}", dir.as_raw_fd())
}

/// Listens on `name` in `dir`, created and set to `0700`; the socket is
/// `0600`. See [`bind`].
pub fn bind_private(dir: &Path, name: &str) -> io::Result<(OwnedFd, UnixListener)> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    bind(dir, name)
}

/// Listens on `name` in the existing directory `dir`, leaving the
/// directory's mode as it is; the socket is `0600`. A stale socket (a
/// crashed server's) is replaced; a live one fails with
/// [`io::ErrorKind::AddrInUse`] and is left alone.
pub fn bind(dir: &Path, name: &str) -> io::Result<(OwnedFd, UnixListener)> {
    let dir_fd = open_dir(dir)?;
    let address = socket_address(&dir_fd, name);
    match UnixStream::connect(&address) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "another process is listening on {}",
                    dir.join(name).display()
                ),
            ));
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => return Err(e),
    }
    match rustix::fs::unlinkat(&dir_fd, name, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&address)?;
    rustix::fs::chmodat(&dir_fd, name, Mode::from_raw_mode(0o600), AtFlags::empty())?;
    Ok((dir_fd, listener))
}

/// Removes the socket `name` from the directory `dir` is open on.
pub fn unlink(dir: &OwnedFd, name: &str) -> io::Result<()> {
    match rustix::fs::unlinkat(dir, name, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Connects to the socket `name` in `dir`. A missing directory or socket
/// fails with [`io::ErrorKind::NotFound`], a stale socket with
/// [`io::ErrorKind::ConnectionRefused`].
pub fn connect(dir: &Path, name: &str) -> io::Result<UnixStream> {
    let dir_fd = open_dir(dir)?;
    UnixStream::connect(socket_address(&dir_fd, name))
}

/// The connection's `SO_PEERCRED`. The pid is 0 when the peer is in a pid
/// namespace this process cannot see.
pub fn peer(stream: &UnixStream) -> io::Result<Peer> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is a valid, writable `ucred` of `len` bytes, the
    // buffer SO_PEERCRED fills; the descriptor is open for the call.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::other("short SO_PEERCRED reply"));
    }
    Ok(Peer {
        uid: cred.uid,
        gid: cred.gid,
        pid: u32::try_from(cred.pid).unwrap_or(0),
    })
}

/// Whether the peer has closed `stream` (or it has failed), without
/// blocking or consuming anything: a pooled connection to a server that
/// has since restarted is dead and must not carry a request. A connection
/// with unread data is not idle and also counts as unusable.
pub fn is_closed(stream: &impl AsRawFd) -> bool {
    let mut byte = 0u8;
    // SAFETY: `byte` is a valid, writable buffer of 1 byte; the descriptor
    // is open for the call.
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&raw mut byte).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if n < 0 {
        let err = io::Error::last_os_error();
        !matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        )
    } else {
        // 0 is the peer's EOF; data nobody asked for is a protocol fault.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn peer_credentials_are_the_peers_own() {
        let (a, _b) = UnixStream::pair().unwrap();
        let peer = peer(&a).unwrap();
        assert_eq!(peer.pid, std::process::id());
        assert_eq!(peer.uid, rustix::process::geteuid().as_raw());
        assert_eq!(peer.gid, rustix::process::getegid().as_raw());
    }

    #[test]
    fn only_root_and_the_servers_user() {
        assert!(allowed(0, 1000));
        assert!(allowed(1000, 1000));
        assert!(!allowed(1001, 1000));
        assert!(allowed(0, 0));
        assert!(!allowed(1000, 0));
    }

    #[test]
    fn private_sockets_work_at_any_depth_with_private_modes() {
        let tmp = tempfile::tempdir().unwrap();
        // The socket path itself would be over 108 bytes.
        let dir = tmp.path().join("x".repeat(60)).join("y".repeat(60));
        let (_fd, listener) = bind_private(&dir, "s.sock").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("s.sock")), 0o600);

        let mut client = connect(&dir, "s.sock").unwrap();
        let (mut server, _) = listener.accept().unwrap();
        client.write_all(b"hi").unwrap();
        let mut buf = [0; 2];
        server.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");
    }

    #[test]
    fn a_live_socket_is_not_replaced_but_a_stale_one_is() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        let (fd, first) = bind_private(&dir, "s").unwrap();
        let err = bind(&dir, "s").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "{err}");
        drop(first);
        assert_eq!(
            connect(&dir, "s").unwrap_err().kind(),
            io::ErrorKind::ConnectionRefused
        );
        let (_fd, _second) = bind(&dir, "s").unwrap();
        connect(&dir, "s").unwrap();
        unlink(&fd, "s").unwrap();
        assert_eq!(
            connect(&dir, "s").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn bind_leaves_a_shared_directorys_mode_alone() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let _l = bind(tmp.path(), "s").unwrap();
        let mode = std::fs::metadata(tmp.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn closed_connections_are_detected() {
        let (a, b) = UnixStream::pair().unwrap();
        assert!(!is_closed(&a));
        drop(b);
        assert!(is_closed(&a));
        let (a, mut b) = UnixStream::pair().unwrap();
        b.write_all(b"x").unwrap();
        assert!(is_closed(&a), "unread data makes it unusable");
    }
}
