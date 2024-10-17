use core::result::Result;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{self, Context, Poll};

use anyhow::anyhow;
use async_trait::async_trait;
use bytes::{Buf, Bytes};
use futures_core::Stream;
use http::{Method, Request, Response, Uri};
use http_body_util::StreamBody;
use hyper::body::{Body, Incoming};
use hyper::server::conn::http2 as Server;
use hyper::service::Service;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::client::legacy::Client;
use hyper_util::rt::tokio::TokioExecutor;
use hyper_util::rt::tokio::TokioIo;
use tokio::io::{self, AsyncRead, AsyncWrite, ReadBuf, ReadHalf, SimplexStream, WriteHalf};
use tokio::net::{TcpListener, ToSocketAddrs};
use tokio::select;
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use tokio_util::io::ReaderStream;

use super::maybe_tls::{MaybeTLSStream, MaybeTLSTransport};
use super::{AddrMaybeCached, SocketOpts, Transport};
use crate::config::TransportConfig;

#[derive(Debug)]
struct IncomingHyper {
    inner: hyper::body::Incoming,
    current_chunk: Option<bytes::Bytes>,
}

impl AsyncRead for IncomingHyper {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if let Some(chunk) = &mut self.current_chunk {
                let len = std::cmp::min(chunk.len(), buf.remaining());
                buf.put_slice(&chunk[..len]);

                chunk.advance(len);
                if !chunk.has_remaining() {
                    self.current_chunk = None;
                }

                return Poll::Ready(Ok(()));
            }

            match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Err(err))) => {
                    return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, err)))
                }
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Err(_) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::Other,
                            "non data frame received",
                        )))
                    }
                    Ok(data) => {
                        self.current_chunk = Some(data);
                        continue;
                    }
                },
            }
        }
    }
}

#[derive(Debug)]
pub struct HTTP2Stream {
    send: WriteHalf<SimplexStream>,
    recv: IncomingHyper,
}

impl AsyncRead for HTTP2Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for HTTP2Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.get_mut().send).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.get_mut().send).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.get_mut().send).poll_shutdown(cx)
    }
}

#[derive(Debug)]
pub struct OutgoingSimplex {
    inner: ReaderStream<ReadHalf<SimplexStream>>,
}

impl Stream for OutgoingSimplex {
    type Item = Result<hyper::body::Frame<Bytes>, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.get_mut().inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
            Poll::Ready(Some(Ok(data))) => Poll::Ready(Some(Ok(hyper::body::Frame::data(data)))),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

#[derive(Debug)]
struct Svc {
    req_sender: mpsc::Sender<anyhow::Result<(SocketAddr, Incoming, mpsc::Sender<OutgoingSimplex>)>>,
    addr: SocketAddr,
}

impl Service<Request<Incoming>> for Svc {
    type Response = Response<StreamBody<OutgoingSimplex>>;
    type Error = anyhow::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let req_sender = self.req_sender.clone();
        let addr = self.addr;

        let future = async move {
            let (res_sender, mut res_receiver) = mpsc::channel::<OutgoingSimplex>(1);
            if let Err(err) = req_sender
                .send(Ok((addr, req.into_body(), res_sender)))
                .await
            {
                return Err(anyhow!(err.to_string()));
            }

            match res_receiver.recv().await {
                None => Err(anyhow!("Channel closed")),
                Some(body) => Ok(Response::new(http_body_util::StreamBody::new(body))),
            }
        };

        Box::pin(future) // Return the boxed future
    }
}

async fn start_http_server(
    listener: TcpListener,
    transport: Arc<MaybeTLSTransport>,
    req_sender: mpsc::Sender<anyhow::Result<(SocketAddr, Incoming, mpsc::Sender<OutgoingSimplex>)>>,
    stop_receiver: broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    loop {
        let mut stop_receiver = stop_receiver.resubscribe();
        let conn = async {
            let (conn, addr) = transport.accept(&listener).await?;
            let stream = transport.handshake(conn).await?;
            Ok((stream, addr))
        };
        select! {
            _ = stop_receiver.recv() => {
                return Ok(());
            }

            conn = conn => {
                if let Err(err) = conn {
                    if let Err(err)= req_sender.send(Err(err)).await {
                        eprintln!("Error sending error message: {}", err);
                    }
                    continue
                }

                let (socket, addr) = conn.unwrap();
                let io = TokioIo::new(socket);
                let svc = Svc {
                    req_sender: req_sender.clone(),
                    addr,
                };
                let req_sender= req_sender.clone();
                tokio::spawn(async move {
                    let mut conn =  Server::Builder::new(TokioExecutor::new())
                        .serve_connection(io, svc);
                    select! {
                        _ = stop_receiver.recv() => {
                            Pin::new(&mut conn).graceful_shutdown();
                        }

                        res = &mut conn => {
                            if let Err(err) = res {
                                if let Err(err)= req_sender.send(Err(anyhow!(err.to_string()))).await {
                                    eprintln!("Error sending error message: {}", err);
                                }
                            }
                        }
                    }
                });
            }
        }
    }
}

impl Connection for MaybeTLSStream {
    fn connected(&self) -> Connected {
        let connected = Connected::new();
        if let (Ok(remote_addr), Ok(local_addr)) = (
            self.get_tcpstream().peer_addr(),
            self.get_tcpstream().local_addr(),
        ) {
            connected.extra((remote_addr, local_addr))
        } else {
            connected
        }
    }
}

#[derive(Clone)]
struct MaybeTLSConnector {
    sub: Arc<MaybeTLSTransport>,
}

impl tower_service::Service<Uri> for MaybeTLSConnector {
    type Response = TokioIo<MaybeTLSStream>;
    type Error = anyhow::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, u: Uri) -> Self::Future {
        let sub = self.sub.clone();
        let future = async move {
            let addr = match (u.host(), u.port()) {
                (Some(host), Some(port)) => format!("{}:{}", host, port),
                _ => String::from(""),
            };
            let addr = AddrMaybeCached::new(addr.as_str());
            let stream = sub.connect(&addr).await?;
            Ok(TokioIo::new(stream))
        };
        Box::pin(future)
    }
}

#[derive(Debug)]
pub struct HTTP2Transport {
    sub: Arc<MaybeTLSTransport>,
    client: Client<MaybeTLSConnector, StreamBody<OutgoingSimplex>>,

    stop_sender: broadcast::Sender<()>,
    stop_receiver: broadcast::Receiver<()>,
}

#[async_trait]
impl Transport for HTTP2Transport {
    type Acceptor = Arc<
        Mutex<
            mpsc::Receiver<anyhow::Result<(SocketAddr, Incoming, mpsc::Sender<OutgoingSimplex>)>>,
        >,
    >;
    type RawStream = (Incoming, mpsc::Sender<OutgoingSimplex>);
    type Stream = HTTP2Stream;

    fn new(config: &TransportConfig) -> anyhow::Result<Self> {
        let cfg = config
            .http2
            .as_ref()
            .ok_or_else(|| anyhow!("Missing http2 config"))?;

        let (stop_sender, stop_receiver) = broadcast::channel(1);
        let sub = Arc::new(MaybeTLSTransport::new_explicit(cfg.tls, config)?);
        let client = Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build(MaybeTLSConnector { sub: sub.clone() });

        Ok(HTTP2Transport {
            sub,
            client,
            stop_sender,
            stop_receiver,
        })
    }

    fn hint(_: &Self::Stream, _: SocketOpts) {}

    async fn bind<A: ToSocketAddrs + Send + Sync>(
        &self,
        addr: A,
    ) -> anyhow::Result<Self::Acceptor> {
        let listener = self.sub.bind(addr).await?;

        let (req_sender, req_receiver) = mpsc::channel::<
            anyhow::Result<(SocketAddr, Incoming, mpsc::Sender<OutgoingSimplex>)>,
        >(1);
        let req_receiver = Arc::new(Mutex::new(req_receiver));
        let sub_transport = self.sub.clone();
        let stop_receiver = self.stop_receiver.resubscribe();
        tokio::spawn(start_http_server(
            listener,
            sub_transport,
            req_sender,
            stop_receiver,
        ));
        Ok(req_receiver)
    }

    async fn accept(&self, a: &Self::Acceptor) -> anyhow::Result<(Self::RawStream, SocketAddr)> {
        let mut receiver = a.lock().await;
        match receiver.recv().await {
            None => Err(anyhow!("Channel closed")),
            Some(Err(err)) => Err(err),
            Some(Ok((addr, incoming, res_sender))) => Ok(((incoming, res_sender), addr)),
        }
    }

    async fn handshake(&self, conn: Self::RawStream) -> anyhow::Result<Self::Stream> {
        let (incoming, res_sender) = conn;

        let (sread, swrite) = io::simplex(4096);
        if let Err(err) = res_sender
            .send(OutgoingSimplex {
                inner: ReaderStream::with_capacity(sread, 4096),
            })
            .await
        {
            return Err(anyhow!(err.to_string()));
        }

        Ok(HTTP2Stream {
            recv: IncomingHyper {
                inner: incoming,
                current_chunk: None,
            },
            send: swrite,
        })
    }

    async fn connect(&self, addr: &AddrMaybeCached) -> anyhow::Result<Self::Stream> {
        let client = self.client.clone();
        let (sread, swrite) = io::simplex(4096);
        let body = http_body_util::StreamBody::new(OutgoingSimplex {
            inner: ReaderStream::with_capacity(sread, 4096),
        });
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{}", &addr.addr.as_str()))
            .body(body)
            .expect("request builder");
        let res = client.request(req).await;
        if let Err(err) = res {
            return Err(anyhow!("Error: {}", err));
        }

        let res = res.unwrap();
        if !res.status().is_success() {
            return Err(anyhow!("Bad status code: {}", res.status()));
        }
        Ok(HTTP2Stream {
            recv: IncomingHyper {
                inner: res.into_body(),
                current_chunk: None,
            },
            send: swrite,
        })
    }
}

impl Drop for HTTP2Transport {
    fn drop(&mut self) {
        let _ = self.stop_sender.send(());
    }
}
