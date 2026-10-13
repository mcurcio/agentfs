//! TLS to the OpenTelemetry collector (design §13), built with the
//! `otlp-tls` feature: rustls on ring, no OpenSSL.
//!
//! The CA bundle (`OTEL_EXPORTER_OTLP_CERTIFICATE`), when set, is the only
//! trust: the system store is not consulted. Without it the system store
//! is (`SSL_CERT_FILE` and `SSL_CERT_DIR` move it). A client certificate
//! and key, set together, give mutual TLS. Every file is read and checked
//! when the exporter is built, so a bad file turns its signal off with a
//! warning at start instead of failing each export.
//!
//! - The HTTP exporters get agentfs's own blocking reqwest client with a
//!   rustls configuration ([`http_client`]), and so the timeout
//!   (`OTEL_EXPORTER_OTLP_TIMEOUT`) that the exporter's default client
//!   would have applied.
//! - The gRPC exporter gets tonic's TLS settings ([`grpc_config`]), with the
//!   same files.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::RootCertStore;
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use super::Tls;

/// Why TLS to the collector cannot be set up.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} holds no PEM certificate", path.display())]
    NoCertificate { path: PathBuf },
    #[error("{} holds no PEM private key", path.display())]
    NoKey { path: PathBuf },
    #[error("{}: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
    #[error("no CA certificates in the system store (set OTEL_EXPORTER_OTLP_CERTIFICATE)")]
    NoSystemRoots,
    #[error("TLS setup: {0}")]
    Setup(String),
}

/// The files of a [`Tls`], read and parsed.
struct Loaded {
    /// The CA bundle as PEM, and its certificates.
    ca: Option<(Vec<u8>, Vec<CertificateDer<'static>>)>,
    /// The client chain and key as PEM, and parsed.
    client: Option<Client>,
}

struct Client {
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

fn read(path: &Path) -> Result<Vec<u8>, TlsError> {
    fs::read(path).map_err(|source| TlsError::Read {
        path: path.to_owned(),
        source,
    })
}

fn certificates(path: &Path, pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::Invalid {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificate {
            path: path.to_owned(),
        });
    }
    Ok(certs)
}

fn load(tls: &Tls) -> Result<Loaded, TlsError> {
    let ca = match &tls.ca {
        Some(path) => {
            let pem = read(path)?;
            let certs = certificates(path, &pem)?;
            Some((pem, certs))
        }
        None => None,
    };
    let client = match &tls.client {
        Some((cert_path, key_path)) => {
            let cert_pem = read(cert_path)?;
            let chain = certificates(cert_path, &cert_pem)?;
            let key_pem = read(key_path)?;
            let key = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|e| match e {
                rustls_pki_types::pem::Error::NoItemsFound => TlsError::NoKey {
                    path: key_path.clone(),
                },
                e => TlsError::Invalid {
                    path: key_path.clone(),
                    message: e.to_string(),
                },
            })?;
            Some(Client {
                cert_pem,
                key_pem,
                chain,
                key,
            })
        }
        None => None,
    };
    Ok(Loaded { ca, client })
}

/// The rustls configuration for `tls`: the bundle or the system store, and
/// the client certificate if any.
fn rustls_config(tls: &Tls) -> Result<rustls::ClientConfig, TlsError> {
    let loaded = load(tls)?;
    let mut roots = RootCertStore::empty();
    match (&loaded.ca, &tls.ca) {
        (Some((_, certs)), Some(path)) => {
            for cert in certs {
                roots.add(cert.clone()).map_err(|e| TlsError::Invalid {
                    path: path.clone(),
                    message: e.to_string(),
                })?;
            }
        }
        _ => {
            let native = rustls_native_certs::load_native_certs();
            let (added, _) = roots.add_parsable_certificates(native.certs);
            if added == 0 {
                return Err(TlsError::NoSystemRoots);
            }
        }
    }
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| TlsError::Setup(e.to_string()))?
    .with_root_certificates(roots);
    match loaded.client {
        Some(client) => {
            let key_path = tls
                .client
                .as_ref()
                .map(|(_, key)| key.clone())
                .unwrap_or_default();
            builder
                .with_client_auth_cert(client.chain, client.key)
                .map_err(|e| TlsError::Invalid {
                    path: key_path,
                    message: e.to_string(),
                })
        }
        None => Ok(builder.with_no_client_auth()),
    }
}

/// A blocking HTTP client for the OTLP/HTTP exporters over TLS, with the
/// export timeout. Built on a thread of its own: reqwest's blocking client
/// must not be created inside an async runtime.
pub fn http_client(tls: &Tls) -> Result<reqwest::blocking::Client, TlsError> {
    let config = rustls_config(tls)?;
    let timeout = tls.timeout;
    std::thread::Builder::new()
        .name("agentfs-otlp-client".into())
        .spawn(move || {
            reqwest::blocking::Client::builder()
                .timeout(timeout)
                .tls_backend_preconfigured(config)
                .build()
        })
        .map_err(|e| TlsError::Setup(e.to_string()))?
        .join()
        .map_err(|_| TlsError::Setup("building the HTTP client panicked".into()))?
        .map_err(|e| TlsError::Setup(e.to_string()))
}

/// tonic's TLS settings for the gRPC exporter: the same trust and client
/// certificate as [`http_client`].
pub fn grpc_config(tls: &Tls) -> Result<tonic::transport::ClientTlsConfig, TlsError> {
    // Checked with rustls first, so a bad file fails here, not at export.
    rustls_config(tls)?;
    let loaded = load(tls)?;
    let mut config = tonic::transport::ClientTlsConfig::new();
    config = match loaded.ca {
        Some((pem, _)) => config.ca_certificate(tonic::transport::Certificate::from_pem(pem)),
        None => config.with_native_roots(),
    };
    if let Some(client) = loaded.client {
        config = config.identity(tonic::transport::Identity::from_pem(
            client.cert_pem,
            client.key_pem,
        ));
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::Duration;

    use super::*;

    /// Certificates from ci/otel-certs.sh.
    fn certs() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ci/otel-certs.sh");
        let status = Command::new("bash")
            .arg(script)
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    fn tls(ca: Option<PathBuf>, client: Option<(PathBuf, PathBuf)>) -> Tls {
        Tls {
            ca,
            client,
            timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn good_files_build_both_clients() {
        let dir = certs();
        let p = |name: &str| dir.path().join(name);
        for settings in [
            tls(Some(p("ca.pem")), None),
            tls(Some(p("ca.pem")), Some((p("client.pem"), p("client.key")))),
        ] {
            http_client(&settings).unwrap();
            grpc_config(&settings).unwrap();
        }
    }

    #[test]
    fn bad_files_are_named() {
        let dir = certs();
        let p = |name: &str| dir.path().join(name);
        std::fs::write(p("empty.pem"), "").unwrap();
        let err = |settings: Tls| http_client(&settings).unwrap_err().to_string();

        let e = err(tls(Some(p("missing.pem")), None));
        assert!(
            e.starts_with("cannot read ") && e.contains("missing.pem"),
            "{e}"
        );
        let e = err(tls(Some(p("empty.pem")), None));
        assert!(e.contains("empty.pem holds no PEM certificate"), "{e}");
        let e = err(tls(
            Some(p("ca.pem")),
            Some((p("client.pem"), p("empty.pem"))),
        ));
        assert!(e.contains("empty.pem holds no PEM private key"), "{e}");
        // The key of another certificate.
        let e = err(tls(
            Some(p("ca.pem")),
            Some((p("client.pem"), p("server.key"))),
        ));
        assert!(e.contains("server.key"), "{e}");
        assert!(grpc_config(&tls(Some(p("empty.pem")), None)).is_err());
    }
}
