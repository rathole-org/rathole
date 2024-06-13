use crate::config::{TlsConfig, TransportConfig};
use crate::helper::host_port_pair;
use crate::transport::{AddrMaybeCached, SocketOpts, TcpTransport, Transport};
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
    connector: Option<TlsConnector>,
    tls_acceptor: Option<TlsAcceptor>,
}

#[async_trait]
impl Transport for TlsTransport {
    type Acceptor = TcpListener;
    type RawStream = TcpStream;
    type Stream = TlsStream<TcpStream>;

    fn new(config: &TransportConfig) -> Result<Self> {
        let tcp = TcpTransport::new(config)?;
        let config = config
            .tls
            .as_ref()
            .ok_or_else(|| anyhow!("Missing tls config"))?;

        let connector = match config.trusted_root.as_ref() {
            Some(path) => {
                let s = fs::read_to_string(path)
                    .with_context(|| "Failed to read the `tls.trusted_root`")?;
                let cert = Certificate::from_pem(s.as_bytes())
                    .with_context(|| "Failed to read certificate from `tls.trusted_root`")?;
                let connector = native_tls::TlsConnector::builder()
                    .add_root_certificate(cert)
                    .build()?;
                Some(TlsConnector::from(connector))
            }
            None => {
                // if no trusted_root is specified, allow TlsConnector to use system default
                let connector = native_tls::TlsConnector::builder().build()?;
                Some(TlsConnector::from(connector))
            }
        };

        let tls_acceptor = match config.pkcs12.as_ref() {
            Some(path) => {
                let ident = Identity::from_pkcs12(
                    &fs::read(path)?,
                    config.pkcs12_password.as_ref().unwrap(),
                )
                .with_context(|| "Failed to create identitiy")?;
                Some(TlsAcceptor::from(
                    native_tls::TlsAcceptor::new(ident).unwrap(),
                ))
            }
            None => None,
        };

        Ok(TlsTransport {
            tcp,
            config: config.clone(),
            connector,
            tls_acceptor,
        })
    }

    fn hint(conn: &Self::Stream, opt: SocketOpts) {
        opt.apply(conn.get_ref().get_ref().get_ref());
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
        let conn = self.tls_acceptor.as_ref().unwrap().accept(conn).await?;
        Ok(conn)
    }

    async fn connect(&self, addr: &AddrMaybeCached) -> Result<Self::Stream> {
        let conn = self.tcp.connect(addr).await?;

        let connector = self.connector.as_ref().unwrap();
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
