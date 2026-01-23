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
pub async fn udp_connect<A: ToSocketAddrs>(addr: A, prefer_ipv6: bool) -> Result<UdpSocket> {

    let (socket_addr, bind_addr);

    match prefer_ipv6 {
        false => {
            socket_addr = to_socket_addr(addr).await?;

            bind_addr = match socket_addr {
                SocketAddr::V4(_) => "0.0.0.0:0",
                SocketAddr::V6(_) => ":::0",
            };
        },
        true => {
            let all_host_addresses: Vec<SocketAddr> = lookup_host(addr).await?.collect();

            // Try to find an IPv6 address
            match all_host_addresses.clone().iter().find(|x| x.is_ipv6()) {
                Some(socket_addr_ipv6) => {
                    socket_addr = *socket_addr_ipv6;
                    bind_addr = ":::0";
                },
                None => {
                    let socket_addr_ipv4 = all_host_addresses.iter().find(|x| x.is_ipv4());
                    match socket_addr_ipv4 {
                        None => return Err(anyhow!("Failed to lookup the host")),
                        // fallback to IPv4
                        Some(socket_addr_ipv4) => {
                            socket_addr = *socket_addr_ipv4;
                            bind_addr = "0.0.0.0:0";
                        }
                    }
                }
            }
        }
    };
    let s = UdpSocket::bind(bind_addr).await?;
    s.connect(socket_addr).await?;
    s.connect(socket_addr).await?;
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

#[cfg(test)]
mod proxy_protocol_tests {
    use super::generate_proxy_protocol_v1_header;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn expected_v1_header(local: std::net::SocketAddr, remote: std::net::SocketAddr) -> String {
        let proto = if local.is_ipv4() { "TCP4" } else { "TCP6" };
        format!(
            "PROXY {proto} {} {} {} {}\r\n",
            remote.ip(),
            local.ip(),
            remote.port(),
            local.port()
        )
    }

    #[tokio::test]
    async fn v1_header_ipv4_format_is_correct() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Create a connection so we get a real TcpStream with real peer/local addrs
        let _client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        let local = server.local_addr().unwrap();
        let remote = server.peer_addr().unwrap();

        assert!(local.is_ipv4());
        assert!(remote.is_ipv4());

        let expected = expected_v1_header(local, remote);
        let got = generate_proxy_protocol_v1_header(&server).unwrap();

        assert_eq!(got, expected);
        assert!(got.ends_with("\r\n"));
        assert!(got.starts_with("PROXY TCP4 "));
    }

    #[tokio::test]
    async fn v1_header_ipv6_format_is_correct_or_skipped_if_unavailable() {
        // Some CI environments don’t have IPv6 loopback enabled; skip gracefully.
        let listener = match TcpListener::bind("[::1]:0").await {
            Ok(l) => l,
            Err(_) => return,
        };
        let addr = listener.local_addr().unwrap();

        let _client = match TcpStream::connect(addr).await {
            Ok(c) => c,
            Err(_) => return,
        };
        let (server, _) = listener.accept().await.unwrap();

        let local = server.local_addr().unwrap();
        let remote = server.peer_addr().unwrap();

        assert!(local.is_ipv6());
        assert!(remote.is_ipv6());

        let expected = expected_v1_header(local, remote);
        let got = generate_proxy_protocol_v1_header(&server).unwrap();

        assert_eq!(got, expected);
        assert!(got.ends_with("\r\n"));
        assert!(got.starts_with("PROXY TCP6 "));
    }

    #[tokio::test]
    async fn header_bytes_are_sent_before_payload_when_written_then_forwarded() {
        // This simulates the exact ordering your server code implements:
        // write PROXY header -> flush -> start forwarding bytes. :contentReference[oaicite:1]{index=1}

        // Visitor side connection (incoming connection to the server)
        let visitor_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let visitor_addr = visitor_listener.local_addr().unwrap();

        let mut visitor_client = TcpStream::connect(visitor_addr).await.unwrap();
        let (mut visitor_server, _) = visitor_listener.accept().await.unwrap();

        let local = visitor_server.local_addr().unwrap();
        let remote = visitor_server.peer_addr().unwrap();
        let expected_header = expected_v1_header(local, remote);

        let payload = b"hello proxy protocol";

        // Simulate the “data channel” stream with a duplex pipe.
        // (One end is what the server writes into; the other end is what the downstream reads.)
        let (mut ch, mut downstream) = tokio::io::duplex(4096);

        let server_task = tokio::spawn(async move {
            let header = generate_proxy_protocol_v1_header(&visitor_server).unwrap();
            ch.write_all(header.as_bytes()).await.unwrap();
            ch.flush().await.unwrap();

            tokio::io::copy_bidirectional(&mut visitor_server, &mut ch)
                .await
                .unwrap();
        });

        // Visitor sends payload
        visitor_client.write_all(payload).await.unwrap();
        visitor_client.shutdown().await.unwrap();

        // Downstream should see header first, then payload
        let mut buf = vec![0u8; expected_header.as_bytes().len() + payload.len()];
        downstream.read_exact(&mut buf).await.unwrap();

        let header_len = expected_header.as_bytes().len();
        assert_eq!(&buf[..header_len], expected_header.as_bytes());
        assert_eq!(&buf[header_len..], payload);

        // Close downstream to let copy_bidirectional finish cleanly
        drop(downstream);
        server_task.await.unwrap();
    }
}