use crate::config::{ClientServiceConfig, ServerServiceConfig, TcpConfig, TransportConfig};
use crate::helper::{to_socket_addr, try_set_tcp_keepalive};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::fmt::{Debug, Display};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, ToSocketAddrs};
use tracing::{error, trace};

pub const DEFAULT_NODELAY: bool = true;

pub const DEFAULT_KEEPALIVE_SECS: u64 = 20;
pub const DEFAULT_KEEPALIVE_INTERVAL: u64 = 8;

#[derive(Clone)]
pub struct AddrMaybeCached {
    pub addr: String,
    pub socket_addr: Option<SocketAddr>,
}

impl AddrMaybeCached {
    pub fn new(addr: &str) -> AddrMaybeCached {
        AddrMaybeCached {
            addr: addr.to_string(),
            socket_addr: None,
        }
    }

    pub async fn resolve(&mut self) -> Result<()> {
        match to_socket_addr(&self.addr).await {
            Ok(s) => {
                self.socket_addr = Some(s);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

impl Display for AddrMaybeCached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.socket_addr {
            Some(s) => f.write_fmt(format_args!("{}", s)),
            None => f.write_str(&self.addr),
        }
    }
}

/// Specify a transport layer, like TCP, TLS
#[async_trait]
pub trait Transport: Debug + Send + Sync {
    type Acceptor: Send + Sync;
    type RawStream: Send + Sync;
    type Stream: 'static + AsyncRead + AsyncWrite + Unpin + Send + Sync + Debug;

    fn new(config: &TransportConfig) -> Result<Self>
    where
        Self: Sized;
    /// Provide the transport with socket options, which can be handled at the need of the transport
    fn hint(conn: &Self::Stream, opts: SocketOpts);
    async fn bind<T: ToSocketAddrs + Send + Sync>(&self, addr: T) -> Result<Self::Acceptor>;
    /// accept must be cancel safe
    async fn accept(&self, a: &Self::Acceptor) -> Result<(Self::RawStream, SocketAddr)>;
    async fn handshake(&self, conn: Self::RawStream) -> Result<Self::Stream>;
    async fn connect(&self, addr: &AddrMaybeCached) -> Result<Self::Stream>;
}

mod tcp;
pub use tcp::TcpTransport;

#[cfg(all(feature = "native-tls", feature = "rustls"))]
compile_error!("Only one of `native-tls` and `rustls` can be enabled");

#[cfg(feature = "native-tls")]
mod native_tls;
#[cfg(feature = "native-tls")]
use native_tls as tls;
#[cfg(feature = "rustls")]
mod rustls;
#[cfg(feature = "rustls")]
use rustls as tls;

#[cfg(any(feature = "native-tls", feature = "rustls"))]
pub(crate) use tls::TlsTransport;

#[cfg(feature = "noise")]
mod noise;
#[cfg(feature = "noise")]
pub use noise::NoiseTransport;

#[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
mod websocket;
#[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
pub use websocket::WebsocketTransport;

#[derive(Debug, Clone, Copy)]
struct Keepalive {
    // tcp_keepalive_time if the underlying protocol is TCP
    pub keepalive_secs: u64,
    // tcp_keepalive_intvl if the underlying protocol is TCP
    pub keepalive_interval: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct SocketOpts {
    // None means do not change
    nodelay: Option<bool>,
    // keepalive must be Some or None at the same time, or the behavior will be platform-dependent
    keepalive: Option<Keepalive>,
}

impl SocketOpts {
    fn none() -> SocketOpts {
        SocketOpts {
            nodelay: None,
            keepalive: None,
        }
    }

    /// Socket options for the control channel
    pub fn for_control_channel() -> SocketOpts {
        SocketOpts {
            nodelay: Some(true),  // Always set nodelay for the control channel
            ..SocketOpts::none()  // None means do not change. Keepalive is set by TcpTransport
        }
    }
}

impl SocketOpts {
    pub fn from_cfg(cfg: &TcpConfig) -> SocketOpts {
        SocketOpts {
            nodelay: Some(cfg.nodelay),
            keepalive: Some(Keepalive {
                keepalive_secs: cfg.keepalive_secs,
                keepalive_interval: cfg.keepalive_interval,
            }),
        }
    }

    pub fn from_client_cfg(cfg: &ClientServiceConfig) -> SocketOpts {
        SocketOpts {
            nodelay: cfg.nodelay,
            ..SocketOpts::none()
        }
    }

    pub fn from_server_cfg(cfg: &ServerServiceConfig) -> SocketOpts {
        SocketOpts {
            nodelay: cfg.nodelay,
            ..SocketOpts::none()
        }
    }

    pub fn apply(&self, conn: &TcpStream) {
        if let Some(v) = self.keepalive {
            let keepalive_duration = Duration::from_secs(v.keepalive_secs);
            let keepalive_interval = Duration::from_secs(v.keepalive_interval);

            if let Err(e) = try_set_tcp_keepalive(conn, keepalive_duration, keepalive_interval)
                .with_context(|| "Failed to set keepalive")
            {
                error!("{:#}", e);
            }
        }

        if let Some(nodelay) = self.nodelay {
            trace!("Set nodelay {}", nodelay);
            if let Err(e) = conn
                .set_nodelay(nodelay)
                .with_context(|| "Failed to set nodelay")
            {
                error!("{:#}", e);
            }
        }
    }
}
