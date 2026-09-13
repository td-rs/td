//! Port of `tdnet/td/net/SslCtx.{h,cpp}` + `SslStream.{h,cpp}` on rustls.
//!
//! `SslCtx::create(cert_file, verify_peer)` keeps its C++ shape: an empty
//! `cert_file` selects the default store, a non-empty one is loaded as the
//! only trust anchor set (the C++ `SSL_CTX_load_verify_locations` path),
//! and `VerifyPeer::Off` accepts any certificate
//! (`SSL_VERIFY_NONE`). [`connect`] replaces the `SslStream` actor with an
//! async TLS client handshake over an already-connected stream, honoring
//! `use_ip_address_as_host` (SNI = the literal address, the MTProto
//! over-TLS transport setup).
//!
//! Documented deviations from the C++:
//! * OpenSSL → rustls; the system CA store is replaced by the pinned
//!   `webpki-roots` bundle (the plan's "pinned CA store" — deterministic,
//!   build-time, wasm-ready). C++ additionally caches the system store in
//!   the binlog (td/telegram); that cache belongs to a Track I step.
//! * The `ByteFlow` interface collapses onto the tokio stream itself;
//!   sync `flow_read`/`flow_write` have no async counterpart (the same
//!   reasoning as the S46 `BufferedUdp` port).
//! * `SslCtx::init_openssl` has no rustls counterpart.

use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use tdutils::status::{Result, Status};

use crate::transparent::status_from_io;

/// `SslCtx::VerifyPeer` — the canonical home (re-exported by [`crate::wget`]
/// for its `WgetParams`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VerifyPeer {
    /// Verify the server certificate against the store.
    #[default]
    On,
    /// Accept any certificate (`SslCtx::VerifyPeer::Off`).
    Off,
}

/// `td::SslCtx` — a client TLS configuration; cheap to clone (the C++
/// `shared_ptr<SSL_CTX>` semantics).
#[derive(Clone)]
pub struct SslCtx {
    connector: TlsConnector,
}

impl SslCtx {
    /// `SslCtx::create(cert_file, verify_peer)`.
    pub fn create(cert_file: &str, verify_peer: VerifyPeer) -> Result<Self> {
        // An explicit provider: the dependency graph may enable several
        // rustls crypto backends (reqwest pulls aws-lc-rs), which defeats
        // rustls's process-level auto-detection.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("supported protocol versions");
        let config = match verify_peer {
            VerifyPeer::On => {
                let root_store = if cert_file.is_empty() {
                    // The pinned CA store.
                    let mut roots = rustls::RootCertStore::empty();
                    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                    roots
                } else {
                    match load_cert_file(cert_file) {
                        Result::Ok(roots) => roots,
                        Result::Err(error) => return Result::Err(error),
                    }
                };
                builder
                    .with_root_certificates(root_store)
                    .with_no_client_auth()
            }
            VerifyPeer::Off => {
                // rustls always verifies something; `SSL_VERIFY_NONE` is
                // modeled by the danger verifier that accepts everything.
                let config = builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoServerCertVerifier(provider)))
                    .with_no_client_auth();
                return Result::Ok(Self {
                    connector: TlsConnector::from(Arc::new(config)),
                });
            }
        };
        Result::Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
        })
    }
}

/// The C++ `SSL_CTX_load_verify_locations` path, PEM-encoded.
fn load_cert_file(cert_file: &str) -> Result<rustls::RootCertStore> {
    use rustls::pki_types::pem::PemObject;

    let invalid_file = || Status::error_message("Failed to set custom certificate file");
    let Ok(iter) = rustls::pki_types::CertificateDer::pem_file_iter(cert_file) else {
        return Result::Err(invalid_file());
    };
    let parsed: std::result::Result<Vec<_>, _> = iter.collect();
    let Ok(certs) = parsed else {
        return Result::Err(invalid_file());
    };
    if certs.is_empty() {
        // C++ SSL_CTX_load_verify_locations fails on a file with no
        // loadable certificates.
        return Result::Err(invalid_file());
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        match roots.add(cert) {
            std::result::Result::Ok(()) => {}
            std::result::Result::Err(_) => {
                return Result::Err(Status::error_message("Failed to add certificate"));
            }
        }
    }
    Result::Ok(roots)
}

/// `SSL_VERIFY_NONE` — accept any certificate and any signature.
#[derive(Debug)]
struct NoServerCertVerifier(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// `SslStream::create(host, ssl_ctx, use_ip_address_as_host)` — the TLS
/// client handshake. The returned stream replaces the C++
/// `read_byte_flow`/`write_byte_flow` pair.
pub async fn connect(
    stream: TcpStream,
    host: &str,
    _use_ip_address_as_host: bool,
    ssl_ctx: &SslCtx,
) -> Result<TlsStream<TcpStream>> {
    // rustls treats IP-literal hosts as IP `ServerName`s and omits SNI
    // for them, so `use_ip_address_as_host` needs no separate handling.
    let Ok(server_name) = rustls::pki_types::ServerName::try_from(host.to_string()) else {
        return Result::Err(Status::error_message("Failed to set SNI host name"));
    };
    match ssl_ctx.connector.connect(server_name, stream).await {
        std::result::Result::Ok(tls_stream) => Result::Ok(tls_stream),
        std::result::Result::Err(error) => Result::Err(status_from_io(&error)),
    }
}
