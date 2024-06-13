# Security

By default, `rathole` forwards traffic as it is. Different options can be enabled to secure the traffic.

## TLS

Checkout the [example](../examples/tls)

### Client

Normally, a self-signed certificate is used. In this case, the client needs to trust the CA. `trusted_root` is the path to the root CA's certificate PEM file.
`hostname` is the hostname that the client used to validate aginst the certificate that the server presents. Note that it does not have to be the same with the `remote_addr` in `[client]`.

```toml
[client.transport.tls]
trusted_root = "example/tls/rootCA.crt"
hostname = "localhost"
```

### Server

PKCS#12 archives are needed to run the server.

It can be created using openssl like:

```sh
openssl pkcs12 -export -out identity.pfx -inkey server.key -in server.crt -certfile ca_chain_certs.crt
```

Aruguments are:

- `-inkey`: Server Private Key
- `-in`: Server Certificate
- `-certfile`: CA Certificate

Creating self-signed certificate with one's own CA is a non-trival task. However, a script is provided under tls example folder for reference.

### Rustls Support

`rathole` provides optional `rustls` support. [Build Guide](build-guide.md) demostrated this.

One difference is that, the crate we use for loading PKCS#12 archives can only handle limited types of PBE algorithms. We only support PKCS#12 archives that they (crate `p12`) support. So we need to specify the legacy format (openssl 1.x format) when creating the PKCS#12 archive.

In short, the command used with openssl 3 to create the PKCS#12 archive with `rustls` support is:

```sh
openssl pkcs12 -export -out identity.pfx -inkey server.key -in server.crt -certfile ca_chain_certs.crt -legacy
```

## Noise Protocol

### Quickstart for the Noise Protocl

In one word, the [Noise Protocol](http://noiseprotocol.org/noise.html) is a lightweigt, easy to configure and drop-in replacement of TLS. No need to create a self-sign certificate to secure the connection.

`rathole` comes with a reasonable default configuration for noise protocol. You can a glimpse of the minimal [example](../examples/noise_nk) for how it will look like.

The default noise protocol that `rathole` uses, which is `Noise_NK_25519_ChaChaPoly_BLAKE2s`, providing the authentication of the server, just like TLS with properly configured certificates. So MITM is no more a problem.

To use it, a X25519 keypair is needed.

#### Generate a Keypair

1. Run `rathole --genkey`, which will generate a keypair using the default X25519 algorithm.

It emits:

```sh
$ rathole --genkey
Private Key:
cQ/vwIqNPJZmuM/OikglzBo/+jlYGrOt9i0k5h5vn1Q=

Public Key:
GQYTKSbWLBUSZiGfdWPSgek9yoOuaiwGD/GIX8Z1kkE=
```

(WARNING: Don't use the keypair from the Internet, including this one)

2. The server should keep the private key to identify itself. And the client should keep the public key, which is used to verify whether the peer is the authentic server.

So relevant snippets of configuration are:

```toml
# Client Side Configuration
[client.transport]
type = "noise"
[client.transport.noise]
remote_public_key = "GQYTKSbWLBUSZiGfdWPSgek9yoOuaiwGD/GIX8Z1kkE="

# Server Side Configuration
[server.transport]
type = "noise"
[server.transport.noise]
local_private_key = "cQ/vwIqNPJZmuM/OikglzBo/+jlYGrOt9i0k5h5vn1Q="
```

Then `rathole` will run under the protection of the Noise Protocol.

## Specifying the Pattern of Noise Protocol

The default configuration of Noise Protocol that comes with `rathole` satifies most use cases, which is described above. But there're other patterns that can be useful.

### No Authentication

This configuration provides encryption of the traffic but provides no authentication, which means it's vulnerable to MITM attack, but is resistent to the sniffing and replay attack. If MITM attack is not one of the concerns, this is more convenient to use.

```toml
# Server Side Configuration
[server.transport.noise]
pattern = "Noise_XX_25519_ChaChaPoly_BLAKE2s"

# Client Side Configuration
[client.transport.noise]
pattern = "Noise_XX_25519_ChaChaPoly_BLAKE2s"
```

### Bidirectional Authentication

```toml
# Server Side Configuration
[server.transport.noise]
pattern = "Noise_KK_25519_ChaChaPoly_BLAKE2s"
local_private_key = "server-priv-key-here"
remote_public_key = "client-pub-key-here"

# Client Side Configuration
[client.transport.noise]
pattern = "Noise_KK_25519_ChaChaPoly_BLAKE2s"
local_private_key = "client-priv-key-here"
remote_public_key = "server-pub-key-here"
```

### Other Patterns

To find out which pattern to use, refer to:

- [7.5. Interactive handshake patterns (fundamental)](https://noiseprotocol.org/noise.html#interactive-handshake-patterns-fundamental)
- [8. Protocol names and modifiers](https://noiseprotocol.org/noise.html#protocol-names-and-modifiers)

Note that PSKs are not supported currently. Free to open an issue if you need it.
