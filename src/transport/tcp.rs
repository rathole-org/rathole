use crate::{
    config::{TcpConfig, TransportConfig},
    helper::tcp_connect_with_proxy,
};
#[cfg(target_os = "linux")]
use crate::helper::{tcp_bind_fast_open, to_socket_addr};

use super::{AddrMaybeCached, SocketOpts, Transport, TransportRole};
use anyhow::Result;
use async_trait::async_trait;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

#[derive(Debug)]
pub struct TcpTransport {
    socket_opts: SocketOpts,
    cfg: TcpConfig,
}

#[async_trait]
impl Transport for TcpTransport {
    type Acceptor = TcpListener;
    type Stream = TcpStream;
    type RawStream = TcpStream;

    fn new(config: &TransportConfig, _role: TransportRole) -> Result<Self> {
        Ok(TcpTransport {
            socket_opts: SocketOpts::from_cfg(&config.tcp),
            cfg: config.tcp.clone(),
        })
    }

    fn hint(conn: &Self::Stream, opt: SocketOpts) {
        opt.apply(conn);
    }

    async fn bind<T: ToSocketAddrs + Send + Sync>(&self, addr: T) -> Result<Self::Acceptor> {
        #[cfg(target_os = "linux")]
        if self.cfg.fast_open {
            let socket_addr = to_socket_addr(addr).await?;
            return tcp_bind_fast_open(socket_addr).await;
        }
        Ok(TcpListener::bind(addr).await?)
    }

    async fn accept(&self, a: &Self::Acceptor) -> Result<(Self::RawStream, SocketAddr)> {
        let (s, addr) = a.accept().await?;
        self.socket_opts.apply(&s);
        Ok((s, addr))
    }

    async fn handshake(&self, conn: Self::RawStream) -> Result<Self::Stream> {
        Ok(conn)
    }

    async fn connect(&self, addr: &AddrMaybeCached) -> Result<Self::Stream> {
        let s = tcp_connect_with_proxy(addr, self.cfg.proxy.as_ref(), self.cfg.fast_open).await?;
        self.socket_opts.apply(&s);
        Ok(s)
    }
}
