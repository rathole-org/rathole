#![allow(dead_code, unused, unused_imports)]
#[cfg(not(any(feature = "client", feature = "server")))]
compile_error!("Enable at least one of features `client` or `server`.");

mod cli;
mod config;
mod config_watcher;
mod constants;
mod helper;
mod multi_map;
mod protocol;
mod transport;

pub use cli::Cli;
use cli::KeypairType;
pub use config::Config;
pub use constants::UDP_BUFFER_SIZE;

use anyhow::{anyhow, Context, Result};
use tokio::{
    sync::{broadcast, mpsc},
    task::{JoinError, JoinSet},
};
use tracing::{debug, info};

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
use client::run_client;

#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
use server::run_server;

use crate::config_watcher::{ConfigChange, ConfigWatcherHandle};

const DEFAULT_CURVE: KeypairType = KeypairType::X25519;

fn get_str_from_keypair_type(curve: KeypairType) -> &'static str {
    match curve {
        KeypairType::X25519 => "25519",
        KeypairType::X448 => "448",
    }
}

#[cfg(feature = "noise")]
fn genkey(curve: Option<KeypairType>) -> Result<()> {
    let curve = curve.unwrap_or(DEFAULT_CURVE);
    let builder = snowstorm::Builder::new(
        format!(
            "Noise_KK_{}_ChaChaPoly_BLAKE2s",
            get_str_from_keypair_type(curve)
        )
        .parse()?,
    );
    let keypair = builder.generate_keypair()?;

    println!("Private Key:\n{}\n", base64::encode(keypair.private));
    println!("Public Key:\n{}", base64::encode(keypair.public));
    Ok(())
}

#[cfg(not(feature = "noise"))]
fn genkey(curve: Option<KeypairType>) -> Result<()> {
    crate::helper::feature_not_compile("nosie")
}

pub async fn run(args: Cli, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    if let Some(curve) = args.genkey {
        return genkey(curve);
    }

    // Raise `nofile` limit on linux and mac
    fdlimit::raise_fd_limit();

    // Spawn a config watcher. The watcher will send a initial signal to start the instance with a config
    let config_path = args.config_path.as_ref().unwrap();
    let mut cfg_watcher = ConfigWatcherHandle::new(config_path, shutdown_rx).await?;

    // shutdown_tx owns the instance
    let (shutdown_tx, _) = broadcast::channel(1);

    // Exactly one instance can be active. JoinSet lets the outer loop observe
    // an instance failure without waiting for another configuration event.
    let mut instance_tasks = JoinSet::new();
    let mut service_update_tx = None;

    loop {
        tokio::select! {
            event = cfg_watcher.event_rx.recv() => {
                match event {
                    Some(ConfigChange::General(config)) => {
                        if !instance_tasks.is_empty() {
                            info!("General configuration change detected. Restarting...");
                            stop_active_instance(&shutdown_tx, &mut instance_tasks).await?;
                        }

                        debug!("{:?}", config);

                        let (update_tx, update_rx) = mpsc::channel(1024);
                        instance_tasks.spawn(run_instance(
                            *config,
                            args.clone(),
                            shutdown_tx.subscribe(),
                            update_rx,
                        ));
                        service_update_tx = Some(update_tx);
                    }
                    Some(event) => {
                        info!("Service change detected. {:?}", event);
                        if let Some(update_tx) = &service_update_tx {
                            let _ = update_tx.send(event).await;
                        }
                    }
                    None => {
                        let watcher_result = cfg_watcher.wait().await;
                        let instance_result =
                            stop_active_instance(&shutdown_tx, &mut instance_tasks).await;
                        return finish_shutdown(watcher_result, instance_result);
                    }
                }
            }
            instance_result = instance_tasks.join_next(), if !instance_tasks.is_empty() => {
                return unexpected_instance_exit(
                    instance_result.ok_or_else(|| anyhow!("active instance task disappeared"))?,
                );
            }
        }
    }
}

fn finish_shutdown(watcher_result: Result<()>, instance_result: Result<()>) -> Result<()> {
    match (watcher_result, instance_result) {
        (Ok(()), result) => result,
        (Err(watcher_error), Ok(())) => Err(watcher_error).context("configuration watcher failed"),
        (Err(watcher_error), Err(instance_error)) => Err(instance_error).context(format!(
            "configuration watcher also failed: {watcher_error:#}"
        )),
    }
}

async fn stop_active_instance(
    shutdown_tx: &broadcast::Sender<bool>,
    instance_tasks: &mut JoinSet<Result<()>>,
) -> Result<()> {
    if instance_tasks.is_empty() {
        return Ok(());
    }

    // A failed instance may already have dropped its receiver. Awaiting the
    // task below preserves that failure instead of returning "channel closed".
    let shutdown_sent = shutdown_tx.send(true).is_ok();
    let instance_result = instance_tasks
        .join_next()
        .await
        .ok_or_else(|| anyhow!("active instance task disappeared while shutting down"))?
        .context("active instance task panicked")?;

    match instance_result {
        Ok(()) if shutdown_sent => Ok(()),
        Ok(()) => Err(anyhow!(
            "active instance exited before receiving the shutdown signal"
        )),
        Err(error) => Err(error).context("active instance failed while shutting down"),
    }
}

fn unexpected_instance_exit(
    instance_result: std::result::Result<Result<()>, JoinError>,
) -> Result<()> {
    match instance_result.context("active instance task panicked")? {
        Ok(()) => Err(anyhow!("active instance exited unexpectedly")),
        Err(error) => Err(error).context("active instance exited unexpectedly"),
    }
}

async fn run_instance(
    config: Config,
    args: Cli,
    shutdown_rx: broadcast::Receiver<bool>,
    service_update: mpsc::Receiver<ConfigChange>,
) -> Result<()> {
    match determine_run_mode(&config, &args) {
        RunMode::Undetermine => panic!("Cannot determine running as a server or a client"),
        RunMode::Client => {
            #[cfg(not(feature = "client"))]
            crate::helper::feature_not_compile("client");
            #[cfg(feature = "client")]
            run_client(config, shutdown_rx, service_update).await
        }
        RunMode::Server => {
            #[cfg(not(feature = "server"))]
            crate::helper::feature_not_compile("server");
            #[cfg(feature = "server")]
            run_server(config, shutdown_rx, service_update).await
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum RunMode {
    Server,
    Client,
    Undetermine,
}

fn determine_run_mode(config: &Config, args: &Cli) -> RunMode {
    use RunMode::*;
    if args.client && args.server {
        Undetermine
    } else if args.client {
        Client
    } else if args.server {
        Server
    } else if config.client.is_some() && config.server.is_none() {
        Client
    } else if config.server.is_some() && config.client.is_none() {
        Server
    } else {
        Undetermine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "server")]
    fn server_args(bind_addr: std::net::SocketAddr) -> (tempfile::TempDir, Cli) {
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("server.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"[server]
bind_addr = "{bind_addr}"

[server.transport]
type = "tcp"

[server.services.test]
bind_addr = "127.0.0.1:0"
token = "test-token"
"#,
            ),
        )
        .unwrap();

        let args = Cli {
            config_path: Some(config_path),
            server: true,
            ..Default::default()
        };
        (config_dir, args)
    }

    #[test]
    fn test_determine_run_mode() {
        use config::*;
        use RunMode::*;

        struct T {
            cfg_s: bool,
            cfg_c: bool,
            arg_s: bool,
            arg_c: bool,
            run_mode: RunMode,
        }

        let tests = [
            T {
                cfg_s: false,
                cfg_c: false,
                arg_s: false,
                arg_c: false,
                run_mode: Undetermine,
            },
            T {
                cfg_s: true,
                cfg_c: false,
                arg_s: false,
                arg_c: false,
                run_mode: Server,
            },
            T {
                cfg_s: false,
                cfg_c: true,
                arg_s: false,
                arg_c: false,
                run_mode: Client,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: false,
                arg_c: false,
                run_mode: Undetermine,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: true,
                arg_c: false,
                run_mode: Server,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: false,
                arg_c: true,
                run_mode: Client,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: true,
                arg_c: true,
                run_mode: Undetermine,
            },
        ];

        for t in tests {
            let config = Config {
                server: match t.cfg_s {
                    true => Some(ServerConfig::default()),
                    false => None,
                },
                client: match t.cfg_c {
                    true => Some(ClientConfig::default()),
                    false => None,
                },
            };

            let args = Cli {
                config_path: Some(std::path::PathBuf::new()),
                server: t.arg_s,
                client: t.arg_c,
                ..Default::default()
            };

            assert_eq!(determine_run_mode(&config, &args), t.run_mode);
        }
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn run_surfaces_instance_startup_failure() {
        use std::time::Duration;
        use tokio::{net::TcpListener, time};

        let occupied_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let occupied_addr = occupied_listener.local_addr().unwrap();
        let (_shutdown_tx, shutdown_rx) = broadcast::channel(1);
        let (_config_dir, args) = server_args(occupied_addr);

        let result = time::timeout(Duration::from_secs(5), run(args, shutdown_rx))
            .await
            .expect("startup failure should be reported promptly");
        let error = format!(
            "{:#}",
            result.expect_err("an occupied listener must fail startup")
        );
        assert!(
            error.contains("active instance exited unexpectedly"),
            "{error}"
        );
        assert!(
            error.contains("Failed to listen at `server.bind_addr`"),
            "{error}"
        );
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn run_releases_listener_before_shutdown_returns() {
        use std::time::Duration;
        use tokio::{net::TcpListener, net::TcpStream, time};

        let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind_addr = reservation.local_addr().unwrap();
        drop(reservation);

        let (_config_dir, args) = server_args(bind_addr);
        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
        let run_task = tokio::spawn(run(args, shutdown_rx));

        time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(stream) = TcpStream::connect(bind_addr).await {
                    drop(stream);
                    break;
                }
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("server listener should start promptly");

        shutdown_tx.send(true).unwrap();
        time::timeout(Duration::from_secs(5), run_task)
            .await
            .expect("shutdown should complete promptly")
            .expect("rathole task should not panic")
            .expect("rathole should shut down cleanly");

        TcpListener::bind(bind_addr)
            .await
            .expect("listener must be released before shutdown returns");
    }

    #[tokio::test]
    async fn shutdown_waits_for_the_active_instance() {
        use std::time::Duration;
        use tokio::{sync::oneshot, time};

        let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);
        let (finish_tx, finish_rx) = oneshot::channel();
        let mut instance_tasks = JoinSet::new();
        instance_tasks.spawn(async move {
            shutdown_rx.recv().await.unwrap();
            finish_rx.await.unwrap();
            Ok(())
        });

        let mut stopping = Box::pin(stop_active_instance(&shutdown_tx, &mut instance_tasks));
        assert!(
            time::timeout(Duration::from_millis(25), &mut stopping)
                .await
                .is_err(),
            "shutdown returned before the active instance finished"
        );

        finish_tx.send(()).unwrap();
        time::timeout(Duration::from_secs(1), stopping)
            .await
            .expect("shutdown should finish promptly after the instance exits")
            .unwrap();
    }

    #[tokio::test]
    async fn restart_does_not_overlap_instances() {
        use std::time::Duration;
        use tokio::{sync::oneshot, time};

        let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);
        let (finish_tx, finish_rx) = oneshot::channel();
        let (replacement_started_tx, mut replacement_started_rx) = oneshot::channel();
        let mut instance_tasks = JoinSet::new();
        instance_tasks.spawn(async move {
            shutdown_rx.recv().await.unwrap();
            finish_rx.await.unwrap();
            Ok(())
        });

        let mut restarting = Box::pin(async {
            stop_active_instance(&shutdown_tx, &mut instance_tasks).await?;
            instance_tasks.spawn(async move {
                replacement_started_tx.send(()).unwrap();
                Ok(())
            });
            Result::<()>::Ok(())
        });

        assert!(
            time::timeout(Duration::from_millis(25), &mut restarting)
                .await
                .is_err(),
            "restart completed before the old instance finished"
        );
        assert!(
            matches!(
                replacement_started_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "replacement started while the old instance was active"
        );

        finish_tx.send(()).unwrap();
        time::timeout(Duration::from_secs(1), restarting)
            .await
            .expect("restart should continue after the old instance exits")
            .unwrap();
        replacement_started_rx
            .await
            .expect("replacement instance should start");
    }
}
