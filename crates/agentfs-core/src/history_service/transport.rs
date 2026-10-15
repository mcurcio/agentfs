//! The connections fronts and the history service talk over (design
//! §14.1): a Unix socket on the service's node, or TLS over TCP from
//! another node (feature `history-tls`). Both carry the same frames
//! ([`super::protocol`]).
//!
//! A [`Stream`] is one connection; a [`Handle`] is a second descriptor of
//! the same socket, to shut it down or peek at it from another thread
//! while the stream is in use. A [`Listener`] accepts connections; a TLS
//! connection's handshake runs on the connection's own thread
//! ([`Accepted::establish`]), so a slow peer never holds the accept loop.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use crate::local_socket::{self, Peer};

/// Who is on the other end of a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerInfo {
    /// A local process, by `SO_PEERCRED`.
    Local(Peer),
    /// A TCP peer that proved it holds the volume's certificate.
    Remote(SocketAddr),
}

/// One connection.
pub enum Stream {
    Unix(UnixStream),
    #[cfg(feature = "history-tls")]
    Tls(Box<super::tls::TlsStream>),
}

/// Another descriptor of a [`Stream`]'s socket.
#[derive(Debug)]
pub enum Handle {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Handle {
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Self::Unix(s) => s.shutdown(how),
            Self::Tcp(s) => s.shutdown(how),
        }
    }

    /// Whether the peer has closed the connection (or it failed, or holds
    /// unread data): see [`local_socket::is_closed`].
    pub fn is_closed(&self) -> bool {
        match self {
            Self::Unix(s) => local_socket::is_closed(s),
            Self::Tcp(s) => local_socket::is_closed(s),
        }
    }
}

impl Stream {
    /// A second descriptor of the socket.
    pub fn handle(&self) -> io::Result<Handle> {
        match self {
            Self::Unix(s) => s.try_clone().map(Handle::Unix),
            #[cfg(feature = "history-tls")]
            Self::Tls(s) => s.tcp().try_clone().map(Handle::Tcp),
        }
    }

    /// See [`Handle::is_closed`]. On a TLS connection the peer sends
    /// nothing unasked (no session tickets, design §14.1), so a byte
    /// waiting is its close_notify, or a fault: unusable either way.
    pub fn is_closed(&self) -> bool {
        match self {
            Self::Unix(s) => local_socket::is_closed(s),
            #[cfg(feature = "history-tls")]
            Self::Tls(s) => local_socket::is_closed(s.tcp()),
        }
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(s) => s.set_read_timeout(timeout),
            #[cfg(feature = "history-tls")]
            Self::Tls(s) => s.tcp().set_read_timeout(timeout),
        }
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(s) => s.set_write_timeout(timeout),
            #[cfg(feature = "history-tls")]
            Self::Tls(s) => s.tcp().set_write_timeout(timeout),
        }
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Self::Unix(s) => s.shutdown(how),
            #[cfg(feature = "history-tls")]
            Self::Tls(s) => s.tcp().shutdown(how),
        }
    }

    /// Who is on the other end.
    pub fn peer(&self) -> io::Result<PeerInfo> {
        match self {
            Self::Unix(s) => local_socket::peer(s).map(PeerInfo::Local),
            #[cfg(feature = "history-tls")]
            Self::Tls(s) => s.tcp().peer_addr().map(PeerInfo::Remote),
        }
    }

    /// Whether this is a TLS connection.
    pub fn is_tls(&self) -> bool {
        !matches!(self, Self::Unix(_))
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(s) => s.read(buf),
            #[cfg(feature = "history-tls")]
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(s) => s.write(buf),
            #[cfg(feature = "history-tls")]
            Self::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(s) => s.flush(),
            #[cfg(feature = "history-tls")]
            Self::Tls(s) => s.flush(),
        }
    }
}

/// Accepts connections.
pub enum Listener {
    Unix(UnixListener),
    /// TLS over TCP: each accepted connection is handshaken on its own
    /// thread, as the service's identity at that moment says.
    #[cfg(feature = "history-tls")]
    Tls {
        listener: TcpListener,
        identity: super::tls::IdentitySource,
    },
}

/// A connection accepted and not yet established.
pub enum Accepted {
    Unix(UnixStream),
    #[cfg(feature = "history-tls")]
    Tcp(TcpStream, super::tls::IdentitySource),
}

impl Accepted {
    /// Completes the connection: for TLS, the handshake, within `timeout`,
    /// which also proves the peer holds the volume's certificate.
    pub fn establish(self, timeout: Duration) -> io::Result<Stream> {
        let _ = timeout;
        match self {
            Self::Unix(s) => Ok(Stream::Unix(s)),
            #[cfg(feature = "history-tls")]
            Self::Tcp(tcp, identity) => super::tls::TlsStream::accept(tcp, &identity, timeout)
                .map(|s| Stream::Tls(Box::new(s))),
        }
    }

    /// A second descriptor of the socket, to shut it down from another
    /// thread before or after it is established.
    pub fn handle(&self) -> io::Result<Handle> {
        match self {
            Self::Unix(s) => s.try_clone().map(Handle::Unix),
            #[cfg(feature = "history-tls")]
            Self::Tcp(s, _) => s.try_clone().map(Handle::Tcp),
        }
    }

    /// Whether it is a TLS connection (which cannot carry a reply before
    /// its handshake).
    pub fn is_tls(&self) -> bool {
        !matches!(self, Self::Unix(_))
    }

    /// The plain Unix stream, for an answer before the hello.
    pub fn as_unix(&mut self) -> Option<&mut UnixStream> {
        match self {
            Self::Unix(s) => Some(s),
            #[cfg(feature = "history-tls")]
            Self::Tcp(..) => None,
        }
    }
}

impl Listener {
    pub fn accept(&self) -> io::Result<Accepted> {
        match self {
            Self::Unix(l) => l.accept().map(|(s, _)| Accepted::Unix(s)),
            #[cfg(feature = "history-tls")]
            Self::Tls { listener, identity } => {
                let (s, _) = listener.accept()?;
                tune_tcp(&s);
                Ok(Accepted::Tcp(s, identity.clone()))
            }
        }
    }

    /// The TCP address it listens on, for a TLS listener.
    pub fn local_tcp(&self) -> Option<SocketAddr> {
        match self {
            Self::Unix(_) => None,
            #[cfg(feature = "history-tls")]
            Self::Tls { listener, .. } => listener.local_addr().ok(),
        }
    }
}

/// Connects a TCP stream within `timeout`, tuned for the history protocol.
pub fn connect_tcp(addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, timeout.max(Duration::from_millis(1)))?;
    tune_tcp(&stream);
    Ok(stream)
}

/// Wakes a TCP listener's blocking accept: connects to it (the loopback
/// address for a listener on every address).
pub fn wake_tcp(listener: &TcpListener) {
    if let Ok(mut addr) = listener.local_addr() {
        if addr.ip().is_unspecified() {
            addr.set_ip(match addr {
                SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
            });
        }
        let _ = TcpStream::connect_timeout(&addr, Duration::from_secs(1));
    }
}

/// `TCP_NODELAY` (one small frame per request), keepalives that notice a
/// dead peer within about 25 s, and `TCP_USER_TIMEOUT` of 30 s, so a
/// connection to a node that vanished fails instead of hanging (design
/// §14.1). Failures leave the defaults.
pub fn tune_tcp(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let fd = stream.as_fd();
    set_int(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set_int(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 10);
    set_int(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 5);
    set_int(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
    set_int(fd, libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, 30_000);
}

fn set_int(fd: BorrowedFd<'_>, level: libc::c_int, name: libc::c_int, value: libc::c_int) {
    // SAFETY: `value` is a valid c_int for the call's duration, and its
    // size is passed; the descriptor is open.
    unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            level,
            name,
            (&raw const value).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_streams_read_write_and_shut_down() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut a = Stream::Unix(a);
        let mut b = Stream::Unix(b);
        a.write_all(b"hi").unwrap();
        a.flush().unwrap();
        let mut buf = [0; 2];
        b.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");
        assert!(matches!(b.peer().unwrap(), PeerInfo::Local(_)));
        assert!(!a.is_tls());
        let handle = a.handle().unwrap();
        assert!(!handle.is_closed());
        b.shutdown(Shutdown::Both).unwrap();
        assert!(a.is_closed());
        assert!(handle.is_closed());
    }

    #[test]
    fn tcp_connections_are_tuned() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = connect_tcp(addr, Duration::from_secs(5)).unwrap();
        assert!(stream.nodelay().unwrap());
        let (server, _) = listener.accept().unwrap();
        let handle = Handle::Tcp(stream);
        assert!(!handle.is_closed());
        drop(server);
        // The FIN arrives at once on loopback.
        std::thread::sleep(Duration::from_millis(20));
        assert!(handle.is_closed());
    }

    #[test]
    fn a_listener_on_every_address_is_woken_through_loopback() {
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let waker = std::thread::spawn({
            let l = listener.try_clone().unwrap();
            move || wake_tcp(&l)
        });
        listener.accept().unwrap();
        waker.join().unwrap();
    }
}
