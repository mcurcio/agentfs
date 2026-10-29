//! A local TCP port that refuses every connection for as long as the
//! test holds it.
//!
//! Binding `127.0.0.1:0` and dropping the listener to get "a port nothing
//! listens on" frees the port: a concurrent test's `bind("127.0.0.1:0")`
//! can be handed it, and the connections meant to be refused reach that
//! test's server instead. A [`ClosedPort`] keeps a socket bound to the
//! port, never listening and without `SO_REUSEADDR`, so the kernel refuses
//! connections to it (RST) and gives the port to no other socket until
//! the value is dropped.
//!
//! Shared by `crates/agentfs-core` and (through `#[path]`)
//! `crates/agentfs/tests`.

#![allow(dead_code)]

use std::net::{Ipv4Addr, SocketAddr};

use socket2::{Domain, Socket, Type};

/// Holds a bound, never-listening socket on a loopback port.
pub struct ClosedPort {
    socket: Socket,
    port: u16,
}

impl ClosedPort {
    pub fn new() -> Self {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        socket
            .bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into())
            .unwrap();
        let port = socket
            .local_addr()
            .unwrap()
            .as_socket()
            .expect("an IPv4 socket has an IPv4 address")
            .port();
        Self { socket, port }
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

#[test]
fn a_closed_port_refuses_connections_and_is_given_to_no_listener() {
    use std::io::ErrorKind;
    use std::net::{TcpListener, TcpStream};

    let closed = ClosedPort::new();
    let err = TcpStream::connect((Ipv4Addr::LOCALHOST, closed.port())).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ConnectionRefused, "{err}");
    // std sets SO_REUSEADDR; the held socket does not, so the bind fails.
    let err = TcpListener::bind((Ipv4Addr::LOCALHOST, closed.port())).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AddrInUse, "{err}");
    let err = TcpListener::bind((Ipv4Addr::UNSPECIFIED, closed.port())).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AddrInUse, "{err}");
}
