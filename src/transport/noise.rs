use std::net::SocketAddr;

use super::{AddrMaybeCached, SocketOpts, TcpTransport, Transport};
use crate::config::{NoiseConfig, TransportConfig};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use snowstorm::{Builder, NoiseParams, NoiseStream};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

pub struct NoiseTransport {
    tcp: TcpTransport,
    config: NoiseConfig,
    params: NoiseParams,
    local_private_key: Vec<u8>,
    remote_public_key: Option<Vec<u8>>,
}

impl std::fmt::Debug for NoiseTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{:?}", self.config)
    }
}

impl NoiseTransport {
    fn builder(&self) -> Builder<'_> {
        let builder = Builder::new(self.params.clone()).local_private_key(&self.local_private_key);
        match &self.remote_public_key {
            Some(x) => builder.remote_public_key(x),
            None => builder,
        }
    }
}

#[async_trait]
impl Transport for NoiseTransport {
    type Acceptor = TcpListener;
    type RawStream = TcpStream;
    type Stream = snowstorm::stream::NoiseStream<TcpStream>;

    fn new(config: &TransportConfig) -> Result<Self> {
        let tcp = TcpTransport::new(config)?;

        let config = match &config.noise {
            Some(v) => v.clone(),
            None => return Err(anyhow!("Missing noise config")),
        };
        let builder = Builder::new(config.pattern.parse()?);

        let remote_public_key = match &config.remote_public_key {
            Some(x) => {
                Some(base64::decode(x).with_context(|| "Failed to decode remote_public_key")?)
            }
            None => None,
        };

        let local_private_key = match &config.local_private_key {
            Some(x) => base64::decode(x.as_bytes())
                .with_context(|| "Failed to decode local_private_key")?,
            None => builder.generate_keypair()?.private,
        };

        let params: NoiseParams = config.pattern.parse()?;

        Ok(NoiseTransport {
            tcp,
            config,
            params,
            local_private_key,
            remote_public_key,
        })
    }

    fn hint(conn: &Self::Stream, opt: SocketOpts) {
        opt.apply(conn.get_inner());
    }

    async fn bind<T: ToSocketAddrs + Send + Sync>(&self, addr: T) -> Result<Self::Acceptor> {
        Ok(TcpListener::bind(addr).await?)
    }

    async fn accept(&self, a: &Self::Acceptor) -> Result<(Self::RawStream, SocketAddr)> {
        self.tcp
            .accept(a)
            .await
            .with_context(|| "Failed to accept TCP connection")
    }

    async fn handshake(&self, conn: Self::RawStream) -> Result<Self::Stream> {
        let conn = NoiseStream::handshake(conn, self.builder().build_responder()?)
            .await
            .with_context(|| "Failed to do noise handshake")?;
        Ok(conn)
    }

    async fn connect(&self, addr: &AddrMaybeCached) -> Result<Self::Stream> {
        let conn = self
            .tcp
            .connect(addr)
            .await
            .with_context(|| "Failed to connect TCP socket")?;

        let conn = NoiseStream::handshake(conn, self.builder().build_initiator()?)
            .await
            .with_context(|| "Failed to do noise handshake")?;
        return Ok(conn);
    }
}
