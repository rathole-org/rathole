use std::net::SocketAddr;

use super::{AddrMaybeCached, SocketOpts, TcpTransport, Transport, TransportRole};
use crate::config::{NoiseConfig, TransportConfig};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use snowstorm::snow::resolvers::{CryptoResolver, DefaultResolver};
use snowstorm::{Builder, NoiseParams, NoiseStream};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

/// How the pattern's initiator (the rathole client) presents its static key,
/// from the first letter of the pattern name (`Noise_KK_...` -> `K`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitiatorStatic {
    /// `N`: the client has no static key, so the server can't authenticate it
    None,
    /// `K`: the server must know the client key before the first message
    Known,
    /// `X` or `I`: the client sends its key during the handshake
    Transmitted,
}

impl InitiatorStatic {
    fn from_pattern(pattern: &str) -> Result<InitiatorStatic> {
        let name = pattern
            .split('_')
            .nth(1)
            .ok_or_else(|| anyhow!("Malformed noise pattern {}", pattern))?;
        match name.chars().next() {
            Some('N') => Ok(InitiatorStatic::None),
            Some('K') => Ok(InitiatorStatic::Known),
            Some('X') | Some('I') => Ok(InitiatorStatic::Transmitted),
            _ => bail!("Unsupported noise handshake pattern {}", name),
        }
    }
}

pub struct NoiseTransport {
    tcp: TcpTransport,
    config: NoiseConfig,
    params: NoiseParams,
    local_private_key: Vec<u8>,
    /// Client: the server's key (at most one). Server: the allowed client keys.
    remote_public_keys: Vec<Vec<u8>>,
    initiator_static: InitiatorStatic,
}

impl std::fmt::Debug for NoiseTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{:?}", self.config)
    }
}

impl NoiseTransport {
    fn builder<'a>(&'a self, remote_public_key: Option<&'a [u8]>) -> Builder<'a> {
        let builder = Builder::new(self.params.clone()).local_private_key(&self.local_private_key);
        match remote_public_key {
            Some(x) => builder.remote_public_key(x),
            None => builder,
        }
    }

    /// KK and other `K` patterns with several allowed client keys: the server
    /// needs the client key before it can read the first message, so try each
    /// one. Only the right key authenticates the message (its `ss` term keys
    /// the payload's AEAD tag). snowstorm continues the handshake from the
    /// state that read it.
    async fn handshake_known_client(&self, mut conn: TcpStream) -> Result<NoiseStream<TcpStream>> {
        let len = conn.read_u16_le().await? as usize;
        let mut msg = vec![0u8; len];
        conn.read_exact(&mut msg).await?;

        let mut payload = vec![0u8; len];
        let state = self
            .remote_public_keys
            .iter()
            .find_map(|k| {
                let mut state = self.builder(Some(k)).build_responder().ok()?;
                state.read_message(&msg, &mut payload).ok()?;
                Some(state)
            })
            .ok_or_else(|| anyhow!("Noise handshake from a client key that isn't allowed"))?;
        Ok(NoiseStream::handshake(conn, state).await?)
    }
}

/// Decode a base64 key and check it has the length the pattern's DH needs
fn decode_key(what: &str, encoded: &str, len: usize, pattern: &str) -> Result<Vec<u8>> {
    let key = base64::decode(encoded).with_context(|| format!("Failed to decode {}", what))?;
    if key.len() != len {
        bail!(
            "{} is {} bytes, but {} needs {}",
            what,
            key.len(),
            pattern,
            len
        );
    }
    Ok(key)
}

#[async_trait]
impl Transport for NoiseTransport {
    type Acceptor = TcpListener;
    type RawStream = TcpStream;
    type Stream = snowstorm::stream::NoiseStream<TcpStream>;

    fn new(config: &TransportConfig, role: TransportRole) -> Result<Self> {
        let tcp = TcpTransport::new(config, role)?;

        let config = match &config.noise {
            Some(v) => v.clone(),
            None => return Err(anyhow!("Missing noise config")),
        };
        let params: NoiseParams = config.pattern.parse()?;
        let dh = DefaultResolver.resolve_dh(&params.dh).ok_or_else(|| {
            anyhow!(
                "{} uses a DH function this build doesn't support",
                config.pattern
            )
        })?;
        let initiator_static = InitiatorStatic::from_pattern(&config.pattern)?;

        let mut remote_public_keys: Vec<Vec<u8>> = Vec::new();
        for (i, x) in config
            .remote_public_key
            .iter()
            .chain(config.remote_public_keys.iter().flatten())
            .enumerate()
        {
            let what = format!("remote public key #{}", i + 1);
            let key = decode_key(&what, x, dh.pub_len(), &config.pattern)?;
            if !remote_public_keys.contains(&key) {
                remote_public_keys.push(key);
            }
        }
        match role {
            TransportRole::Client if config.remote_public_keys.is_some() => {
                bail!("`remote_public_keys` is for servers; a client takes the server's key in `remote_public_key`")
            }
            TransportRole::Server
                if initiator_static == InitiatorStatic::None
                    && config.remote_public_keys.is_some() =>
            {
                bail!(
                    "Pattern {} doesn't authenticate clients, so `remote_public_keys` would have no effect",
                    config.pattern
                )
            }
            TransportRole::Server
                if initiator_static == InitiatorStatic::Known && remote_public_keys.is_empty() =>
            {
                bail!(
                    "Pattern {} needs the client public key in `remote_public_key` or `remote_public_keys`",
                    config.pattern
                )
            }
            _ => {}
        }

        let local_private_key = match &config.local_private_key {
            Some(x) => decode_key("local_private_key", x, dh.priv_len(), &config.pattern)?,
            None => Builder::new(params.clone()).generate_keypair()?.private,
        };

        Ok(NoiseTransport {
            tcp,
            config,
            params,
            local_private_key,
            remote_public_keys,
            initiator_static,
        })
    }

    fn hint(conn: &Self::Stream, opt: SocketOpts) {
        opt.apply(conn.get_inner());
    }

    async fn bind<T: ToSocketAddrs + Send + Sync>(&self, addr: T) -> Result<Self::Acceptor> {
        Ok(TcpListener::bind(addr).await?)
    }

    async fn accept(&self, a: &Self::Acceptor) -> Result<(Self::RawStream, SocketAddr)> {
        self.tcp
            .accept(a)
            .await
            .with_context(|| "Failed to accept TCP connection")
    }

    async fn handshake(&self, conn: Self::RawStream) -> Result<Self::Stream> {
        let keys = &self.remote_public_keys;
        let conn = match self.initiator_static {
            InitiatorStatic::Known if keys.len() > 1 => self.handshake_known_client(conn).await,
            // The client sends its key: check it against the allowed ones
            InitiatorStatic::Transmitted if !keys.is_empty() => {
                NoiseStream::handshake_with_verifier(
                    conn,
                    self.builder(None).build_responder()?,
                    |k| {
                        if keys.iter().any(|x| x.as_slice() == k) {
                            Ok(())
                        } else {
                            Err(snowstorm::SnowstormError::HandshakeError(
                                "client key isn't allowed".into(),
                            ))
                        }
                    },
                )
                .await
                .map_err(Into::into)
            }
            // A single known client key, or none to check
            _ => {
                let key = (self.initiator_static == InitiatorStatic::Known)
                    .then(|| keys.first().map(Vec::as_slice))
                    .flatten();
                NoiseStream::handshake(conn, self.builder(key).build_responder()?)
                    .await
                    .map_err(Into::into)
            }
        };
        conn.with_context(|| "Failed to do noise handshake")
    }

    async fn connect(&self, addr: &AddrMaybeCached) -> Result<Self::Stream> {
        let conn = self
            .tcp
            .connect(addr)
            .await
            .with_context(|| "Failed to connect TCP socket")?;

        let conn = NoiseStream::handshake(
            conn,
            self.builder(self.remote_public_keys.first().map(Vec::as_slice))
                .build_initiator()?,
        )
        .await
        .with_context(|| "Failed to do noise handshake")?;
        return Ok(conn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TransportType;
    use tokio::io::AsyncWriteExt;

    fn keypair() -> (String, String) {
        let kp = Builder::new("Noise_KK_25519_ChaChaPoly_BLAKE2s".parse().unwrap())
            .generate_keypair()
            .unwrap();
        (base64::encode(kp.private), base64::encode(kp.public))
    }

    fn transport(
        role: TransportRole,
        pattern: &str,
        private: &str,
        remote: Option<&str>,
        more: Option<Vec<String>>,
    ) -> Result<NoiseTransport> {
        NoiseTransport::new(
            &TransportConfig {
                transport_type: TransportType::Noise,
                noise: Some(NoiseConfig {
                    pattern: pattern.into(),
                    local_private_key: Some(private.into()),
                    remote_public_key: remote.map(String::from),
                    remote_public_keys: more,
                }),
                ..Default::default()
            },
            role,
        )
    }

    /// Handshake `client` against `server` over a real socket and echo a byte
    async fn connects(server: &NoiseTransport, client: &NoiseTransport) -> bool {
        let listener = server.bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let mut target = AddrMaybeCached::new(&addr);
        target.resolve().await.unwrap();
        let client_side = async {
            let mut c = client.connect(&target).await?;
            c.write_all(b"x").await?;
            c.flush().await?;
            let mut b = [0u8; 1];
            c.read_exact(&mut b).await?;
            anyhow::Ok(b)
        };
        let server_side = async {
            let (raw, _) = server.accept(&listener).await?;
            let mut s = server.handshake(raw).await?;
            let mut b = [0u8; 1];
            s.read_exact(&mut b).await?;
            s.write_all(&b).await?;
            s.flush().await?;
            anyhow::Ok(())
        };
        let (c, s) = tokio::join!(client_side, server_side);
        matches!((c, s), (Ok(b), Ok(())) if &b == b"x")
    }

    const KK: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";
    const IK: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

    #[tokio::test]
    async fn kk_accepts_each_listed_client_key_and_rejects_others() {
        let (srv_priv, srv_pub) = keypair();
        let clients: Vec<_> = (0..3).map(|_| keypair()).collect();
        let (stranger_priv, _) = keypair();
        let server = transport(
            TransportRole::Server,
            KK,
            &srv_priv,
            Some(&clients[0].1),
            Some(clients[1..].iter().map(|c| c.1.clone()).collect()),
        )
        .unwrap();
        for (private, _) in &clients {
            let client =
                transport(TransportRole::Client, KK, private, Some(&srv_pub), None).unwrap();
            assert!(connects(&server, &client).await);
        }
        let stranger = transport(
            TransportRole::Client,
            KK,
            &stranger_priv,
            Some(&srv_pub),
            None,
        )
        .unwrap();
        assert!(!connects(&server, &stranger).await);
    }

    #[tokio::test]
    async fn kk_single_key_still_works() {
        let (srv_priv, srv_pub) = keypair();
        let (cli_priv, cli_pub) = keypair();
        let server = transport(TransportRole::Server, KK, &srv_priv, Some(&cli_pub), None).unwrap();
        let client = transport(TransportRole::Client, KK, &cli_priv, Some(&srv_pub), None).unwrap();
        assert!(connects(&server, &client).await);
    }

    #[tokio::test]
    async fn ik_checks_transmitted_client_key() {
        let (srv_priv, srv_pub) = keypair();
        let (a_priv, a_pub) = keypair();
        let (b_priv, _) = keypair();
        let server = transport(
            TransportRole::Server,
            IK,
            &srv_priv,
            None,
            Some(vec![a_pub]),
        )
        .unwrap();
        let a = transport(TransportRole::Client, IK, &a_priv, Some(&srv_pub), None).unwrap();
        let b = transport(TransportRole::Client, IK, &b_priv, Some(&srv_pub), None).unwrap();
        assert!(connects(&server, &a).await);
        assert!(!connects(&server, &b).await);
    }

    #[test]
    fn rejects_misplaced_or_useless_key_lists() {
        let (k, p) = keypair();
        let nk = "Noise_NK_25519_ChaChaPoly_BLAKE2s";
        assert!(transport(
            TransportRole::Client,
            KK,
            &k,
            Some(&p),
            Some(vec![p.clone()])
        )
        .is_err());
        assert!(transport(TransportRole::Server, nk, &k, None, Some(vec![p.clone()])).is_err());
        assert!(transport(TransportRole::Server, KK, &k, None, None).is_err());
        assert!(transport(TransportRole::Server, KK, &k, Some("not base64!"), None).is_err());
        // empty or truncated keys fail at startup, not on every handshake
        assert!(transport(TransportRole::Server, KK, &k, Some(""), None).is_err());
        assert!(transport(
            TransportRole::Server,
            KK,
            &k,
            Some(&p),
            Some(vec!["AAAA".into()])
        )
        .is_err());
        assert!(transport(TransportRole::Server, KK, "", Some(&p), None).is_err());
    }
}
