//! TLS between fronts and the history service across nodes (design
//! §14.1), feature `history-tls`.
//!
//! **Trust.** Each volume has one self-signed certificate and key
//! (ECDSA P-256), made by the active service under the state lock the
//! first time it listens on TLS, in `meta/history/tls/identity.pem`
//! (directory 0700, file 0600, one file so a crash never pairs a key with
//! another key's certificate). Both ends present it and accept exactly it,
//! byte for byte: a peer proves it can read the volume's state area, which
//! is what reading and rewriting its history takes anyway. No CA, no
//! Kubernetes Secret, no names: a pinned certificate needs none.
//!
//! **Protocol.** TLS 1.3 only (rustls on ring), no session tickets (so an
//! idle connection never has bytes waiting and the peek that tells a dead
//! pooled connection still works), no SNI. A changed identity file is
//! read again at the next connection on either side.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, ClientConfig, ClientConnection, DigitallySignedStruct, DistinguishedName,
    Error as TlsError, ServerConfig, ServerConnection, SignatureScheme, StreamOwned,
};

use crate::volume::StateDir;

/// The identity file in the state area.
pub fn identity_file(state: &StateDir) -> PathBuf {
    state.history_dir().join("tls").join("identity.pem")
}

/// Makes sure the volume has its identity, creating it if missing. The
/// caller holds the state lock, so only one process ever creates it.
pub fn ensure(state: &StateDir, volume: &str) -> io::Result<()> {
    let path = identity_file(state);
    if let Ok(pem) = fs::read(&path)
        && parse(&pem).is_ok()
    {
        return Ok(());
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let pem = generate(volume).map_err(io::Error::other)?;
    let tmp = dir.join("identity.pem.tmp");
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(pem.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    fs::File::open(dir)?.sync_all()
}

/// A new certificate and key for `volume`, as PEM.
fn generate(volume: &str) -> Result<String, rcgen::Error> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        format!("agentfs volume {volume}"),
    );
    let cert = params.self_signed(&key)?;
    Ok(format!("{}{}", cert.pem(), key.serialize_pem()))
}

/// The certificate and key in an identity file.
fn parse(pem: &[u8]) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), String> {
    let cert = CertificateDer::from_pem_slice(pem).map_err(|e| format!("certificate: {e}"))?;
    let key = PrivateKeyDer::from_pem_slice(pem).map_err(|e| format!("key: {e}"))?;
    Ok((cert, key))
}

fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::ring::default_provider()))
        .clone()
}

fn algorithms() -> WebPkiSupportedAlgorithms {
    provider().signature_verification_algorithms
}

/// Accepts exactly the volume's certificate, and its TLS 1.3 signatures.
#[derive(Debug)]
struct Pinned {
    cert: CertificateDer<'static>,
}

impl Pinned {
    fn check(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<(), TlsError> {
        if intermediates.is_empty() && end_entity.as_ref() == self.cert.as_ref() {
            Ok(())
        } else {
            Err(TlsError::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn tls13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &algorithms())
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        self.check(end_entity, intermediates)
            .map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        algorithms().supported_schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        self.check(end_entity, intermediates)
            .map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        algorithms().supported_schemes()
    }
}

/// Both ends' TLS settings from one identity file.
struct Configs {
    server: Arc<ServerConfig>,
    client: Arc<ClientConfig>,
}

fn configs(pem: &[u8]) -> Result<Configs, String> {
    let (cert, key) = parse(pem)?;
    let pinned = Arc::new(Pinned { cert: cert.clone() });
    let mut server = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(pinned.clone())
        .with_single_cert(vec![cert.clone()], key.clone_key())
        .map_err(|e| format!("the identity's key does not match its certificate: {e}"))?;
    // An idle connection then never has bytes waiting (design §14.1).
    server.send_tls13_tickets = 0;
    let mut client = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(pinned)
        .with_client_auth_cert(vec![cert], key)
        .map_err(|e| format!("the identity's key does not match its certificate: {e}"))?;
    client.enable_sni = false;
    client.resumption = rustls::client::Resumption::disabled();
    Ok(Configs {
        server: Arc::new(server),
        client: Arc::new(client),
    })
}

/// An identity file's inode, size and mtime: read again when it changes.
type FileStamp = (u64, u64, i64, u64);

/// The configs last read, and the file they were read from.
type Cached = Option<(FileStamp, Arc<Configs>)>;

/// The identity of a volume, read again from its state area when the file
/// changes. Cheap to clone.
#[derive(Clone)]
pub struct IdentitySource {
    path: PathBuf,
    cached: Arc<Mutex<Cached>>,
}

impl IdentitySource {
    /// The identity of `state`'s volume; fails if it has none (yet) or it
    /// is unreadable or malformed.
    pub fn load(state: &StateDir) -> io::Result<Self> {
        let source = Self::for_state(state);
        source.configs()?;
        Ok(source)
    }

    /// This process's source for `state` (one per state area, so every
    /// connection shares its parsed configs).
    pub fn for_state(state: &StateDir) -> Self {
        static SOURCES: Mutex<Option<HashMap<PathBuf, IdentitySource>>> = Mutex::new(None);
        let path = identity_file(state);
        SOURCES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(HashMap::new)
            .entry(path.clone())
            .or_insert_with(|| Self {
                path,
                cached: Arc::default(),
            })
            .clone()
    }

    fn configs(&self) -> io::Result<Arc<Configs>> {
        let meta = fs::metadata(&self.path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "the volume's TLS identity {}: {e} (the active history service creates it \
                     when it listens on TLS)",
                    self.path.display()
                ),
            )
        })?;
        let stamp = (
            meta.ino(),
            meta.size(),
            meta.mtime_nsec(),
            meta.mtime() as u64,
        );
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((s, configs)) = cached.as_ref()
            && *s == stamp
        {
            return Ok(configs.clone());
        }
        let pem = fs::read(&self.path)?;
        let configs = Arc::new(configs(&pem).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the volume's TLS identity {}: {e}", self.path.display()),
            )
        })?);
        *cached = Some((stamp, configs.clone()));
        Ok(configs)
    }
}

enum Inner {
    Client(StreamOwned<ClientConnection, TcpStream>),
    Server(StreamOwned<ServerConnection, TcpStream>),
}

/// An established TLS connection, both ends authenticated.
pub struct TlsStream {
    inner: Inner,
}

/// Runs a handshake to completion within the socket's timeouts.
fn handshake<S: rustls::SideData>(
    conn: &mut rustls::ConnectionCommon<S>,
    sock: &mut TcpStream,
) -> io::Result<()> {
    while conn.is_handshaking() {
        let (read, written) = conn.complete_io(sock)?;
        if read == 0 && written == 0 && conn.is_handshaking() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the peer closed the connection during the TLS handshake",
            ));
        }
    }
    // The last flight (the client's Finished) is sent.
    while conn.wants_write() {
        conn.write_tls(sock)?;
    }
    Ok(())
}

impl TlsStream {
    /// The client side: proves this front holds the volume's identity to
    /// the service, and checks the service holds it too, within `timeout`.
    pub fn connect(tcp: TcpStream, state: &StateDir, timeout: Duration) -> io::Result<Self> {
        let configs = IdentitySource::for_state(state).configs()?;
        let name = ServerName::try_from("agentfs-history").map_err(io::Error::other)?;
        let mut conn = ClientConnection::new(configs.client.clone(), name.to_owned())
            .map_err(io::Error::other)?;
        let mut tcp = tcp;
        let timeout = Some(timeout.max(Duration::from_millis(1)));
        tcp.set_read_timeout(timeout)?;
        tcp.set_write_timeout(timeout)?;
        handshake(&mut conn, &mut tcp)?;
        Ok(Self {
            inner: Inner::Client(StreamOwned::new(conn, tcp)),
        })
    }

    /// The server side, within `timeout`.
    pub fn accept(
        tcp: TcpStream,
        identity: &IdentitySource,
        timeout: Duration,
    ) -> io::Result<Self> {
        let configs = identity.configs()?;
        let mut conn = ServerConnection::new(configs.server.clone()).map_err(io::Error::other)?;
        let mut tcp = tcp;
        let timeout = Some(timeout.max(Duration::from_millis(1)));
        tcp.set_read_timeout(timeout)?;
        tcp.set_write_timeout(timeout)?;
        handshake(&mut conn, &mut tcp)?;
        Ok(Self {
            inner: Inner::Server(StreamOwned::new(conn, tcp)),
        })
    }

    /// The TCP connection underneath.
    pub fn tcp(&self) -> &TcpStream {
        match &self.inner {
            Inner::Client(s) => &s.sock,
            Inner::Server(s) => &s.sock,
        }
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Client(s) => s.read(buf),
            Inner::Server(s) => s.read(buf),
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Client(s) => s.write(buf),
            Inner::Server(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            Inner::Client(s) => s.flush(),
            Inner::Server(s) => s.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        (dir, state)
    }

    /// Accepts one connection with `server`'s identity and echoes a line.
    fn echo_server(
        server: &StateDir,
    ) -> (
        std::net::SocketAddr,
        std::thread::JoinHandle<io::Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let identity = IdentitySource::load(server).unwrap();
        let thread = std::thread::spawn(move || {
            let (tcp, _) = listener.accept()?;
            let mut s = TlsStream::accept(tcp, &identity, Duration::from_secs(5))?;
            let mut buf = [0; 5];
            s.read_exact(&mut buf)?;
            s.write_all(&buf)?;
            s.flush()
        });
        (addr, thread)
    }

    #[test]
    fn the_identity_is_made_once_with_private_modes() {
        let (_dir, state) = state();
        ensure(&state, "vol").unwrap();
        let path = identity_file(&state);
        let first = fs::read(&path).unwrap();
        ensure(&state, "vol").unwrap();
        assert_eq!(fs::read(&path).unwrap(), first, "kept");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let text = String::from_utf8(first).unwrap();
        assert!(text.contains("BEGIN CERTIFICATE") && text.contains("PRIVATE KEY"));
        // A torn file is made again.
        fs::write(&path, "garbage").unwrap();
        ensure(&state, "vol").unwrap();
        assert!(parse(&fs::read(&path).unwrap()).is_ok());
    }

    #[test]
    fn both_ends_with_the_volumes_identity_talk() {
        let (_dir, state) = state();
        ensure(&state, "vol").unwrap();
        let (addr, server) = echo_server(&state);
        let tcp = TcpStream::connect(addr).unwrap();
        let mut c = TlsStream::connect(tcp, &state, Duration::from_secs(5)).unwrap();
        c.write_all(b"hello").unwrap();
        c.flush().unwrap();
        let mut buf = [0; 5];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        server.join().unwrap().unwrap();
    }

    #[test]
    fn another_volumes_identity_is_refused_both_ways() {
        let (_d1, server_state) = state();
        let (_d2, client_state) = state();
        ensure(&server_state, "vol").unwrap();
        ensure(&client_state, "vol").unwrap();
        let (addr, server) = echo_server(&server_state);
        let tcp = TcpStream::connect(addr).unwrap();
        let client = TlsStream::connect(tcp, &client_state, Duration::from_secs(5));
        assert!(
            client.is_err(),
            "the client refuses the server's certificate"
        );
        assert!(
            server.join().unwrap().is_err(),
            "and the server sees it fail"
        );
    }

    #[test]
    fn a_client_without_a_certificate_is_refused() {
        let (_dir, state) = state();
        ensure(&state, "vol").unwrap();
        let (addr, server) = echo_server(&state);
        // A client that trusts the pinned certificate but presents none.
        let pem = fs::read(identity_file(&state)).unwrap();
        let (cert, _) = parse(&pem).unwrap();
        let config = ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned { cert }))
            .with_no_client_auth();
        let name = ServerName::try_from("x").unwrap().to_owned();
        let mut conn = ClientConnection::new(Arc::new(config), name).unwrap();
        let mut tcp = TcpStream::connect(addr).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = handshake(&mut conn, &mut tcp);
        let mut s = StreamOwned::new(conn, tcp);
        let _ = s.write_all(b"hello");
        let _ = s.flush();
        let mut buf = [0; 5];
        assert!(s.read_exact(&mut buf).is_err());
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn plaintext_is_refused() {
        let (_dir, state) = state();
        ensure(&state, "vol").unwrap();
        let (addr, server) = echo_server(&state);
        let mut tcp = TcpStream::connect(addr).unwrap();
        tcp.write_all(&[0, 0, 0, 5]).unwrap();
        tcp.write_all(b"hello").unwrap();
        let mut buf = Vec::new();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = tcp.read_to_end(&mut buf);
        assert!(!buf.windows(5).any(|w| w == b"hello"), "never echoed");
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn a_new_identity_is_picked_up() {
        let (_dir, state) = state();
        ensure(&state, "vol").unwrap();
        let source = IdentitySource::load(&state).unwrap();
        let first = source.configs().unwrap();
        assert!(Arc::ptr_eq(&first, &source.configs().unwrap()), "cached");
        fs::remove_file(identity_file(&state)).unwrap();
        assert!(source.configs().is_err(), "no identity");
        ensure(&state, "vol").unwrap();
        assert!(!Arc::ptr_eq(&first, &source.configs().unwrap()));
        // Both ends read the new one.
        let (addr, server) = echo_server(&state);
        let tcp = TcpStream::connect(addr).unwrap();
        let mut c = TlsStream::connect(tcp, &state, Duration::from_secs(5)).unwrap();
        c.write_all(b"again").unwrap();
        c.flush().unwrap();
        let mut buf = [0; 5];
        c.read_exact(&mut buf).unwrap();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn a_missing_identity_is_an_error_that_says_so() {
        let (_dir, state) = state();
        let err = IdentitySource::load(&state).err().unwrap();
        assert!(err.to_string().contains("TLS identity"), "{err}");
    }
}
