use crate::config::{TlsConfig, TransportConfig};
use crate::helper::host_port_pair;
use crate::transport::{AddrMaybeCached, SocketOpts, TcpTransport, Transport, TransportRole};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use std::fs;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_native_tls::native_tls::{self, Certificate, Identity};
pub(crate) use tokio_native_tls::TlsStream;
use tokio_native_tls::{TlsAcceptor, TlsConnector};

#[derive(Debug)]
pub struct TlsTransport {
    tcp: TcpTransport,
    config: TlsConfig,
    endpoint: TlsEndpoint,
}

#[derive(Debug)]
enum TlsEndpoint {
    Client(TlsConnector),
    Server(TlsAcceptor),
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
                let connector = match config.trusted_root.as_ref() {
                    Some(path) => {
                        let s = fs::read_to_string(path)
                            .with_context(|| "Failed to read the `tls.trusted_root`")?;
                        let cert = Certificate::from_pem(s.as_bytes()).with_context(|| {
                            "Failed to read certificate from `tls.trusted_root`"
                        })?;
                        native_tls::TlsConnector::builder()
                            .add_root_certificate(cert)
                            .build()
                            .context("Failed to create TLS connector")?
                    }
                    None => {
                        // If no trusted_root is specified, use the system defaults.
                        native_tls::TlsConnector::builder()
                            .build()
                            .context("Failed to create TLS connector from system roots")?
                    }
                };
                TlsEndpoint::Client(TlsConnector::from(connector))
            }
            TransportRole::Server => {
                let path = config.pkcs12.as_ref().context("Missing `tls.pkcs12`")?;
                let password = config
                    .pkcs12_password
                    .as_ref()
                    .context("Missing `tls.pkcs12_password`")?;
                let identity = fs::read(path).context("Failed to read `tls.pkcs12`")?;
                let ident = Identity::from_pkcs12(&identity, password)
                    .with_context(|| "Failed to create identity")?;
                TlsEndpoint::Server(TlsAcceptor::from(
                    native_tls::TlsAcceptor::new(ident).context("Failed to create TLS acceptor")?,
                ))
            }
        };

        Ok(TlsTransport {
            tcp,
            config: config.clone(),
            endpoint,
        })
    }

    fn hint(conn: &Self::Stream, opt: SocketOpts) {
        opt.apply(conn.get_ref().get_ref().get_ref());
    }

    async fn bind<A: ToSocketAddrs + Send + Sync>(&self, addr: A) -> Result<Self::Acceptor> {
        self.tcp
            .bind(addr)
            .await
            .with_context(|| "Failed to create tcp listener")
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
        Ok(conn)
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

        Ok(connector
            .connect(
                self.config
                    .hostname
                    .as_deref()
                    .unwrap_or(host_port_pair(&addr.addr)?.0),
                conn,
            )
            .await?)
    }
}

#[cfg(feature = "websocket-native-tls")]
pub(crate) fn get_tcpstream(s: &TlsStream<TcpStream>) -> &TcpStream {
    s.get_ref().get_ref().get_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TlsConfig, TransportType};
    use std::path::Path;

    fn server_transport(identity_path: &Path) -> TransportConfig {
        TransportConfig {
            transport_type: TransportType::Tls,
            tls: Some(TlsConfig {
                hostname: None,
                trusted_root: None,
                pkcs12: Some(identity_path.to_string_lossy().into_owned()),
                pkcs12_password: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn missing_identity_password_returns_error() -> Result<()> {
        let identity = tempfile::NamedTempFile::new()?;
        let result = TlsTransport::new(&server_transport(identity.path()), TransportRole::Server);

        let error = result.expect_err("missing PKCS#12 password should fail");
        assert!(error.to_string().contains("tls.pkcs12_password"));
        Ok(())
    }
}
