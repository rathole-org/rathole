use crate::config::{TlsConfig, TransportConfig};
use crate::helper::host_port_pair;
use crate::transport::{AddrMaybeCached, SocketOpts, TcpTransport, Transport};
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
    connector: Option<TlsConnector>,
    tls_acceptor: Option<TlsAcceptor>,
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

fn load_server_config(config: &TlsConfig) -> Result<Option<ServerConfig>> {
    if let Some(pkcs12_path) = config.pkcs12.as_ref() {
        let buf = fs::read(pkcs12_path)?;
        let pfx = PFX::parse(buf.as_slice())?;
        let pass = config.pkcs12_password.as_ref().unwrap();

        let certs = pfx.cert_bags(pass)?;
        let keys = pfx.key_bags(pass)?;

        let chain: Vec<CertificateDer> = certs.into_iter().map(CertificateDer::from).collect();
        let key = PrivatePkcs8KeyDer::from(keys.into_iter().next().unwrap());

        Ok(Some(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(chain, key.into())?,
        ))
    } else {
        Ok(None)
    }
}

fn load_client_config(config: &TlsConfig) -> Result<Option<ClientConfig>> {
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
            // allow missing client root_certs (old behaviour)
            return Ok(None);
        }

        for cert in native.certs {
            // Some certs may fail parsing into the store
            root_certs.add(cert).context("Failed to add native cert")?;
        }
    }

    Ok(Some(
        ClientConfig::builder()
            .with_root_certificates(root_certs)
            .with_no_client_auth(),
    ))
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

        let connector = load_client_config(config)
            .unwrap()
            .map(|c| Arc::new(c).into());
        let tls_acceptor = load_server_config(config)
            .unwrap()
            .map(|c| Arc::new(c).into());

        Ok(TlsTransport {
            tcp,
            config: config.clone(),
            connector,
            tls_acceptor,
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
        let conn = self.tls_acceptor.as_ref().unwrap().accept(conn).await?;
        Ok(tokio_rustls::TlsStream::Server(conn))
    }

    async fn connect(&self, addr: &AddrMaybeCached) -> Result<Self::Stream> {
        let conn = self.tcp.connect(addr).await?;

        let connector = self.connector.as_ref().unwrap();

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
    &s.get_ref().0
}
