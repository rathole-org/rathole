use anyhow::{anyhow, Context, Result};
use async_http_proxy::{http_connect_tokio, http_connect_tokio_with_basic_auth};
use backoff::{backoff::Backoff, Notify};
use socket2::{SockRef, TcpKeepalive};
use std::{future::Future, net::SocketAddr, time::Duration};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::{
    net::{lookup_host, TcpStream, ToSocketAddrs, UdpSocket},
    sync::broadcast,
};
use tracing::trace;
use url::Url;

use crate::transport::AddrMaybeCached;

// Tokio hesitates to expose this option...So we have to do it on our own :(
// The good news is that using socket2 it can be easily done, without losing portability.
// See https://github.com/tokio-rs/tokio/issues/3082
pub fn try_set_tcp_keepalive(
    conn: &TcpStream,
    keepalive_duration: Duration,
    keepalive_interval: Duration,
) -> Result<()> {
    let s = SockRef::from(conn);
    let keepalive = TcpKeepalive::new()
        .with_time(keepalive_duration)
        .with_interval(keepalive_interval);

    trace!(
        "Set TCP keepalive {:?} {:?}",
        keepalive_duration,
        keepalive_interval
    );

    Ok(s.set_tcp_keepalive(&keepalive)?)
}

#[allow(dead_code)]
pub fn feature_not_compile(feature: &str) -> ! {
    panic!(
        "The feature '{}' is not compiled in this binary. Please re-compile rathole",
        feature
    )
}

#[allow(dead_code)]
pub fn feature_neither_compile(feature1: &str, feature2: &str) -> ! {
    panic!(
        "Neither of the feature '{}' or '{}' is compiled in this binary. Please re-compile rathole",
        feature1, feature2
    )
}

pub async fn to_socket_addr<A: ToSocketAddrs>(addr: A) -> Result<SocketAddr> {
    lookup_host(addr)
        .await?
        .next()
        .ok_or_else(|| anyhow!("Failed to lookup the host"))
}

pub fn host_port_pair(s: &str) -> Result<(&str, u16)> {
    let semi = s.rfind(':').expect("missing semicolon");
    Ok((&s[..semi], s[semi + 1..].parse()?))
}

/// Create a UDP socket and connect to `addr`
pub async fn udp_connect<A: ToSocketAddrs>(addr: A) -> Result<UdpSocket> {
    let addr = to_socket_addr(addr).await?;

    let bind_addr = match addr {
        SocketAddr::V4(_) => "0.0.0.0:0",
        SocketAddr::V6(_) => ":::0",
    };

    let s = UdpSocket::bind(bind_addr).await?;
    s.connect(addr).await?;
    Ok(s)
}

/// Create a TcpStream using a proxy
/// e.g. socks5://user:pass@127.0.0.1:1080 http://127.0.0.1:8080
pub async fn tcp_connect_with_proxy(
    addr: &AddrMaybeCached,
    proxy: Option<&Url>,
) -> Result<TcpStream> {
    if let Some(url) = proxy {
        let addr = &addr.addr;
        let mut s = TcpStream::connect((
            url.host_str().expect("proxy url should have host field"),
            url.port().expect("proxy url should have port field"),
        ))
        .await?;

        let auth = if !url.username().is_empty() || url.password().is_some() {
            Some(async_socks5::Auth {
                username: url.username().into(),
                password: url.password().unwrap_or("").into(),
            })
        } else {
            None
        };
        match url.scheme() {
            "socks5" => {
                async_socks5::connect(&mut s, host_port_pair(addr)?, auth).await?;
            }
            "http" => {
                let (host, port) = host_port_pair(addr)?;
                match auth {
                    Some(auth) => {
                        http_connect_tokio_with_basic_auth(
                            &mut s,
                            host,
                            port,
                            &auth.username,
                            &auth.password,
                        )
                        .await?
                    }
                    None => http_connect_tokio(&mut s, host, port).await?,
                }
            }
            _ => panic!("unknown proxy scheme"),
        }
        Ok(s)
    } else {
        Ok(match addr.socket_addr {
            Some(s) => TcpStream::connect(s).await?,
            None => TcpStream::connect(&addr.addr).await?,
        })
    }
}

// Wrapper of retry_notify
pub async fn retry_notify_with_deadline<I, E, Fn, Fut, B, N>(
    backoff: B,
    operation: Fn,
    notify: N,
    deadline: &mut broadcast::Receiver<bool>,
) -> Result<I>
where
    E: std::error::Error + Send + Sync + 'static,
    B: Backoff,
    Fn: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<I, backoff::Error<E>>>,
    N: Notify<E>,
{
    tokio::select! {
        v = backoff::future::retry_notify(backoff, operation, notify) => {
            v.map_err(anyhow::Error::new)
        }
        _ = deadline.recv() => {
            Err(anyhow!("shutdown"))
        }
    }
}

pub async fn write_and_flush<T>(conn: &mut T, data: &[u8]) -> Result<()>
where
    T: AsyncWrite + Unpin,
{
    conn.write_all(data)
        .await
        .with_context(|| "Failed to write data")?;
    conn.flush().await.with_context(|| "Failed to flush data")?;
    Ok(())
}

pub fn generate_proxy_protocol_v1_header(s: &TcpStream) -> Result<String> {
    let local_addr = s.local_addr()?;
    let remote_addr = s.peer_addr()?;
    let proto = if local_addr.is_ipv4() { "TCP4" } else { "TCP6" };
    let header = format!(
        "PROXY {} {} {} {} {}\r\n", 
        proto, 
        remote_addr.ip(), 
        local_addr.ip(), 
        remote_addr.port(), 
        local_addr.port()
    );
    Ok(header)
}
