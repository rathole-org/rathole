use core::result::Result;
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use super::{AddrMaybeCached, SocketOpts, TcpTransport, TlsTransport, Transport, TransportRole};
use crate::config::TransportConfig;
use anyhow::{anyhow, Context as _};
use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::Stream;
use futures_sink::Sink;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

#[cfg(any(feature = "native-tls", feature = "rustls"))]
use super::tls::get_tcpstream;
#[cfg(any(feature = "native-tls", feature = "rustls"))]
use super::tls::TlsStream;

use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::{accept_async_with_config, client_async_with_config, WebSocketStream};
use tokio_util::io::StreamReader;
use url::Url;

#[derive(Debug)]
enum TransportStream {
    Insecure(TcpStream),
    Secure(Box<TlsStream<TcpStream>>),
}

impl TransportStream {
    fn get_tcpstream(&self) -> &TcpStream {
        match self {
            TransportStream::Insecure(s) => s,
            TransportStream::Secure(s) => get_tcpstream(s),
        }
    }
}

impl AsyncRead for TransportStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TransportStream::Insecure(s) => Pin::new(s).poll_read(cx, buf),
            TransportStream::Secure(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TransportStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            TransportStream::Insecure(s) => Pin::new(s).poll_write(cx, buf),
            TransportStream::Secure(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            TransportStream::Insecure(s) => Pin::new(s).poll_flush(cx),
            TransportStream::Secure(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            TransportStream::Insecure(s) => Pin::new(s).poll_shutdown(cx),
            TransportStream::Secure(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[derive(Debug)]
struct StreamWrapper {
    inner: WebSocketStream<TransportStream>,
}

impl Stream for StreamWrapper {
    type Item = Result<Bytes, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.get_mut().inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(Error::other(err)))),
            Poll::Ready(Some(Ok(res))) => {
                if let Message::Binary(b) = res {
                    Poll::Ready(Some(Ok(Bytes::from(b))))
                } else {
                    Poll::Ready(Some(Err(Error::new(
                        ErrorKind::InvalidData,
                        "unexpected frame",
                    ))))
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

#[derive(Debug)]
pub struct WebsocketTunnel {
    inner: StreamReader<StreamWrapper, Bytes>,
}

impl AsyncRead for WebsocketTunnel {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncBufRead for WebsocketTunnel {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
        Pin::new(&mut self.get_mut().inner).poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        Pin::new(&mut self.get_mut().inner).consume(amt)
    }
}

impl AsyncWrite for WebsocketTunnel {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let sw = self.get_mut().inner.get_mut();
        ready!(Pin::new(&mut sw.inner).poll_ready(cx).map_err(Error::other))?;

        match Pin::new(&mut sw.inner).start_send(Message::Binary(buf.to_vec())) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(e) => Poll::Ready(Err(Error::other(e))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.get_mut().inner.get_mut().inner)
            .poll_flush(cx)
            .map_err(Error::other)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.get_mut().inner.get_mut().inner)
            .poll_close(cx)
            .map_err(Error::other)
    }
}

#[derive(Debug)]
enum SubTransport {
    Secure(TlsTransport),
    Insecure(TcpTransport),
}

#[derive(Debug)]
pub struct WebsocketTransport {
    sub: SubTransport,
    conf: WebSocketConfig,
}

#[async_trait]
impl Transport for WebsocketTransport {
    type Acceptor = TcpListener;
    type RawStream = TcpStream;
    type Stream = WebsocketTunnel;

    fn new(config: &TransportConfig, role: TransportRole) -> anyhow::Result<Self> {
        let wsconfig = config
            .websocket
            .as_ref()
            .ok_or_else(|| anyhow!("Missing websocket config"))?;

        let conf = WebSocketConfig {
            write_buffer_size: 0,
            ..WebSocketConfig::default()
        };
        let sub = match wsconfig.tls {
            true => SubTransport::Secure(TlsTransport::new(config, role)?),
            false => SubTransport::Insecure(TcpTransport::new(config, role)?),
        };
        Ok(WebsocketTransport { sub, conf })
    }

    fn hint(conn: &Self::Stream, opt: SocketOpts) {
        opt.apply(conn.inner.get_ref().inner.get_ref().get_tcpstream())
    }

    async fn bind<A: ToSocketAddrs + Send + Sync>(
        &self,
        addr: A,
    ) -> anyhow::Result<Self::Acceptor> {
        TcpListener::bind(addr).await.map_err(Into::into)
    }

    async fn accept(&self, a: &Self::Acceptor) -> anyhow::Result<(Self::RawStream, SocketAddr)> {
        let (s, addr) = match &self.sub {
            SubTransport::Insecure(t) => t.accept(a).await?,
            SubTransport::Secure(t) => t.accept(a).await?,
        };
        Ok((s, addr))
    }

    async fn handshake(&self, conn: Self::RawStream) -> anyhow::Result<Self::Stream> {
        let tsream = match &self.sub {
            SubTransport::Insecure(t) => TransportStream::Insecure(t.handshake(conn).await?),
            SubTransport::Secure(t) => TransportStream::Secure(Box::new(t.handshake(conn).await?)),
        };
        let wsstream = accept_async_with_config(tsream, Some(self.conf)).await?;
        let tun = WebsocketTunnel {
            inner: StreamReader::new(StreamWrapper { inner: wsstream }),
        };
        Ok(tun)
    }

    async fn connect(&self, addr: &AddrMaybeCached) -> anyhow::Result<Self::Stream> {
        let u = format!("ws://{}", addr.addr.as_str());
        let url = Url::parse(&u).with_context(|| format!("Invalid WebSocket URL `{u}`"))?;
        let tstream = match &self.sub {
            SubTransport::Insecure(t) => TransportStream::Insecure(t.connect(addr).await?),
            SubTransport::Secure(t) => TransportStream::Secure(Box::new(t.connect(addr).await?)),
        };
        let (wsstream, _) = client_async_with_config(url, tstream, Some(self.conf))
            .await
            .context("Failed to complete WebSocket client handshake")?;
        let tun = WebsocketTunnel {
            inner: StreamReader::new(StreamWrapper { inner: wsstream }),
        };
        Ok(tun)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TlsConfig, TransportType, WebsocketConfig};
    use std::fs;

    fn secure_websocket_transport(tls: TlsConfig) -> TransportConfig {
        TransportConfig {
            transport_type: TransportType::Websocket,
            tls: Some(tls),
            websocket: Some(WebsocketConfig { tls: true }),
            ..Default::default()
        }
    }

    #[test]
    fn secure_websocket_server_without_identity_returns_error() {
        let config = secure_websocket_transport(TlsConfig {
            hostname: None,
            trusted_root: None,
            pkcs12: None,
            pkcs12_password: None,
        });

        let error = WebsocketTransport::new(&config, TransportRole::Server)
            .expect_err("missing server identity should fail during transport construction");
        assert!(error.to_string().contains("tls.pkcs12"));
    }

    #[test]
    fn secure_websocket_client_with_invalid_trust_returns_error() -> anyhow::Result<()> {
        let trusted_root = tempfile::NamedTempFile::new()?;
        fs::write(trusted_root.path(), b"not a PEM certificate")?;
        let config = secure_websocket_transport(TlsConfig {
            hostname: None,
            trusted_root: Some(trusted_root.path().to_string_lossy().into_owned()),
            pkcs12: None,
            pkcs12_password: None,
        });

        WebsocketTransport::new(&config, TransportRole::Client)
            .expect_err("invalid client trust should fail during transport construction");
        Ok(())
    }
}
