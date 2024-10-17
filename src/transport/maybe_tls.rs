use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

use super::tls::get_tcpstream;
use super::tls::TlsStream;
use super::{AddrMaybeCached, SocketOpts, TlsTransport};
use super::{TcpTransport, Transport};
use crate::config::TransportConfig;

#[derive(Debug)]
pub(super) enum MaybeTLSStream {
    No(TcpStream),
    Yes(TlsStream<TcpStream>),
}

impl MaybeTLSStream {
    pub(super) fn get_tcpstream(&self) -> &TcpStream {
        match self {
            MaybeTLSStream::No(s) => s,
            MaybeTLSStream::Yes(s) => get_tcpstream(s),
        }
    }
}

impl AsyncRead for MaybeTLSStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MaybeTLSStream::No(s) => Pin::new(s).poll_read(cx, buf),
            MaybeTLSStream::Yes(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTLSStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            MaybeTLSStream::No(s) => Pin::new(s).poll_write(cx, buf),
            MaybeTLSStream::Yes(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            MaybeTLSStream::No(s) => Pin::new(s).poll_flush(cx),
            MaybeTLSStream::Yes(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            MaybeTLSStream::No(s) => Pin::new(s).poll_shutdown(cx),
            MaybeTLSStream::Yes(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[derive(Debug)]
pub(super) enum MaybeTLSTransport {
    Yes(TlsTransport),
    No(TcpTransport),
}

impl MaybeTLSTransport {
    pub(super) fn new_explicit(tls: bool, tconfig: &TransportConfig) -> anyhow::Result<Self> {
        match tls {
            true => Ok(MaybeTLSTransport::Yes(TlsTransport::new(tconfig)?)),
            false => Ok(MaybeTLSTransport::No(TcpTransport::new(tconfig)?)),
        }
    }
}

#[async_trait]
impl Transport for MaybeTLSTransport {
    type Acceptor = TcpListener;
    type RawStream = TcpStream;
    type Stream = MaybeTLSStream;

    fn new(config: &TransportConfig) -> anyhow::Result<Self> {
        MaybeTLSTransport::new_explicit(config.tls.is_some(), config)
    }

    async fn bind<A: ToSocketAddrs + Send + Sync>(
        &self,
        addr: A,
    ) -> anyhow::Result<Self::Acceptor> {
        match self {
            MaybeTLSTransport::Yes(t) => t.bind(addr).await,
            MaybeTLSTransport::No(t) => t.bind(addr).await,
        }
    }

    fn hint(conn: &Self::Stream, opt: SocketOpts) {
        match conn {
            MaybeTLSStream::Yes(t) => TlsTransport::hint(t, opt),
            MaybeTLSStream::No(t) => TcpTransport::hint(t, opt),
        }
    }

    async fn accept(&self, a: &Self::Acceptor) -> anyhow::Result<(Self::RawStream, SocketAddr)> {
        match self {
            MaybeTLSTransport::Yes(t) => t.accept(a).await,
            MaybeTLSTransport::No(t) => t.accept(a).await,
        }
    }

    async fn handshake(&self, conn: Self::RawStream) -> anyhow::Result<Self::Stream> {
        match self {
            MaybeTLSTransport::Yes(t) => {
                let stream = t.handshake(conn).await?;
                Ok(MaybeTLSStream::Yes(stream))
            }
            MaybeTLSTransport::No(t) => {
                let stream = t.handshake(conn).await?;
                Ok(MaybeTLSStream::No(stream))
            }
        }
    }

    async fn connect(&self, addr: &AddrMaybeCached) -> anyhow::Result<Self::Stream> {
        match self {
            MaybeTLSTransport::Yes(t) => {
                let stream = t.connect(addr).await?;
                Ok(MaybeTLSStream::Yes(stream))
            }
            MaybeTLSTransport::No(t) => {
                let stream = t.connect(addr).await?;
                Ok(MaybeTLSStream::No(stream))
            }
        }
    }
}
