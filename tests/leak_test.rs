//! Regression tests for the two-phase forwarder.
//!
//! Two scenarios under one rathole instance:
//!   1. Half-close-then-respond (correctness): visitor writes, half-closes,
//!      upstream sees EOF, upstream writes a response, visitor must read it.
//!      Verifies we did NOT regress what `copy_bidirectional` did right.
//!   2. Stuck-peer cleanup (leak): visitor half-closes, upstream never
//!      writes nor closes; the post-half-close idle timeout must reap the
//!      connection so the visitor's read returns within a bounded time.
//!
//! Both scenarios run across every enabled transport, with a separate Unix
//! socket variant for the `socket_stream` service type. Every scenario runs
//! under a 60-second deadline and TLS scenarios use generated certificates,
//! matching `tests/integration_test.rs` (see `tests/README.md`).

use anyhow::{anyhow, Context, Result};
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::broadcast,
    task::JoinHandle,
    time::{self, timeout},
};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::common::{run_rathole_client, run_rathole_server};

// `common` is shared with `integration_test.rs`; the echo/pingpong helpers
// it carries are not needed here.
#[allow(dead_code)]
mod common;

// === TCP transports ===
const DELAYED_RESPONSE_LOCAL: &str = "127.0.0.1:9080";
const SINKHOLE_LOCAL: &str = "127.0.0.1:9081";
const DELAYED_RESPONSE_EXPOSED: &str = "127.0.0.1:3334";
const SINKHOLE_EXPOSED: &str = "127.0.0.1:3335";

// === Unix socket variant (separate config to avoid port collisions) ===
#[cfg(unix)]
const SOCK_DELAYED_RESPONSE_LOCAL: &str = "/tmp/rathole_leak_test/delayed_response.sock";
#[cfg(unix)]
const SOCK_SINKHOLE_LOCAL: &str = "/tmp/rathole_leak_test/sinkhole.sock";
#[cfg(unix)]
const SOCK_DELAYED_RESPONSE_EXPOSED: &str = "/tmp/rathole_leak_test/delayed_response_exposed.sock";
#[cfg(unix)]
const SOCK_SINKHOLE_EXPOSED: &str = "/tmp/rathole_leak_test/sinkhole_exposed.sock";

const SCENARIO_TIMEOUT: Duration = Duration::from_secs(60);
const TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(any(feature = "native-tls", feature = "rustls"))]
const TLS_SETUP_TIMEOUT: Duration = Duration::from_secs(60);

fn init() {
    let level = "info";
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from(level)),
        )
        .try_init();
}

/// Local target that reads the request, observes EOF, writes a response, then
/// closes. This is the half-close-then-respond pattern that
/// `copy_bidirectional` handles correctly and the broken `select!+copy`
/// approach destroys.
async fn delayed_response_server(addr: &'static str) -> Result<()> {
    let l = TcpListener::bind(addr).await?;
    loop {
        let (mut conn, _) = l.accept().await?;
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            // Drain request until EOF.
            loop {
                match conn.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(_) => return,
                }
            }
            // Small delay to simulate "compute after EOF" — must not race the
            // forwarder's idle timeout (which is 1s in the toml).
            time::sleep(Duration::from_millis(150)).await;
            let _ = conn.write_all(b"RESP").await;
            let _ = conn.shutdown().await;
        });
    }
}

/// Local target that drains the request and never closes its own write side.
/// Triggers the post-half-close idle path.
async fn sinkhole_server(addr: &'static str) -> Result<()> {
    let l = TcpListener::bind(addr).await?;
    loop {
        let (mut conn, _) = l.accept().await?;
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match conn.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
            time::sleep(Duration::from_secs(120)).await;
            drop(conn);
        });
    }
}

#[cfg(unix)]
async fn delayed_response_unix_server(addr: &'static str) -> Result<()> {
    let _ = std::fs::remove_file(addr);
    let l = tokio::net::UnixListener::bind(addr)?;
    loop {
        let (mut conn, _) = l.accept().await?;
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match conn.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(_) => return,
                }
            }
            time::sleep(Duration::from_millis(150)).await;
            let _ = conn.write_all(b"RESP").await;
            let _ = conn.shutdown().await;
        });
    }
}

#[cfg(unix)]
async fn sinkhole_unix_server(addr: &'static str) -> Result<()> {
    let _ = std::fs::remove_file(addr);
    let l = tokio::net::UnixListener::bind(addr)?;
    loop {
        let (mut conn, _) = l.accept().await?;
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match conn.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
            time::sleep(Duration::from_secs(120)).await;
            drop(conn);
        });
    }
}

#[tokio::test]
async fn forwarder_regression_across_transports() -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        return Ok(());
    }

    init();

    tokio::spawn(async move {
        if let Err(e) = delayed_response_server(DELAYED_RESPONSE_LOCAL).await {
            panic!("delayed-response server failed: {:?}", e);
        }
    });
    tokio::spawn(async move {
        if let Err(e) = sinkhole_server(SINKHOLE_LOCAL).await {
            panic!("sinkhole server failed: {:?}", e);
        }
    });
    time::sleep(Duration::from_millis(200)).await;

    run_for_transport(PathBuf::from("tests/for_leak/tcp_transport.toml"), true).await?;

    #[cfg(any(
        // FIXME: Self-signed certificate on macOS nativetls requires manual interference.
        all(target_os = "macos", feature = "rustls"),
        // On other OS accept run with either
        all(not(target_os = "macos"), any(feature = "native-tls", feature = "rustls")),
    ))]
    // SChannel (`native-tls` on Windows) tears down the whole TLS session on
    // close_notify, so a half-close-then-respond exchange cannot be carried
    // over it; that was equally true with `copy_bidirectional`. OpenSSL-based
    // `native-tls` and `rustls` preserve it. Stuck-peer cleanup applies to all.
    run_for_transport_tls(
        "tests/for_leak/tls_transport.toml",
        !cfg!(all(windows, feature = "native-tls")),
    )
    .await?;

    #[cfg(feature = "noise")]
    run_for_transport(PathBuf::from("tests/for_leak/noise_transport.toml"), true).await?;

    // WebSocket cannot carry a half-close-then-respond pattern because per
    // RFC 6455 the peer that receives a Close frame must respond with its
    // own Close, which closes its sending side too — we can still verify
    // the stuck-peer cleanup path though.
    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    run_for_transport(
        PathBuf::from("tests/for_leak/websocket_transport.toml"),
        false,
    )
    .await?;

    #[cfg(not(target_os = "macos"))]
    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    run_for_transport_tls("tests/for_leak/websocket_tls_transport.toml", false).await?;

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn forwarder_regression_unix_socket() -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        return Ok(());
    }

    init();
    let _ = std::fs::create_dir_all("/tmp/rathole_leak_test");

    tokio::spawn(async move {
        if let Err(e) = delayed_response_unix_server(SOCK_DELAYED_RESPONSE_LOCAL).await {
            panic!("delayed-response unix server failed: {:?}", e);
        }
    });
    tokio::spawn(async move {
        if let Err(e) = sinkhole_unix_server(SOCK_SINKHOLE_LOCAL).await {
            panic!("sinkhole unix server failed: {:?}", e);
        }
    });
    time::sleep(Duration::from_millis(200)).await;

    run_unix_for_transport(PathBuf::from("tests/for_socket_leak/tcp_transport.toml")).await?;

    Ok(())
}

/// A rathole server or client running on its own task. Dropping it aborts
/// the task, so a failed assertion or a scenario deadline cannot leave an
/// instance holding the ports that the next transport in the same test needs.
struct Instance {
    name: &'static str,
    shutdown_tx: broadcast::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
}

impl Instance {
    fn start<F, Fut>(name: &'static str, config_path: PathBuf, run: F) -> Self
    where
        F: FnOnce(PathBuf, broadcast::Receiver<bool>) -> Fut,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
        let task = tokio::spawn(run(config_path, shutdown_rx));
        Instance {
            name,
            shutdown_tx,
            task: Some(task),
        }
    }

    /// Fail fast, with the instance's own error, if it died before the
    /// scenario got to use it (e.g. a port still held by a previous run).
    async fn check_running(&mut self) -> Result<()> {
        let Some(task) = self.task.as_ref() else {
            return Err(anyhow!("{} is not running", self.name));
        };
        if !task.is_finished() {
            return Ok(());
        }
        let result = self
            .task
            .take()
            .expect("finished task should exist")
            .await
            .with_context(|| format!("{} task panicked", self.name))?;
        match result {
            Ok(()) => Err(anyhow!("{} exited unexpectedly", self.name)),
            Err(error) => Err(error).with_context(|| format!("{} exited unexpectedly", self.name)),
        }
    }

    async fn stop(mut self) -> Result<()> {
        let Some(mut task) = self.task.take() else {
            return Ok(());
        };
        let _ = self.shutdown_tx.send(true);
        match time::timeout(TASK_SHUTDOWN_TIMEOUT, &mut task).await {
            Ok(join_result) => join_result
                .with_context(|| format!("{} task panicked", self.name))?
                .with_context(|| format!("{} failed while shutting down", self.name)),
            Err(_) => {
                task.abort();
                Err(anyhow!(
                    "{} did not stop within {TASK_SHUTDOWN_TIMEOUT:?}",
                    self.name
                ))
            }
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Run both forwarder checks for one transport config under the scenario
/// deadline. On timeout the scenario future is dropped, which aborts the
/// rathole instances it owns.
async fn run_for_transport(config_path: PathBuf, check_half_close_response: bool) -> Result<()> {
    let label = config_path.display().to_string();
    info!("forwarder regression for {label}");
    match time::timeout(
        SCENARIO_TIMEOUT,
        run_scenario(config_path, check_half_close_response),
    )
    .await
    {
        Ok(result) => result.with_context(|| format!("forwarder regression `{label}` failed")),
        Err(_) => Err(anyhow!(
            "forwarder regression `{label}` timed out after {SCENARIO_TIMEOUT:?}"
        )),
    }
}

async fn run_scenario(config_path: PathBuf, check_half_close_response: bool) -> Result<()> {
    let mut server = Instance::start("rathole server", config_path.clone(), run_rathole_server);
    time::sleep(Duration::from_millis(500)).await;
    let mut client = Instance::start("rathole client", config_path, run_rathole_client);
    time::sleep(Duration::from_millis(2500)).await;
    server.check_running().await?;
    client.check_running().await?;

    if check_half_close_response {
        check_half_close_with_response(DELAYED_RESPONSE_EXPOSED).await?;
    }
    check_stuck_peer_cleanup_tcp(SINKHOLE_EXPOSED).await?;

    client.stop().await?;
    server.stop().await?;
    // Let the listener ports settle before the next transport reuses them.
    time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

#[cfg(unix)]
async fn run_unix_for_transport(config_path: PathBuf) -> Result<()> {
    let label = config_path.display().to_string();
    info!("unix-socket forwarder regression for {label}");
    match time::timeout(SCENARIO_TIMEOUT, run_unix_scenario(config_path)).await {
        Ok(result) => {
            result.with_context(|| format!("unix-socket forwarder regression `{label}` failed"))
        }
        Err(_) => Err(anyhow!(
            "unix-socket forwarder regression `{label}` timed out after {SCENARIO_TIMEOUT:?}"
        )),
    }
}

#[cfg(unix)]
async fn run_unix_scenario(config_path: PathBuf) -> Result<()> {
    // Delete stale exposed sockets from prior runs.
    let _ = std::fs::remove_file(SOCK_DELAYED_RESPONSE_EXPOSED);
    let _ = std::fs::remove_file(SOCK_SINKHOLE_EXPOSED);

    let mut server = Instance::start("rathole server", config_path.clone(), run_rathole_server);
    time::sleep(Duration::from_millis(500)).await;
    let mut client = Instance::start("rathole client", config_path, run_rathole_client);
    time::sleep(Duration::from_millis(2500)).await;
    server.check_running().await?;
    client.check_running().await?;

    check_half_close_with_response_unix(SOCK_DELAYED_RESPONSE_EXPOSED).await?;
    check_stuck_peer_cleanup_unix(SOCK_SINKHOLE_EXPOSED).await?;

    client.stop().await?;
    server.stop().await?;
    time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

/// Render a TLS template with freshly generated certificates, run the
/// scenario against it, then remove the artifacts even if the scenario
/// failed. Gated like the call sites so it is never dead code.
#[cfg(any(
    all(target_os = "macos", feature = "rustls"),
    all(
        not(target_os = "macos"),
        any(feature = "native-tls", feature = "rustls")
    ),
))]
async fn run_for_transport_tls(
    config_template: &'static str,
    check_half_close_response: bool,
) -> Result<()> {
    let config = setup_tls_test_config(config_template).await?;
    let scenario_result =
        run_for_transport(config.path().to_path_buf(), check_half_close_response).await;
    let cleanup_result = config.close();
    match (scenario_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(cleanup_error)) => Err(cleanup_error).context("TLS artifact cleanup failed"),
        (Err(scenario_error), Ok(())) => Err(scenario_error),
        (Err(scenario_error), Err(cleanup_error)) => Err(scenario_error.context(format!(
            "TLS artifact cleanup also failed: {cleanup_error:#}"
        ))),
    }
}

/// Generate per-scenario TLS artifacts from a template config on a dedicated
/// thread (RSA key generation is CPU-bound) under its own deadline.
/// `common::tls::tests` expects this function at the crate root.
#[cfg(any(feature = "native-tls", feature = "rustls"))]
async fn setup_tls_test_config(
    config_template: impl AsRef<std::path::Path>,
) -> Result<common::tls::TlsTestConfig> {
    let template_path = config_template.as_ref().to_path_buf();
    let label = template_path.display().to_string();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("rathole-tls-test-setup".to_owned())
        .spawn(move || {
            let _ = result_tx.send(common::tls::TlsTestConfig::from_template(template_path));
        })
        .context("failed to spawn TLS artifact setup thread")?;

    match time::timeout(TLS_SETUP_TIMEOUT, result_rx).await {
        Ok(result) => result
            .context("TLS artifact setup thread panicked")?
            .with_context(|| format!("TLS artifact setup for `{label}` failed")),
        Err(_) => Err(anyhow!(
            "TLS artifact setup for `{label}` timed out after {TLS_SETUP_TIMEOUT:?}"
        )),
    }
}

/// Visitor sends a request, half-closes, then expects to read a response.
/// Under the broken `select!+copy` PR this would lose the response. Under the
/// two-phase forwarder this must succeed.
async fn check_half_close_with_response(addr: &str) -> Result<()> {
    info!("check half-close with response (TCP)");
    let mut conn = connect_with_retry_tcp(addr, Duration::from_secs(15)).await?;
    conn.write_all(b"REQ").await?;
    conn.shutdown().await?;

    let mut resp = [0u8; 4];
    let read = timeout(Duration::from_secs(5), conn.read_exact(&mut resp)).await;
    let r = read.map_err(|_| {
        anyhow!(
            "visitor read hung waiting for delayed response \u{2014} forwarder regressed half-close semantics"
        )
    })?;
    r.map_err(|e| anyhow!("visitor read errored: {e}"))?;
    assert_eq!(
        &resp, b"RESP",
        "expected RESP after half-close, got {:?}",
        resp
    );
    Ok(())
}

/// Visitor half-closes; sinkhole upstream never closes its write side. The
/// post-half-close idle timeout (1s in the toml) must reap the connection so
/// the visitor sees EOF promptly.
async fn check_stuck_peer_cleanup_tcp(addr: &str) -> Result<()> {
    info!("check stuck-peer cleanup (TCP)");
    let mut conn = connect_with_retry_tcp(addr, Duration::from_secs(15)).await?;
    conn.write_all(b"hello").await?;
    conn.shutdown().await?;

    let mut buf = [0u8; 16];
    let read = timeout(Duration::from_secs(5), conn.read(&mut buf)).await;
    let n = read
        .map_err(|_| {
            anyhow!("visitor read hung after half-close \u{2014} stuck-peer cleanup regressed")
        })?
        .map_err(|e| anyhow!("visitor read errored: {e}"))?;
    assert_eq!(n, 0, "expected EOF after stuck-peer cleanup, got {n} bytes");
    Ok(())
}

#[cfg(unix)]
async fn check_half_close_with_response_unix(addr: &str) -> Result<()> {
    info!("check half-close with response (unix)");
    let mut conn = connect_with_retry_unix(addr, Duration::from_secs(15)).await?;
    conn.write_all(b"REQ").await?;
    conn.shutdown().await?;

    let mut resp = [0u8; 4];
    let read = timeout(Duration::from_secs(5), conn.read_exact(&mut resp)).await;
    let r = read.map_err(|_| {
        anyhow!(
            "unix visitor read hung waiting for delayed response \u{2014} forwarder regressed half-close semantics"
        )
    })?;
    r.map_err(|e| anyhow!("unix visitor read errored: {e}"))?;
    assert_eq!(
        &resp, b"RESP",
        "expected RESP after half-close, got {:?}",
        resp
    );
    Ok(())
}

#[cfg(unix)]
async fn check_stuck_peer_cleanup_unix(addr: &str) -> Result<()> {
    info!("check stuck-peer cleanup (unix)");
    let mut conn = connect_with_retry_unix(addr, Duration::from_secs(15)).await?;
    conn.write_all(b"hello").await?;
    conn.shutdown().await?;

    let mut buf = [0u8; 16];
    let read = timeout(Duration::from_secs(5), conn.read(&mut buf)).await;
    let n = read
        .map_err(|_| {
            anyhow!("unix visitor read hung after half-close \u{2014} stuck-peer cleanup regressed")
        })?
        .map_err(|e| anyhow!("unix visitor read errored: {e}"))?;
    assert_eq!(n, 0, "expected EOF after stuck-peer cleanup, got {n} bytes");
    Ok(())
}

/// Connect with retry — the service listener on the rathole server side is
/// only bound after the client's control-channel handshake completes. On slow
/// runners a single connect attempt races the handshake.
async fn connect_with_retry_tcp(addr: &str, total: Duration) -> Result<TcpStream> {
    let deadline = tokio::time::Instant::now() + total;
    let mut last_err = None;
    while tokio::time::Instant::now() < deadline {
        match TcpStream::connect(addr).await {
            Ok(c) => return Ok(c),
            Err(e) => {
                last_err = Some(e);
                time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    Err(anyhow!(
        "failed to connect to {addr} within {:?}: {}",
        total,
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no error".into())
    ))
}

#[cfg(unix)]
async fn connect_with_retry_unix(addr: &str, total: Duration) -> Result<tokio::net::UnixStream> {
    let deadline = tokio::time::Instant::now() + total;
    let mut last_err = None;
    while tokio::time::Instant::now() < deadline {
        match tokio::net::UnixStream::connect(addr).await {
            Ok(c) => return Ok(c),
            Err(e) => {
                last_err = Some(e);
                time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    Err(anyhow!(
        "failed to connect to {addr} within {:?}: {}",
        total,
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no error".into())
    ))
}
