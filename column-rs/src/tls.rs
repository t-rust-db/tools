//! `s3-tls` (#125): HTTPS for object stores (AWS S3). db-core speaks plain
//! HTTP only and leaves TLS to the application: this transport opens a
//! rustls connection (ring, Mozilla's roots from `webpki-roots`) and hands
//! it to db-core's `exchange`, which writes the request and parses the
//! response. An `http://` endpoint still goes over db-core's `PlainHttp`.
//!
//! `AWS_CA_BUNDLE`, as the AWS SDKs read it, names a PEM file whose
//! certificates are trusted too: a corporate TLS-inspecting proxy's CA, or
//! a test CA.

use db_core::storage::ingest::store::s3::{
    exchange, HttpRequest, HttpResponse, PlainHttp, Transport,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

/// HTTPS over rustls, plain HTTP through db-core's [`PlainHttp`].
#[derive(Debug)]
pub struct TlsTransport {
    config: Arc<ClientConfig>,
    plain: PlainHttp,
}

impl TlsTransport {
    /// Trusts Mozilla's roots, plus the certificates in the PEM file
    /// `AWS_CA_BUNDLE` names, if it is set.
    pub fn from_env() -> io::Result<TlsTransport> {
        let mut roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        if let Some(bundle) = std::env::var_os("AWS_CA_BUNDLE") {
            let shown = std::path::Path::new(&bundle).display().to_string();
            let bad = |e: &dyn std::fmt::Display| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("AWS_CA_BUNDLE {shown}: {e}"),
                )
            };
            for cert in CertificateDer::pem_file_iter(&bundle).map_err(|e| bad(&e))? {
                roots.add(cert.map_err(|e| bad(&e))?).map_err(|e| bad(&e))?;
            }
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(TlsTransport {
            config: Arc::new(config),
            plain: PlainHttp::default(),
        })
    }
}

impl Transport for TlsTransport {
    fn send(&self, request: &HttpRequest) -> io::Result<HttpResponse> {
        let endpoint = &request.endpoint;
        if !endpoint.https {
            return self.plain.send(request);
        }
        let name = ServerName::try_from(endpoint.host.clone()).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: {e}", endpoint.host),
            )
        })?;
        let connection =
            ClientConnection::new(Arc::clone(&self.config), name).map_err(io::Error::other)?;
        let tcp = TcpStream::connect((endpoint.host.as_str(), endpoint.port))?;
        tcp.set_read_timeout(Some(self.plain.timeout))?;
        tcp.set_write_timeout(Some(self.plain.timeout))?;
        exchange(&mut CloseIsEnd(StreamOwned::new(connection, tcp)), request)
    }
}

/// A TLS stream whose peer closing the connection without a `close_notify`
/// reads as the end of the stream. The request asks for `Connection:
/// close` and servers often just close; a body cut short still fails,
/// because db-core checks it against `Content-Length` or the chunking.
struct CloseIsEnd<S>(S);

impl<S: Read> Read for CloseIsEnd<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
            other => other,
        }
    }
}

impl<S: Write> Write for CloseIsEnd<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
