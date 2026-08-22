use crate::config::{TlsConfig, TransportConfig};
use crate::helper::host_port_pair;
use crate::transport::{AddrMaybeCached, SocketOpts, TcpTransport, Transport, TransportRole};
use std::fmt::Debug;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
pub(crate) use tokio_rustls::TlsStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use p12::PFX;

pub struct TlsTransport {
    tcp: TcpTransport,
    config: TlsConfig,
    endpoint: TlsEndpoint,
}

enum TlsEndpoint {
    Client(TlsConnector),
    Server(TlsAcceptor),
}

// workaround for TlsConnector and TlsAcceptor not implementing Debug
impl Debug for TlsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsTransport")
            .field("tcp", &self.tcp)
            .field("config", &self.config)
            .finish()
    }
}

fn load_server_config(config: &TlsConfig) -> Result<ServerConfig> {
    let pkcs12_path = config.pkcs12.as_ref().context("Missing `tls.pkcs12`")?;
    let pass = config
        .pkcs12_password
        .as_ref()
        .context("Missing `tls.pkcs12_password`")?;
    let buf = fs::read(pkcs12_path).context("Failed to read `tls.pkcs12`")?;
    let pfx = PFX::parse(buf.as_slice())?;

    let certs = pfx.cert_bags(pass)?;
    let keys = pfx.key_bags(pass)?;

    let chain: Vec<CertificateDer> = certs.into_iter().map(CertificateDer::from).collect();
    let key = keys
        .into_iter()
        .next()
        .map(PrivatePkcs8KeyDer::from)
        .context("PKCS#12 identity contains no private key")?;

    Ok(ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key.into())?)
}

fn load_client_config(config: &TlsConfig) -> Result<ClientConfig> {
    let mut root_certs = RootCertStore::empty();

    if let Some(path) = config.trusted_root.as_deref() {
        // Parse CERTIFICATE blocks from PEM using rustls-pki-types
        let iter = CertificateDer::pem_file_iter(path).with_context(|| {
            format!(
                "Failed to open/read certificate file {}",
                Path::new(path).display()
            )
        })?;

        let mut added_any = false;
        for cert in iter {
            let cert = cert?; // pem::Error -> anyhow
            root_certs.add(cert.into_owned())?; // add expects owned DER
            added_any = true;
        }

        if !added_any {
            anyhow::bail!(
                "No CERTIFICATE entries found in PEM file {}",
                Path::new(path).display()
            );
        }
    } else {
        // New rustls-native-certs API: CertificateResult { certs, errors }
        let native = rustls_native_certs::load_native_certs();

        for err in &native.errors {
            eprintln!("Failed to load some native certs: {err}");
        }

        if native.certs.is_empty() {
            anyhow::bail!("No trusted root certificates found in the system certificate store");
        }

        for cert in native.certs {
            // Some certs may fail parsing into the store
            root_certs.add(cert).context("Failed to add native cert")?;
        }
    }

    Ok(ClientConfig::builder()
        .with_root_certificates(root_certs)
        .with_no_client_auth())
}

#[async_trait]
impl Transport for TlsTransport {
    type Acceptor = TcpListener;
    type RawStream = TcpStream;
    type Stream = TlsStream<TcpStream>;

    fn new(config: &TransportConfig, role: TransportRole) -> Result<Self> {
        let tcp = TcpTransport::new(config, role)?;
        let config = config
            .tls
            .as_ref()
            .ok_or_else(|| anyhow!("Missing tls config"))?;

        let endpoint = match role {
            TransportRole::Client => {
                TlsEndpoint::Client(Arc::new(load_client_config(config)?).into())
            }
            TransportRole::Server => {
                TlsEndpoint::Server(Arc::new(load_server_config(config)?).into())
            }
        };

        Ok(TlsTransport {
            tcp,
            config: config.clone(),
            endpoint,
        })
    }

    fn hint(conn: &Self::Stream, opt: SocketOpts) {
        opt.apply(conn.get_ref().0);
    }

    async fn bind<A: ToSocketAddrs + Send + Sync>(&self, addr: A) -> Result<Self::Acceptor> {
        let l = TcpListener::bind(addr)
            .await
            .with_context(|| "Failed to create tcp listener")?;
        Ok(l)
    }

    async fn accept(&self, a: &Self::Acceptor) -> Result<(Self::RawStream, SocketAddr)> {
        self.tcp
            .accept(a)
            .await
            .with_context(|| "Failed to accept TCP connection")
    }

    async fn handshake(&self, conn: Self::RawStream) -> Result<Self::Stream> {
        let acceptor = match &self.endpoint {
            TlsEndpoint::Server(acceptor) => acceptor,
            TlsEndpoint::Client(_) => {
                return Err(anyhow!(
                    "Client TLS transport cannot perform a server handshake"
                ));
            }
        };
        let conn = acceptor
            .accept(conn)
            .await
            .context("Failed to accept TLS connection")?;
        Ok(tokio_rustls::TlsStream::Server(conn))
    }

    async fn connect(&self, addr: &AddrMaybeCached) -> Result<Self::Stream> {
        let connector = match &self.endpoint {
            TlsEndpoint::Client(connector) => connector,
            TlsEndpoint::Server(_) => {
                return Err(anyhow!(
                    "Server TLS transport cannot perform a client connection"
                ));
            }
        };
        let conn = self.tcp.connect(addr).await?;

        let host_name = self
            .config
            .hostname
            .as_deref()
            .unwrap_or(host_port_pair(&addr.addr)?.0);

        Ok(tokio_rustls::TlsStream::Client(
            connector
                .connect(ServerName::try_from(host_name)?.to_owned(), conn)
                .await?,
        ))
    }
}

pub(crate) fn get_tcpstream(s: &TlsStream<TcpStream>) -> &TcpStream {
    s.get_ref().0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MaskedString, TransportType};

    fn server_transport(identity_path: &Path, password: Option<&str>) -> TransportConfig {
        TransportConfig {
            transport_type: TransportType::Tls,
            tls: Some(TlsConfig {
                hostname: None,
                trusted_root: None,
                pkcs12: Some(identity_path.to_string_lossy().into_owned()),
                pkcs12_password: password.map(MaskedString::from),
            }),
            ..Default::default()
        }
    }

    fn client_transport(trusted_root_path: &Path) -> TransportConfig {
        TransportConfig {
            transport_type: TransportType::Tls,
            tls: Some(TlsConfig {
                hostname: None,
                trusted_root: Some(trusted_root_path.to_string_lossy().into_owned()),
                pkcs12: None,
                pkcs12_password: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn malformed_trusted_root_returns_error() -> Result<()> {
        let trusted_root = tempfile::NamedTempFile::new()?;
        fs::write(trusted_root.path(), b"not a PEM certificate")?;

        let result = TlsTransport::new(
            &client_transport(trusted_root.path()),
            TransportRole::Client,
        );
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn malformed_identity_returns_error() -> Result<()> {
        let identity = tempfile::NamedTempFile::new()?;
        fs::write(identity.path(), b"not a PKCS#12 identity")?;

        let result = TlsTransport::new(
            &server_transport(identity.path(), Some("password")),
            TransportRole::Server,
        );
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn missing_identity_password_returns_error() -> Result<()> {
        let identity = tempfile::NamedTempFile::new()?;
        let result = TlsTransport::new(
            &server_transport(identity.path(), None),
            TransportRole::Server,
        );

        let error = result.expect_err("missing PKCS#12 password should fail");
        assert!(error.to_string().contains("tls.pkcs12_password"));
        Ok(())
    }
}
