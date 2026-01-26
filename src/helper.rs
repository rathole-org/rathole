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

pub fn generate_proxy_protocol_header(s: &TcpStream, proxy_protocol: &str) -> Result<Vec<u8>, anyhow::Error> {
    let local_addr = s.local_addr()?;
    let remote_addr = s.peer_addr()?;

    match proxy_protocol {
        "v1" => {
            let proto = if local_addr.is_ipv4() { "TCP4" } else { "TCP6" };
            let header = format!(
                "PROXY {} {} {} {} {}\r\n", 
                proto, 
                remote_addr.ip(), 
                local_addr.ip(), 
                remote_addr.port(), 
                local_addr.port()
            );

            Ok(header.into_bytes())
        }
        "v2" => {

            let v2sig: &[u8] = &[0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A];
            let ver_cmd = &[0x21]; // 0x21 version 2 and PROXY command
            let proto = if local_addr.is_ipv4() { &[0x11] } else { &[0x21] }; // 0x11 for TCP IPv4 and 0x21 for TCP IPv6, TODO: support UNIX
            let addrs_length: &[u8] = if local_addr.is_ipv4() { &[0, 12] } else { &[0, 36] }; // 12 for IPv4 and 36 for IPv6, TOOD: support UNIX
            let src_addr = match remote_addr {
                SocketAddr::V4(v4) => v4.ip().octets().to_vec(),
                SocketAddr::V6(v6) => v6.ip().octets().to_vec(),
            };
            let dst_addr = match local_addr {
                SocketAddr::V4(v4) => v4.ip().octets().to_vec(),
                SocketAddr::V6(v6) => v6.ip().octets().to_vec(),
            };
    
            let header:Vec<u8> = [
                v2sig, 
                ver_cmd, 
                proto, 
                addrs_length,
                &src_addr,
                &dst_addr,
                &remote_addr.port().to_be_bytes(),
                &local_addr.port().to_be_bytes()
                ].concat();
    
            trace!("Proxy protocol v2 header: {:02x?}", header);
    
            Ok(header)

        },
        _ => {
            Err(anyhow!("Unknown proxy protocol {}", proxy_protocol))
        }
    }

}

#[cfg(test)]
mod proxy_protocol_tests {
    use super::generate_proxy_protocol_header;
    use std::net::{IpAddr, SocketAddr};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const V2_SIG: [u8; 12] = [
        0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
    ];

    fn expected_v1_header(local: SocketAddr, remote: SocketAddr) -> Vec<u8> {
        let proto = if local.is_ipv4() { "TCP4" } else { "TCP6" };
        format!(
            "PROXY {proto} {} {} {} {}\r\n",
            remote.ip(),
            local.ip(),
            remote.port(),
            local.port()
        )
        .into_bytes()
    }

    fn expected_v2_header(local: SocketAddr, remote: SocketAddr) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&V2_SIG);
        out.push(0x21); // v2 + PROXY command

        match (remote.ip(), local.ip()) {
            (IpAddr::V4(src), IpAddr::V4(dst)) => {
                out.push(0x11); // AF_INET (0x1) + STREAM (0x1) => 0x11
                out.extend_from_slice(&[0x00, 0x0c]); // len = 12
                out.extend_from_slice(&src.octets());
                out.extend_from_slice(&dst.octets());
            }
            (IpAddr::V6(src), IpAddr::V6(dst)) => {
                out.push(0x21); // AF_INET6 (0x2) + STREAM (0x1) => 0x21
                out.extend_from_slice(&[0x00, 0x24]); // len = 36
                out.extend_from_slice(&src.octets());
                out.extend_from_slice(&dst.octets());
            }
            _ => panic!("mismatched address families in test"),
        }

        // src port then dst port
        out.extend_from_slice(&remote.port().to_be_bytes());
        out.extend_from_slice(&local.port().to_be_bytes());
        out
    }

    #[tokio::test]
    async fn v1_header_ipv4_format_is_correct() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let _client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        let local = server.local_addr().unwrap();
        let remote = server.peer_addr().unwrap();
        assert!(local.is_ipv4());
        assert!(remote.is_ipv4());

        let expected = expected_v1_header(local, remote);
        let got = generate_proxy_protocol_header(&server, "v1").unwrap();

        assert_eq!(got, expected);
        assert!(got.ends_with(b"\r\n"));
        assert!(got.starts_with(b"PROXY TCP4 "));
    }

    #[tokio::test]
    async fn v2_header_ipv4_format_is_correct() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let _client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        let local = server.local_addr().unwrap();
        let remote = server.peer_addr().unwrap();
        assert!(local.is_ipv4());
        assert!(remote.is_ipv4());

        let expected = expected_v2_header(local, remote);
        let got = generate_proxy_protocol_header(&server, "v2").unwrap();

        assert_eq!(got, expected);

        // Spot-check fixed fields and sizes
        assert_eq!(&got[..12], &V2_SIG);
        assert_eq!(got[12], 0x21);
        assert_eq!(got[13], 0x11);
        assert_eq!(&got[14..16], &[0x00, 0x0c]);
        assert_eq!(got.len(), 28);
    }

    #[tokio::test]
    async fn v1_header_ipv6_format_is_correct_or_skipped_if_unavailable() {
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
        let got = generate_proxy_protocol_header(&server, "v1").unwrap();

        assert_eq!(got, expected);
        assert!(got.ends_with(b"\r\n"));
        assert!(got.starts_with(b"PROXY TCP6 "));
    }

    #[tokio::test]
    async fn v2_header_ipv6_format_is_correct_or_skipped_if_unavailable() {
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

        let expected = expected_v2_header(local, remote);
        let got = generate_proxy_protocol_header(&server, "v2").unwrap();

        assert_eq!(got, expected);
        assert_eq!(&got[..12], &V2_SIG);
        assert_eq!(got[12], 0x21);
        assert_eq!(got[13], 0x21);
        assert_eq!(&got[14..16], &[0x00, 0x24]);
        assert_eq!(got.len(), 52);
    }

    #[tokio::test]
    async fn unknown_proxy_protocol_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let _client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        let err = generate_proxy_protocol_header(&server, "nope").unwrap_err();
        assert!(err.to_string().contains("Unknown proxy protocol"));
    }

    async fn header_is_sent_before_payload(version: &'static str) {
        let visitor_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let visitor_addr = visitor_listener.local_addr().unwrap();

        let mut visitor_client = TcpStream::connect(visitor_addr).await.unwrap();
        let (mut visitor_server, _) = visitor_listener.accept().await.unwrap();

        let expected_header = generate_proxy_protocol_header(&visitor_server, version).unwrap();
        let payload = b"hello proxy protocol";

        let (mut ch, mut downstream) = tokio::io::duplex(4096);

        let server_task = tokio::spawn(async move {
            let header = generate_proxy_protocol_header(&visitor_server, version).unwrap();
            ch.write_all(&header).await.unwrap();
            ch.flush().await.unwrap();

            tokio::io::copy_bidirectional(&mut visitor_server, &mut ch)
                .await
                .unwrap();
        });

        visitor_client.write_all(payload).await.unwrap();
        visitor_client.shutdown().await.unwrap();

        let mut buf = vec![0u8; expected_header.len() + payload.len()];
        downstream.read_exact(&mut buf).await.unwrap();

        let header_len = expected_header.len();
        assert_eq!(&buf[..header_len], &expected_header);
        assert_eq!(&buf[header_len..], payload);

        drop(downstream);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn v1_header_bytes_are_sent_before_payload_when_forwarding() {
        header_is_sent_before_payload("v1").await;
    }

    #[tokio::test]
    async fn v2_header_bytes_are_sent_before_payload_when_forwarding() {
        header_is_sent_before_payload("v2").await;
    }
}
