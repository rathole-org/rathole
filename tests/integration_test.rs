use anyhow::{anyhow, Ok, Result};
use common::{run_rathole_client, PING, PONG};
use rand::Rng;
use rand::RngCore;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpStream, UdpSocket},
    sync::broadcast,
    time,
};
use tracing::{debug, info, instrument};
use tracing_subscriber::EnvFilter;

use crate::common::run_rathole_server;

mod common;

const ECHO_SERVER_ADDR: &str = "127.0.0.1:8080";
const PINGPONG_SERVER_ADDR: &str = "127.0.0.1:8081";
const ECHO_SERVER_ADDR_EXPOSED: &str = "127.0.0.1:2334";
const PINGPONG_SERVER_ADDR_EXPOSED: &str = "127.0.0.1:2335";
const HITTER_NUM: usize = 4;

const PP2_SIG: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];


#[derive(Clone, Copy, Debug)]
enum Type {
    Tcp,
    Udp,
}

fn init() {
    let level = "info";
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from(level)),
        )
        .try_init();
}

#[tokio::test]
async fn tcp() -> Result<()> {
    init();

    // Spawn a echo server
    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    // Spawn a pingpong server
    tokio::spawn(async move {
        if let Err(e) = common::tcp::pingpong_server(PINGPONG_SERVER_ADDR).await {
            panic!("Failed to run the pingpong server for testing: {:?}", e);
        }
    });

    test("tests/for_tcp/tcp_transport.toml", Type::Tcp).await?;

    test_proxy_protocol("tests/for_tcp/tcp_transport_proxy_protocol_v1.toml").await?;
    test_proxy_protocol("tests/for_tcp/tcp_transport_proxy_protocol_v2.toml").await?;

    #[cfg(any(
         // FIXME: Self-signed certificate on macOS nativetls requires manual interference.
         all(target_os = "macos", feature = "rustls"),
         // On other OS accept run with either
         all(not(target_os = "macos"), any(feature = "native-tls", feature = "rustls")),
     ))]
    test("tests/for_tcp/tls_transport.toml", Type::Tcp).await?;

    #[cfg(feature = "noise")]
    test("tests/for_tcp/noise_transport.toml", Type::Tcp).await?;

    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test("tests/for_tcp/websocket_transport.toml", Type::Tcp).await?;

    #[cfg(not(target_os = "macos"))]
    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test("tests/for_tcp/websocket_tls_transport.toml", Type::Tcp).await?;

    Ok(())
}

#[tokio::test]
async fn udp() -> Result<()> {
    init();

    // Spawn a echo server
    tokio::spawn(async move {
        if let Err(e) = common::udp::echo_server(ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    // Spawn a pingpong server
    tokio::spawn(async move {
        if let Err(e) = common::udp::pingpong_server(PINGPONG_SERVER_ADDR).await {
            panic!("Failed to run the pingpong server for testing: {:?}", e);
        }
    });

    test("tests/for_udp/tcp_transport.toml", Type::Udp).await?;

    #[cfg(any(
         // FIXME: Self-signed certificate on macOS nativetls requires manual interference.
         all(target_os = "macos", feature = "rustls"),
         // On other OS accept run with either
         all(not(target_os = "macos"), any(feature = "native-tls", feature = "rustls")),
     ))]
    test("tests/for_udp/tls_transport.toml", Type::Udp).await?;

    #[cfg(feature = "noise")]
    test("tests/for_udp/noise_transport.toml", Type::Udp).await?;

    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test("tests/for_udp/websocket_transport.toml", Type::Udp).await?;

    #[cfg(not(target_os = "macos"))]
    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test("tests/for_udp/websocket_tls_transport.toml", Type::Udp).await?;

    Ok(())
}

#[instrument]
async fn test(config_path: &'static str, t: Type) -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        // Skip the test if the client or the server is not enabled
        return Ok(());
    }

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    // Start the client
    info!("start the client");
    let client = tokio::spawn(async move {
        run_rathole_client(config_path, client_shutdown_rx)
            .await
            .unwrap();
    });

    // Sleep for 1 second. Expect the client keep retrying to reach the server
    time::sleep(Duration::from_secs(1)).await;

    // Start the server
    info!("start the server");
    let server = tokio::spawn(async move {
        run_rathole_server(config_path, server_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_millis(2500)).await; // Wait for the client to retry

    info!("echo");
    echo_hitter(ECHO_SERVER_ADDR_EXPOSED, t).await.unwrap();
    info!("pingpong");
    pingpong_hitter(PINGPONG_SERVER_ADDR_EXPOSED, t)
        .await
        .unwrap();

    // Simulate the client crash and restart
    info!("shutdown the client");
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(client);

    info!("restart the client");
    let client_shutdown_rx = client_shutdown_tx.subscribe();
    let client = tokio::spawn(async move {
        run_rathole_client(config_path, client_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_secs(1)).await; // Wait for the client to start

    info!("echo");
    echo_hitter(ECHO_SERVER_ADDR_EXPOSED, t).await.unwrap();
    info!("pingpong");
    pingpong_hitter(PINGPONG_SERVER_ADDR_EXPOSED, t)
        .await
        .unwrap();

    // Simulate the server crash and restart
    info!("shutdown the server");
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(server);

    info!("restart the server");
    let server_shutdown_rx = server_shutdown_tx.subscribe();
    let server = tokio::spawn(async move {
        run_rathole_server(config_path, server_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_millis(2500)).await; // Wait for the client to retry

    // Simulate heavy load
    info!("lots of echo and pingpong");

    let mut v = Vec::new();

    for _ in 0..HITTER_NUM / 2 {
        v.push(tokio::spawn(async move {
            echo_hitter(ECHO_SERVER_ADDR_EXPOSED, t).await.unwrap();
        }));

        v.push(tokio::spawn(async move {
            pingpong_hitter(PINGPONG_SERVER_ADDR_EXPOSED, t)
                .await
                .unwrap();
        }));
    }

    for h in v {
        assert!(tokio::join!(h).0.is_ok());
    }

    // Shutdown
    info!("shutdown the server and the client");
    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;

    let _ = tokio::join!(server, client);

    Ok(())
}

async fn echo_hitter(addr: &'static str, t: Type) -> Result<()> {
    match t {
        Type::Tcp => tcp_echo_hitter(addr).await,
        Type::Udp => udp_echo_hitter(addr).await,
    }
}

async fn pingpong_hitter(addr: &'static str, t: Type) -> Result<()> {
    match t {
        Type::Tcp => tcp_pingpong_hitter(addr).await,
        Type::Udp => udp_pingpong_hitter(addr).await,
    }
}

async fn tcp_echo_hitter(addr: &'static str) -> Result<()> {
    let mut conn = TcpStream::connect(addr).await?;

    let mut wr = [0u8; 1024];
    let mut rd = [0u8; 1024];
    for _ in 0..100 {
        rand::thread_rng().fill(&mut wr);
        conn.write_all(&wr).await?;
        conn.read_exact(&mut rd).await?;
        assert_eq!(wr, rd);
    }

    Ok(())
}

async fn udp_echo_hitter(addr: &'static str) -> Result<()> {
    let conn = UdpSocket::bind("127.0.0.1:0").await?;
    conn.connect(addr).await?;

    let mut wr = [0u8; 128];
    let mut rd = [0u8; 128];
    for _ in 0..3 {
        rand::thread_rng().fill(&mut wr);

        conn.send(&wr).await?;
        debug!("send");

        conn.recv(&mut rd).await?;
        debug!("recv");

        assert_eq!(wr, rd);
    }
    Ok(())
}

async fn tcp_pingpong_hitter(addr: &'static str) -> Result<()> {
    let mut conn = TcpStream::connect(addr).await?;

    let wr = PING.as_bytes();
    let mut rd = [0u8; PONG.len()];

    for _ in 0..100 {
        conn.write_all(wr).await?;
        conn.read_exact(&mut rd).await?;
        assert_eq!(rd, PONG.as_bytes());
    }

    Ok(())
}

async fn udp_pingpong_hitter(addr: &'static str) -> Result<()> {
    let conn = UdpSocket::bind("127.0.0.1:0").await?;
    conn.connect(&addr).await?;

    let wr = PING.as_bytes();
    let mut rd = [0u8; PONG.len()];

    for _ in 0..3 {
        conn.send(wr).await?;
        debug!("ping");

        conn.recv(&mut rd).await?;
        debug!("pong");

        assert_eq!(rd, PONG.as_bytes());
    }

    Ok(())
}

#[instrument]
async fn test_proxy_protocol(config_path: &'static str) -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        return Ok(());
    }

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    info!("start the client");
    let client = tokio::spawn(async move {
        run_rathole_client(config_path, client_shutdown_rx)
            .await
            .unwrap();
    });

    time::sleep(Duration::from_secs(1)).await;

    info!("start the server");
    let server = tokio::spawn(async move {
        run_rathole_server(config_path, server_shutdown_rx)
            .await
            .unwrap();
    });

    time::sleep(Duration::from_millis(2500)).await;

    info!("echo");
    tcp_echo_hitter_expect_proxy_protocol(ECHO_SERVER_ADDR_EXPOSED).await?;

    info!("pingpong )");
    tcp_pingpong_hitter(PINGPONG_SERVER_ADDR_EXPOSED).await?;

    info!("shutdown the server and the client");
    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;

    let _ = tokio::join!(server, client);

    Ok(())
}

async fn read_proxy_protocol_header(rd: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Result<Vec<u8>> {
    // Read 12 bytes to distinguish v2 signature vs v1 ("PROXY ...")
    let mut first12 = [0u8; 12];
    time::timeout(Duration::from_secs(5), rd.read_exact(&mut first12)).await??;

    if first12 == PP2_SIG {
        // v2: read fixed header (ver/cmd, fam/proto, len[2]) then read len bytes
        let mut fixed = [0u8; 4];
        time::timeout(Duration::from_secs(5), rd.read_exact(&mut fixed)).await??;

        let len = u16::from_be_bytes([fixed[2], fixed[3]]) as usize;
        let mut addr_and_tlvs = vec![0u8; len];
        time::timeout(Duration::from_secs(5), rd.read_exact(&mut addr_and_tlvs)).await??;

        let mut out = Vec::with_capacity(16 + len);
        out.extend_from_slice(&first12);
        out.extend_from_slice(&fixed);
        out.extend_from_slice(&addr_and_tlvs);
        Ok(out)
    } else {
        // v1: we've already consumed 12 bytes; read until newline to complete the line
        let mut out = first12.to_vec();
        let n = time::timeout(Duration::from_secs(5), rd.read_until(b'\n', &mut out)).await??;
        if n == 0 {
            return Err(anyhow!("EOF while reading proxy protocol v1 line"));
        }
        Ok(out)
    }
}

fn assert_proxy_v2_matches(header: &[u8], local: SocketAddr, peer: SocketAddr) {
    assert!(header.len() >= 16);
    assert_eq!(&header[..12], &PP2_SIG);

    // version/command
    assert_eq!(header[12], 0x21, "expected v2 PROXY command (0x21)");

    let fam_proto = header[13];
    let len = u16::from_be_bytes([header[14], header[15]]) as usize;
    assert_eq!(header.len(), 16 + len, "v2 length mismatch");

    match fam_proto {
        0x11 => {
            // INET + STREAM, minimum 12 bytes address block
            assert!(len >= 12);

            let src = IpAddr::V4(Ipv4Addr::new(header[16], header[17], header[18], header[19]));
            let dst = IpAddr::V4(Ipv4Addr::new(header[20], header[21], header[22], header[23]));
            let src_port = u16::from_be_bytes([header[24], header[25]]);
            let dst_port = u16::from_be_bytes([header[26], header[27]]);

            assert_eq!(src, local.ip());
            assert_eq!(dst, peer.ip());
            assert_eq!(src_port, local.port());
            assert_eq!(dst_port, peer.port());
        }
        0x21 => {
            // INET6 + STREAM, minimum 36 bytes address block
            assert!(len >= 36);

            let mut src_oct = [0u8; 16];
            let mut dst_oct = [0u8; 16];
            src_oct.copy_from_slice(&header[16..32]);
            dst_oct.copy_from_slice(&header[32..48]);

            let src = IpAddr::V6(Ipv6Addr::from(src_oct));
            let dst = IpAddr::V6(Ipv6Addr::from(dst_oct));
            let src_port = u16::from_be_bytes([header[48], header[49]]);
            let dst_port = u16::from_be_bytes([header[50], header[51]]);

            assert_eq!(src, local.ip());
            assert_eq!(dst, peer.ip());
            assert_eq!(src_port, local.port());
            assert_eq!(dst_port, peer.port());
        }
        other => panic!("unexpected v2 fam/proto byte: {other:#x}"),
    }
}


async fn tcp_echo_hitter_expect_proxy_protocol(addr: &'static str) -> Result<()> {
    let conn = TcpStream::connect(addr).await?;
    let local = conn.local_addr()?;
    let peer = conn.peer_addr()?;

    let (rd, mut wr) = conn.into_split();
    let mut rd = BufReader::new(rd);

    // Read & validate proxy protocol header (v1 or v2)
    let header = read_proxy_protocol_header(&mut rd).await?;

    if header.starts_with(b"PROXY ") {
        // v1 assertion (stringy)
        let proto = if local.is_ipv4() { "TCP4" } else { "TCP6" };
        let expected = format!(
            "PROXY {proto} {} {} {} {}\r\n",
            local.ip(),
            peer.ip(),
            local.port(),
            peer.port()
        )
        .into_bytes();
        assert_eq!(header, expected);
    } else {
        // v2 assertion (binary)
        assert_proxy_v2_matches(&header, local, peer);
    }

    // Now the stream should behave like a normal echo connection.
    let mut wr_buf = [0u8; 1024];
    let mut rd_buf = [0u8; 1024];

    for _ in 0..100 {
        rand::thread_rng().fill_bytes(&mut wr_buf);
        wr.write_all(&wr_buf).await?;
        rd.read_exact(&mut rd_buf).await?;
        assert_eq!(wr_buf, rd_buf);
    }

    Ok(())
}





