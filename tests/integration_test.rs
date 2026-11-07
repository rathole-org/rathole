use anyhow::{anyhow, Context, Result};
use common::{run_rathole_client, PING, PONG};
use rand::Rng;
use rand::RngCore;
use std::future::Future;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::ops::AsyncFnOnce;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{broadcast, oneshot},
    task::{JoinHandle, JoinSet},
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

// assume tmp directory exists (since sockets only work on unix systems this should be fine)
const ECHO_SERVER_SOCKET: &str = "/tmp/rathole_integration_test/echo.sock";
const PINGPONG_SERVER_SOCKET: &str = "/tmp/rathole_integration_test/pingpong.sock";
const ECHO_SERVER_SOCKET_EXPOSED: &str = "/tmp/rathole_integration_test/echo_exposed.sock";
const PINGPONG_SERVER_SOCKET_EXPOSED: &str = "/tmp/rathole_integration_test/pingpong_exposed.sock";

const HITTER_NUM: usize = 4;
const TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const INTEGRATION_SCENARIO_TIMEOUT: Duration = Duration::from_secs(60);
const CONTROL_CHANNEL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(any(feature = "native-tls", feature = "rustls"))]
const TLS_SETUP_TIMEOUT: Duration = Duration::from_secs(60);

const PP2_SIG: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

#[derive(Clone, Copy, Debug)]
enum Type {
    Tcp,
    Udp,
    #[cfg(unix)]
    SocketStream,
}

fn init() {
    let level = "info";
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from(level)),
        )
        .try_init();
}

fn reserve_tcp_addrs() -> Result<(SocketAddr, SocketAddr)> {
    let server = std::net::TcpListener::bind("127.0.0.1:0")?;
    let service = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok((server.local_addr()?, service.local_addr()?))
}

async fn wait_until_tcp_listener_is_bound(addr: SocketAddr) -> Result<()> {
    time::timeout(CONTROL_CHANNEL_CLEANUP_TIMEOUT, async {
        loop {
            match TcpListener::bind(addr).await {
                Ok(listener) => {
                    drop(listener);
                    time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) if error.kind() == ErrorKind::AddrInUse => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    })
    .await
    .context("service listener was not created before the deadline")?
}

async fn wait_until_tcp_listener_is_released(addr: SocketAddr) -> Result<TcpListener> {
    time::timeout(CONTROL_CHANNEL_CLEANUP_TIMEOUT, async {
        loop {
            match TcpListener::bind(addr).await {
                Ok(listener) => return Ok(listener),
                Err(error) if error.kind() == ErrorKind::AddrInUse => {
                    time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    })
    .await
    .context("service listener was not released before the deadline")?
}

#[tokio::test]
async fn disconnected_client_releases_listener_without_heartbeat() -> Result<()> {
    init();

    if cfg!(not(all(feature = "client", feature = "server"))) {
        return Ok(());
    }

    let (server_addr, service_addr) = reserve_tcp_addrs()?;
    let config_dir = tempfile::tempdir()?;
    let config_path = config_dir.path().join("control-channel-cleanup.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[client]
remote_addr = "{server_addr}"
default_token = "test-token"

[client.transport]
type = "tcp"

[client.services.cleanup]
local_addr = "127.0.0.1:1"

[server]
bind_addr = "{server_addr}"
default_token = "test-token"
heartbeat_interval = 0

[server.transport]
type = "tcp"

[server.services.cleanup]
bind_addr = "{service_addr}"
"#
        ),
    )?;

    run_managed_scenario(
        config_path,
        "control-channel cleanup scenario",
        INTEGRATION_SCENARIO_TIMEOUT,
        async move |processes| {
            processes.start_server();
            processes.start_client();
            wait_until_tcp_listener_is_bound(service_addr).await?;
            processes.check_running().await?;

            processes.stop_client().await?;
            let released_listener = wait_until_tcp_listener_is_released(service_addr).await?;
            check_task("server", &mut processes.server).await?;
            drop(released_listener);
            Ok(())
        },
    )
    .await
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
    test_tls("tests/for_tcp/tls_transport.toml", Type::Tcp).await?;

    #[cfg(feature = "noise")]
    test("tests/for_tcp/noise_transport.toml", Type::Tcp).await?;

    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test("tests/for_tcp/websocket_transport.toml", Type::Tcp).await?;

    #[cfg(not(target_os = "macos"))]
    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test_tls("tests/for_tcp/websocket_tls_transport.toml", Type::Tcp).await?;

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
    test_tls("tests/for_udp/tls_transport.toml", Type::Udp).await?;

    #[cfg(feature = "noise")]
    test("tests/for_udp/noise_transport.toml", Type::Udp).await?;

    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test("tests/for_udp/websocket_transport.toml", Type::Udp).await?;

    #[cfg(not(target_os = "macos"))]
    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test_tls("tests/for_udp/websocket_tls_transport.toml", Type::Udp).await?;

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn socket_stream() -> Result<()> {
    init();

    std::fs::remove_dir_all("/tmp/rathole_integration_test").ok();
    std::fs::create_dir_all("/tmp/rathole_integration_test").ok();

    // Spawn a echo server
    tokio::spawn(async move {
        if let Err(e) = common::socket_stream::echo_server(ECHO_SERVER_SOCKET).await {
            panic!("Failed to run the echo server for testing: {:?}", e);
        }
    });

    // Spawn a pingpong server
    tokio::spawn(async move {
        if let Err(e) = common::socket_stream::pingpong_server(PINGPONG_SERVER_SOCKET).await {
            panic!("Failed to run the pingpong server for testing: {:?}", e);
        }
    });

    test(
        "tests/for_socket_stream/tcp_transport.toml",
        Type::SocketStream,
    )
    .await?;

    #[cfg(any(
         // FIXME: Self-signed certificate on macOS nativetls requires manual interference.
         all(target_os = "macos", feature = "rustls"),
         // On other OS accept run with either
         all(not(target_os = "macos"), any(feature = "native-tls", feature = "rustls")),
     ))]
    test_tls(
        "tests/for_socket_stream/tls_transport.toml",
        Type::SocketStream,
    )
    .await?;

    #[cfg(feature = "noise")]
    test(
        "tests/for_socket_stream/noise_transport.toml",
        Type::SocketStream,
    )
    .await?;

    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test(
        "tests/for_socket_stream/websocket_transport.toml",
        Type::SocketStream,
    )
    .await?;

    #[cfg(not(target_os = "macos"))]
    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test_tls(
        "tests/for_socket_stream/websocket_tls_transport.toml",
        Type::SocketStream,
    )
    .await?;

    Ok(())
}

#[instrument]
async fn test(config_path: impl AsRef<Path> + std::fmt::Debug, t: Type) -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        return Ok(());
    }

    run_managed_scenario(
        config_path.as_ref().to_path_buf(),
        "traffic scenario",
        INTEGRATION_SCENARIO_TIMEOUT,
        async move |processes| run_scenario(processes, t).await,
    )
    .await
}

async fn run_managed_scenario(
    config_path: PathBuf,
    phase: &'static str,
    timeout: Duration,
    scenario: impl AsyncFnOnce(&mut TestProcesses) -> Result<()>,
) -> Result<()> {
    let mut processes = TestProcesses::new(config_path.clone());
    let scenario_result =
        run_with_timeout(&config_path, phase, timeout, scenario(&mut processes)).await;
    let cleanup_result = processes.shutdown().await;

    finish_with_cleanup(
        scenario_result,
        cleanup_result,
        "integration-test process cleanup",
    )
}

async fn run_with_timeout<T>(
    config_path: &Path,
    phase: &str,
    timeout: Duration,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    match time::timeout(timeout, operation).await {
        Ok(result) => result.with_context(|| {
            format!(
                "integration test `{}` failed during {phase}",
                config_path.display()
            )
        }),
        Err(_) => Err(anyhow!(
            "integration test `{}` timed out during {phase} after {timeout:?}",
            config_path.display()
        )),
    }
}

fn finish_with_cleanup(
    scenario_result: Result<()>,
    cleanup_result: Result<()>,
    cleanup_name: &str,
) -> Result<()> {
    match (scenario_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(cleanup_error)) => {
            Err(cleanup_error).with_context(|| format!("{cleanup_name} failed"))
        }
        (Err(scenario_error), Ok(())) => Err(scenario_error),
        (Err(scenario_error), Err(cleanup_error)) => {
            Err(scenario_error.context(format!("{cleanup_name} also failed: {cleanup_error:#}")))
        }
    }
}

async fn run_scenario(processes: &mut TestProcesses, t: Type) -> Result<()> {
    info!("start the client");
    processes.start_client();

    // Sleep for 1 second. Expect the client keep retrying to reach the server
    time::sleep(Duration::from_secs(1)).await;
    processes.check_client_running().await?;

    info!("start the server");
    processes.start_server();
    time::sleep(Duration::from_millis(2500)).await; // Wait for the client to retry
    processes.check_running().await?;

    info!("echo");
    echo_hitter(ECHO_SERVER_ADDR_EXPOSED, t).await?;
    info!("pingpong");
    pingpong_hitter(PINGPONG_SERVER_ADDR_EXPOSED, t).await?;

    // Simulate the client crash and restart
    info!("shutdown the client");
    processes.stop_client().await?;

    info!("restart the client");
    processes.start_client();
    time::sleep(Duration::from_secs(1)).await; // Wait for the client to start
    processes.check_running().await?;

    info!("echo");
    echo_hitter(ECHO_SERVER_ADDR_EXPOSED, t).await?;
    info!("pingpong");
    pingpong_hitter(PINGPONG_SERVER_ADDR_EXPOSED, t).await?;

    // Simulate the server crash and restart
    info!("shutdown the server");
    processes.stop_server().await?;

    info!("restart the server");
    processes.start_server();
    time::sleep(Duration::from_millis(2500)).await; // Wait for the client to retry
    processes.check_running().await?;

    // Simulate heavy load
    info!("lots of echo and pingpong");

    let mut hitters = JoinSet::new();

    for _ in 0..HITTER_NUM / 2 {
        hitters.spawn(async move { echo_hitter(ECHO_SERVER_ADDR_EXPOSED, t).await });

        hitters.spawn(async move { pingpong_hitter(PINGPONG_SERVER_ADDR_EXPOSED, t).await });
    }

    while let Some(result) = hitters.join_next().await {
        result.context("traffic task panicked")??;
    }

    Ok(())
}

struct TestProcesses {
    config_path: std::path::PathBuf,
    client_shutdown_tx: broadcast::Sender<bool>,
    server_shutdown_tx: broadcast::Sender<bool>,
    client: Option<JoinHandle<Result<()>>>,
    server: Option<JoinHandle<Result<()>>>,
}

impl TestProcesses {
    fn new(config_path: std::path::PathBuf) -> Self {
        let (client_shutdown_tx, _) = broadcast::channel(1);
        let (server_shutdown_tx, _) = broadcast::channel(1);
        Self {
            config_path,
            client_shutdown_tx,
            server_shutdown_tx,
            client: None,
            server: None,
        }
    }

    fn start_client(&mut self) {
        assert!(self.client.is_none(), "client is already running");
        let config_path = self.config_path.clone();
        let shutdown_rx = self.client_shutdown_tx.subscribe();
        self.client = Some(tokio::spawn(async move {
            run_rathole_client(config_path, shutdown_rx).await
        }));
    }

    fn start_server(&mut self) {
        assert!(self.server.is_none(), "server is already running");
        let config_path = self.config_path.clone();
        let shutdown_rx = self.server_shutdown_tx.subscribe();
        self.server = Some(tokio::spawn(async move {
            run_rathole_server(config_path, shutdown_rx).await
        }));
    }

    async fn stop_client(&mut self) -> Result<()> {
        stop_task(
            "client",
            &self.client_shutdown_tx,
            &mut self.client,
            TASK_SHUTDOWN_TIMEOUT,
        )
        .await
    }

    async fn stop_server(&mut self) -> Result<()> {
        stop_task(
            "server",
            &self.server_shutdown_tx,
            &mut self.server,
            TASK_SHUTDOWN_TIMEOUT,
        )
        .await
    }

    async fn check_running(&mut self) -> Result<()> {
        check_task("client", &mut self.client).await?;
        check_task("server", &mut self.server).await
    }

    async fn check_client_running(&mut self) -> Result<()> {
        check_task("client", &mut self.client).await
    }

    async fn shutdown(&mut self) -> Result<()> {
        info!("shutdown the server and the client");
        // Signal both sides before waiting because either side can be blocked on
        // work owned by the other.
        let _ = self.server_shutdown_tx.send(true);
        let _ = self.client_shutdown_tx.send(true);
        let server_result = self.stop_server().await;
        let client_result = self.stop_client().await;
        match (server_result, client_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(server_error), Err(client_error)) => Err(anyhow!(
                "server cleanup failed: {server_error:#}; client cleanup failed: {client_error:#}"
            )),
        }
    }
}

impl Drop for TestProcesses {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            client.abort();
        }
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

async fn check_task(name: &str, task: &mut Option<JoinHandle<Result<()>>>) -> Result<()> {
    let Some(running_task) = task.as_ref() else {
        return Err(anyhow!("{name} is not running"));
    };
    if !running_task.is_finished() {
        return Ok(());
    }

    let result = task
        .take()
        .expect("finished task should exist")
        .await
        .with_context(|| format!("{name} task panicked"))?;
    match result {
        Ok(()) => Err(anyhow!("{name} exited unexpectedly")),
        Err(error) => Err(error).with_context(|| format!("{name} exited unexpectedly")),
    }
}

async fn stop_task(
    name: &str,
    shutdown_tx: &broadcast::Sender<bool>,
    task: &mut Option<JoinHandle<Result<()>>>,
    shutdown_timeout: Duration,
) -> Result<()> {
    let Some(running_task) = task.as_mut() else {
        return Ok(());
    };

    let _ = shutdown_tx.send(true);
    match time::timeout(shutdown_timeout, running_task).await {
        Ok(join_result) => {
            task.take();
            join_result
                .with_context(|| format!("{name} task panicked"))?
                .with_context(|| format!("{name} failed while shutting down"))
        }
        Err(_) => {
            let task = task.take().expect("running task should exist");
            task.abort();
            Err(anyhow!("{name} did not stop within {shutdown_timeout:?}"))
        }
    }
}

struct NotifyOnDrop(Option<oneshot::Sender<()>>);

impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

#[tokio::test]
async fn stop_task_aborts_an_unresponsive_task() {
    let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);
    let (started_tx, started_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let mut task = Some(tokio::spawn(async move {
        let _notify_on_drop = NotifyOnDrop(Some(dropped_tx));
        let _ = started_tx.send(());
        let _ = shutdown_rx.recv().await;
        std::future::pending::<Result<()>>().await
    }));

    time::timeout(Duration::from_secs(1), started_rx)
        .await
        .expect("test task should start promptly")
        .expect("test task should report startup");

    let result = stop_task(
        "test task",
        &shutdown_tx,
        &mut task,
        Duration::from_millis(10),
    )
    .await;

    assert!(result.is_err());
    assert!(task.is_none());
    time::timeout(Duration::from_secs(1), dropped_rx)
        .await
        .expect("aborted task should be dropped promptly")
        .expect("drop notification sender should survive until task cleanup");
}

#[tokio::test]
async fn cancelling_stop_keeps_the_task_managed() {
    let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);
    let mut task = Some(tokio::spawn(async move {
        let _ = shutdown_rx.recv().await;
        std::future::pending::<Result<()>>().await
    }));

    let result = time::timeout(
        Duration::from_millis(10),
        stop_task("test task", &shutdown_tx, &mut task, Duration::from_secs(1)),
    )
    .await;

    assert!(result.is_err());
    assert!(task.is_some());

    let result = stop_task(
        "test task",
        &shutdown_tx,
        &mut task,
        Duration::from_millis(10),
    )
    .await;
    assert!(result.is_err());
    assert!(task.is_none());
}

#[tokio::test]
async fn scenario_timeout_reports_phase_and_cleans_up_tasks() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let shutdown_count = Arc::new(AtomicUsize::new(0));
    let scenario_shutdown_count = Arc::clone(&shutdown_count);
    let result = run_managed_scenario(
        PathBuf::from("tests/stalled-traffic.toml"),
        "stalled traffic scenario",
        Duration::from_millis(10),
        async move |processes| {
            let mut client_shutdown_rx = processes.client_shutdown_tx.subscribe();
            let client_shutdown_count = Arc::clone(&scenario_shutdown_count);
            processes.client = Some(tokio::spawn(async move {
                // shutdown() sends twice through a one-slot channel. Production
                // shutdown receivers finish on either a value or a lag error.
                assert!(matches!(
                    client_shutdown_rx.recv().await,
                    Ok(true) | Err(broadcast::error::RecvError::Lagged(_))
                ));
                client_shutdown_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }));

            let mut server_shutdown_rx = processes.server_shutdown_tx.subscribe();
            let server_shutdown_count = Arc::clone(&scenario_shutdown_count);
            processes.server = Some(tokio::spawn(async move {
                // See the matching client task above.
                assert!(matches!(
                    server_shutdown_rx.recv().await,
                    Ok(true) | Err(broadcast::error::RecvError::Lagged(_))
                ));
                server_shutdown_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }));

            std::future::pending::<Result<()>>().await
        },
    )
    .await;

    let error = format!(
        "{:#}",
        result.expect_err("stalled scenario should time out")
    );
    assert!(error.contains("tests/stalled-traffic.toml"));
    assert!(error.contains("stalled traffic scenario"));
    assert!(error.contains("10ms"));
    assert!(!error.contains("cleanup"));
    assert_eq!(shutdown_count.load(Ordering::SeqCst), 2);
}

#[cfg(any(feature = "native-tls", feature = "rustls"))]
async fn test_tls(config_template: impl AsRef<Path>, t: Type) -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        return Ok(());
    }

    let config = setup_tls_test_config(config_template).await?;
    let scenario_result = test(config.path(), t).await;
    let cleanup_result = config.close();
    finish_with_cleanup(scenario_result, cleanup_result, "TLS artifact cleanup")
}

#[cfg(any(feature = "native-tls", feature = "rustls"))]
async fn setup_tls_test_config(
    config_template: impl AsRef<Path>,
) -> Result<common::tls::TlsTestConfig> {
    let template_path = config_template.as_ref().to_path_buf();
    let generation_path = template_path.clone();
    run_with_timeout(
        &template_path,
        "TLS artifact setup",
        TLS_SETUP_TIMEOUT,
        generate_tls_test_config(generation_path),
    )
    .await
}

#[cfg(any(feature = "native-tls", feature = "rustls"))]
async fn generate_tls_test_config(template_path: PathBuf) -> Result<common::tls::TlsTestConfig> {
    let (result_tx, result_rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("rathole-tls-test-setup".to_owned())
        .spawn(move || {
            let result = common::tls::TlsTestConfig::from_template(template_path);
            let _ = result_tx.send(result);
        })
        .context("failed to spawn TLS artifact setup thread")?;

    result_rx
        .await
        .context("TLS artifact setup thread panicked")?
}

async fn echo_hitter(addr: &'static str, t: Type) -> Result<()> {
    match t {
        Type::Tcp => tcp_echo_hitter(addr).await,
        Type::Udp => udp_echo_hitter(addr).await,
        #[cfg(unix)]
        Type::SocketStream => socket_stream_echo_hitter(ECHO_SERVER_SOCKET_EXPOSED).await,
    }
}

async fn pingpong_hitter(addr: &'static str, t: Type) -> Result<()> {
    match t {
        Type::Tcp => tcp_pingpong_hitter(addr).await,
        Type::Udp => udp_pingpong_hitter(addr).await,
        #[cfg(unix)]
        Type::SocketStream => socket_stream_pingpong_hitter(PINGPONG_SERVER_SOCKET_EXPOSED).await,
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

#[cfg(unix)]
async fn socket_stream_echo_hitter(addr: &'static str) -> Result<()> {
    use tokio::net::UnixStream;
    use tracing::warn;

    while !std::path::Path::new(addr).exists() {
        warn!("waiting for socket {} to be created", addr);
        time::sleep(Duration::from_millis(500)).await;
    }
    let mut conn = UnixStream::connect(addr).await?;

    let mut wr = [0u8; 1024];
    let mut rd = [0u8; 1024];
    for _ in 0..100 {
        rand::thread_rng().fill(&mut wr);
        conn.write_all(&wr).await?;
        conn.read_exact(&mut rd).await?;
        assert_eq!(wr, rd);
    }
    conn.shutdown().await?;

    Ok(())
}

#[cfg(unix)]
async fn socket_stream_pingpong_hitter(addr: &'static str) -> Result<()> {
    use tokio::net::UnixStream;
    let mut conn = UnixStream::connect(addr).await?;

    let wr = PING.as_bytes();
    let mut rd = [0u8; PONG.len()];

    for _ in 0..100 {
        conn.write_all(wr).await?;
        conn.read_exact(&mut rd).await?;
        assert_eq!(rd, PONG.as_bytes());
    }
    conn.shutdown().await?;

    Ok(())
}

#[instrument]
async fn test_proxy_protocol(config_path: &'static str) -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        return Ok(());
    }

    run_managed_scenario(
        config_path.into(),
        "proxy-protocol traffic scenario",
        INTEGRATION_SCENARIO_TIMEOUT,
        async |processes| {
            info!("start the client");
            processes.start_client();
            time::sleep(Duration::from_secs(1)).await;
            processes.check_client_running().await?;

            info!("start the server");
            processes.start_server();
            time::sleep(Duration::from_millis(2500)).await;
            processes.check_running().await?;

            info!("echo");
            tcp_echo_hitter_expect_proxy_protocol(ECHO_SERVER_ADDR_EXPOSED).await?;

            info!("pingpong");
            tcp_pingpong_hitter(PINGPONG_SERVER_ADDR_EXPOSED).await
        },
    )
    .await
}

async fn read_proxy_protocol_header(
    rd: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Result<Vec<u8>> {
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

            let src = IpAddr::V4(Ipv4Addr::new(
                header[16], header[17], header[18], header[19],
            ));
            let dst = IpAddr::V4(Ipv4Addr::new(
                header[20], header[21], header[22], header[23],
            ));
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
