use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::path::Path;
use tokio::fs;
use url::Url;

use crate::transport::{DEFAULT_KEEPALIVE_INTERVAL, DEFAULT_KEEPALIVE_SECS, DEFAULT_NODELAY};

/// Application-layer heartbeat interval in secs
const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 30;
const DEFAULT_HEARTBEAT_TIMEOUT_SECS: u64 = 40;

/// Client
const DEFAULT_CLIENT_RETRY_INTERVAL_SECS: u64 = 1;

/// String with Debug implementation that emits "MASKED"
/// Used to mask sensitive strings when logging
#[derive(Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
pub struct MaskedString(String);

impl Debug for MaskedString {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.write_str("MASKED")
    }
}

impl Deref for MaskedString {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<&str> for MaskedString {
    fn from(s: &str) -> MaskedString {
        MaskedString(String::from(s))
    }
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone, PartialEq, Eq, Default)]
pub enum TransportType {
    #[default]
    #[serde(rename = "tcp")]
    Tcp,
    #[serde(rename = "tls")]
    Tls,
    #[serde(rename = "noise")]
    Noise,
    #[serde(rename = "websocket")]
    Websocket,
}

/// Per service config
/// All Option are optional in configuration but must be Some value in runtime
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ClientServiceConfig {
    #[serde(rename = "type", default = "default_service_type")]
    pub service_type: ServiceType,
    #[serde(skip)]
    pub name: String,
    pub local_addr: String,
    pub token: Option<MaskedString>,
    pub nodelay: Option<bool>,
    pub retry_interval: Option<u64>,
}

impl ClientServiceConfig {
    pub fn with_name(name: &str) -> ClientServiceConfig {
        ClientServiceConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceType {
    #[serde(rename = "tcp")]
    #[default]
    Tcp,
    #[serde(rename = "udp")]
    Udp,
}

fn default_service_type() -> ServiceType {
    Default::default()
}

/// Per service config
/// All Option are optional in configuration but must be Some value in runtime
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerServiceConfig {
    #[serde(rename = "type", default = "default_service_type")]
    pub service_type: ServiceType,
    #[serde(skip)]
    pub name: String,
    pub bind_addr: String,
    pub token: Option<MaskedString>,
    pub nodelay: Option<bool>,
    pub enable_proxy_protocol: Option<bool>,
}

impl ServerServiceConfig {
    pub fn with_name(name: &str) -> ServerServiceConfig {
        ServerServiceConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub hostname: Option<String>,
    pub trusted_root: Option<String>,
    pub pkcs12: Option<String>,
    pub pkcs12_password: Option<MaskedString>,
}

fn default_noise_pattern() -> String {
    String::from("Noise_NK_25519_ChaChaPoly_BLAKE2s")
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NoiseConfig {
    #[serde(default = "default_noise_pattern")]
    pub pattern: String,
    pub local_private_key: Option<MaskedString>,
    pub remote_public_key: Option<String>,
    // TODO: Maybe psk can be added
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WebsocketConfig {
    pub tls: bool,
}

fn default_nodelay() -> bool {
    DEFAULT_NODELAY
}

fn default_keepalive_secs() -> u64 {
    DEFAULT_KEEPALIVE_SECS
}

fn default_keepalive_interval() -> u64 {
    DEFAULT_KEEPALIVE_INTERVAL
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TcpConfig {
    #[serde(default = "default_nodelay")]
    pub nodelay: bool,
    #[serde(default = "default_keepalive_secs")]
    pub keepalive_secs: u64,
    #[serde(default = "default_keepalive_interval")]
    pub keepalive_interval: u64,
    pub proxy: Option<Url>,
}

impl Default for TcpConfig {
    fn default() -> Self {
        Self {
            nodelay: default_nodelay(),
            keepalive_secs: default_keepalive_secs(),
            keepalive_interval: default_keepalive_interval(),
            proxy: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct TransportConfig {
    #[serde(rename = "type")]
    pub transport_type: TransportType,
    #[serde(default)]
    pub tcp: TcpConfig,
    pub tls: Option<TlsConfig>,
    pub noise: Option<NoiseConfig>,
    pub websocket: Option<WebsocketConfig>,
}

fn default_heartbeat_timeout() -> u64 {
    DEFAULT_HEARTBEAT_TIMEOUT_SECS
}

fn default_client_retry_interval() -> u64 {
    DEFAULT_CLIENT_RETRY_INTERVAL_SECS
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub remote_addr: String,
    pub default_token: Option<MaskedString>,
    pub services: HashMap<String, ClientServiceConfig>,
    #[serde(default)]
    pub transport: TransportConfig,
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout: u64,
    #[serde(default = "default_client_retry_interval")]
    pub retry_interval: u64,
}

fn default_heartbeat_interval() -> u64 {
    DEFAULT_HEARTBEAT_INTERVAL_SECS
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub bind_addr: String,
    pub default_token: Option<MaskedString>,
    pub services: HashMap<String, ServerServiceConfig>,
    #[serde(default)]
    pub transport: TransportConfig,
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval: u64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: Option<ServerConfig>,
    pub client: Option<ClientConfig>,
}

impl Config {
    fn from_str(s: &str) -> Result<Config> {
        let mut config: Config = toml::from_str(s).with_context(|| "Failed to parse the config")?;

        if let Some(server) = config.server.as_mut() {
            Config::validate_server_config(server)?;
        }

        if let Some(client) = config.client.as_mut() {
            Config::validate_client_config(client)?;
        }

        if config.server.is_none() && config.client.is_none() {
            Err(anyhow!("Neither of `[server]` or `[client]` is defined"))
        } else {
            Ok(config)
        }
    }

    fn validate_server_config(server: &mut ServerConfig) -> Result<()> {
        // Validate services
        for (name, s) in &mut server.services {
            s.name = name.clone();
            if s.token.is_none() {
                s.token = server.default_token.clone();
                if s.token.is_none() {
                    bail!("The token of service {} is not set", name);
                }
            }
        }

        Config::validate_transport_config(&server.transport, true)?;

        Ok(())
    }

    fn validate_client_config(client: &mut ClientConfig) -> Result<()> {
        // Validate services
        for (name, s) in &mut client.services {
            s.name = name.clone();
            if s.token.is_none() {
                s.token = client.default_token.clone();
                if s.token.is_none() {
                    bail!("The token of service {} is not set", name);
                }
            }
            if s.retry_interval.is_none() {
                s.retry_interval = Some(client.retry_interval);
            }
        }

        Config::validate_transport_config(&client.transport, false)?;

        Ok(())
    }

    fn validate_transport_config(config: &TransportConfig, is_server: bool) -> Result<()> {
        config
            .tcp
            .proxy
            .as_ref()
            .map_or(Ok(()), |u| match u.scheme() {
                "socks5" => Ok(()),
                "http" => Ok(()),
                _ => Err(anyhow!(format!("Unknown proxy scheme: {}", u.scheme()))),
            })?;
        match config.transport_type {
            TransportType::Tcp => Ok(()),
            TransportType::Tls => {
                let tls_config = config
                    .tls
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing TLS configuration"))?;
                if is_server {
                    tls_config
                        .pkcs12
                        .as_ref()
                        .and(tls_config.pkcs12_password.as_ref())
                        .ok_or_else(|| anyhow!("Missing `pkcs12` or `pkcs12_password`"))?;
                }
                Ok(())
            }
            TransportType::Noise => {
                // The check is done in transport
                Ok(())
            }
            TransportType::Websocket => Ok(()),
        }
    }

    pub async fn from_file(path: &Path) -> Result<Config> {
        let s: String = fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read the config {:?}", path))?;
        Config::from_str(&s).with_context(|| {
            "Configuration is invalid. Please refer to the configuration specification."
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    use anyhow::Result;

    fn list_config_files<T: AsRef<Path>>(root: T) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            } else if path.is_dir() {
                files.append(&mut list_config_files(path)?);
            }
        }
        Ok(files)
    }

    fn get_all_example_config() -> Result<Vec<PathBuf>> {
        Ok(list_config_files("./examples")?
            .into_iter()
            .filter(|x| x.ends_with(".toml"))
            .collect())
    }

    #[test]
    fn test_example_config() -> Result<()> {
        let paths = get_all_example_config()?;
        for p in paths {
            let s = fs::read_to_string(p)?;
            Config::from_str(&s)?;
        }
        Ok(())
    }

    #[test]
    fn test_valid_config() -> Result<()> {
        let paths = list_config_files("tests/config_test/valid_config")?;
        for p in paths {
            let s = fs::read_to_string(p)?;
            Config::from_str(&s)?;
        }
        Ok(())
    }

    #[test]
    fn test_invalid_config() -> Result<()> {
        let paths = list_config_files("tests/config_test/invalid_config")?;
        for p in paths {
            let s = fs::read_to_string(p)?;
            assert!(Config::from_str(&s).is_err());
        }
        Ok(())
    }

    #[test]
    fn test_validate_server_config() -> Result<()> {
        let mut cfg = ServerConfig::default();

        cfg.services.insert(
            "foo1".into(),
            ServerServiceConfig {
                service_type: ServiceType::Tcp,
                name: "foo1".into(),
                bind_addr: "127.0.0.1:80".into(),
                token: None,
                ..Default::default()
            },
        );

        // Missing the token
        assert!(Config::validate_server_config(&mut cfg).is_err());

        // Use the default token
        cfg.default_token = Some("123".into());
        assert!(Config::validate_server_config(&mut cfg).is_ok());
        assert_eq!(
            cfg.services
                .get("foo1")
                .as_ref()
                .unwrap()
                .token
                .as_ref()
                .unwrap()
                .0,
            "123"
        );

        // The default token won't override the service token
        cfg.services.get_mut("foo1").unwrap().token = Some("4".into());
        assert!(Config::validate_server_config(&mut cfg).is_ok());
        assert_eq!(
            cfg.services
                .get("foo1")
                .as_ref()
                .unwrap()
                .token
                .as_ref()
                .unwrap()
                .0,
            "4"
        );
        Ok(())
    }

    #[test]
    fn test_validate_client_config() -> Result<()> {
        let mut cfg = ClientConfig::default();

        cfg.services.insert(
            "foo1".into(),
            ClientServiceConfig {
                service_type: ServiceType::Tcp,
                name: "foo1".into(),
                local_addr: "127.0.0.1:80".into(),
                token: None,
                ..Default::default()
            },
        );

        // Missing the token
        assert!(Config::validate_client_config(&mut cfg).is_err());

        // Use the default token
        cfg.default_token = Some("123".into());
        assert!(Config::validate_client_config(&mut cfg).is_ok());
        assert_eq!(
            cfg.services
                .get("foo1")
                .as_ref()
                .unwrap()
                .token
                .as_ref()
                .unwrap()
                .0,
            "123"
        );

        // The default token won't override the service token
        cfg.services.get_mut("foo1").unwrap().token = Some("4".into());
        assert!(Config::validate_client_config(&mut cfg).is_ok());
        assert_eq!(
            cfg.services
                .get("foo1")
                .as_ref()
                .unwrap()
                .token
                .as_ref()
                .unwrap()
                .0,
            "4"
        );
        Ok(())
    }
}
