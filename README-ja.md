# rathole

![rathole-logo](./docs/img/rathole-logo.png)

[![GitHub stars](https://img.shields.io/github/stars/rapiz1/rathole)](https://github.com/rapiz1/rathole/stargazers)
[![GitHub release (latest SemVer)](https://img.shields.io/github/v/release/rapiz1/rathole)](https://github.com/rapiz1/rathole/releases)
![GitHub Workflow Status (branch)](https://img.shields.io/github/actions/workflow/status/rapiz1/rathole/rust.yml?branch=main)
[![GitHub all releases](https://img.shields.io/github/downloads/rapiz1/rathole/total)](https://github.com/rapiz1/rathole/releases)
[![Docker Pulls](https://img.shields.io/docker/pulls/rapiz1/rathole)](https://hub.docker.com/r/rapiz1/rathole)
[![Join the chat at https://gitter.im/rapiz1/rathole](https://badges.gitter.im/rapiz1/rathole.svg)](https://gitter.im/rapiz1/rathole?utm_source=badge&utm_medium=badge&utm_campaign=pr-badge&utm_content=badge)

[English](README.md) | [简体中文](README-zh.md) | [日本語](README-ja.md)

Rustで書かれた、NATトラバーサルのための安全で安定した高性能リバースプロキシ

ratholeは、[frp](https://github.com/fatedier/frp)や[ngrok](https://github.com/inconshreveable/ngrok)と同様に、NAT配下のデバイス上のサービスを、パブリックIPを持つサーバー経由でインターネットに公開するのに役立ちます。

<!-- TOC -->

- [rathole](#rathole)
  - [機能](#機能)
  - [クイックスタート](#クイックスタート)
  - [設定](#設定)
    - [ログ出力](#ログ出力)
    - [チューニング](#チューニング)
  - [ベンチマーク](#ベンチマーク)
  - [計画](#計画)

<!-- /TOC -->

## 機能

- **高性能** frpよりもはるかに高いスループットを達成でき、大量の接続を処理する際により安定しています。[ベンチマーク](#ベンチマーク)を参照
- **低リソース消費** 同様のツールよりもはるかに少ないメモリを消費します。[ベンチマーク](#ベンチマーク)を参照。[バイナリは](docs/build-guide.md)ルーターのような組み込みデバイスの制約に合わせて**約500KiBまで小さくすることができます**。
- **セキュリティ** サービストークンは必須で、サービス単位で設定されます。サーバーとクライアントはそれぞれ独自の設定に責任を持ちます。オプションのNoise Protocolを使用すれば、暗号化を簡単に設定できます。自己署名証明書を作成する必要はありません！TLSもサポートされています。
- **ホットリロード** 設定ファイルをホットリロードすることで、サービスを動的に追加または削除できます。HTTP APIは開発中です。

## クイックスタート

フル機能の`rathole`は[リリース](https://github.com/rapiz1/rathole/releases)ページから入手できます。または、**他のプラットフォーム向けやバイナリを最小化するために**[ソースからビルド](docs/build-guide.md)することもできます。[Dockerイメージ](https://hub.docker.com/r/rapiz1/rathole)も利用可能です。

`rathole`の使い方はfrpと非常に似ています。後者の経験があれば、設定は非常に簡単です。唯一の違いは、サービスの設定がクライアント側とサーバー側に分割され、トークンが必須であることです。

`rathole`を使用するには、パブリックIPを持つサーバーと、NAT配下にあり、インターネットに公開する必要があるサービスを持つデバイスが必要です。

自宅のNAT配下にNASがあり、そのSSHサービスをインターネットに公開したいと仮定します：

1. パブリックIPを持つサーバー上で

以下の内容で`server.toml`を作成し、必要に応じて調整します。

```toml
# server.toml
[server]
bind_addr = "0.0.0.0:2333" # `2333`はratholeがクライアントを待ち受けるポートを指定します

[server.services.my_nas_ssh]
token = "use_a_secret_that_only_you_know" # サービスのクライアント認証に使用されるトークン。任意の値に変更してください。
bind_addr = "0.0.0.0:5202" # `5202`は`my_nas_ssh`をインターネットに公開するポートを指定します
```

その後、以下を実行します：

```bash
./rathole server.toml
```

2. NAT配下のホスト（あなたのNAS）上で

以下の内容で`client.toml`を作成し、必要に応じて調整します。

```toml
# client.toml
[client]
remote_addr = "myserver.com:2333" # サーバーのアドレス。ポートは`server.bind_addr`のポートと同じである必要があります

[client.services.my_nas_ssh]
token = "use_a_secret_that_only_you_know" # 検証をパスするためにサーバーと同じである必要があります
local_addr = "127.0.0.1:22" # 転送する必要があるサービスのアドレス
```

その後、以下を実行します：

```bash
./rathole client.toml
```

3. これで、クライアントはポート`2333`でサーバー`myserver.com`に接続を試み、`myserver.com:5202`へのトラフィックはすべてクライアントのポート`22`に転送されます。

したがって、`ssh myserver.com:5202`でNASにSSH接続できます。

Linuxで`rathole`をバックグラウンドサービスとして実行するには、[systemdの例](./examples/systemd)を確認してください。

## 設定

`rathole`は、[クイックスタート](#クイックスタート)の例のように、`[server]`と`[client]`ブロックのうち1つだけが存在する場合、設定ファイルの内容に応じてサーバーモードまたはクライアントモードで実行するかを自動的に判断できます。

ただし、`[client]`と`[server]`ブロックを1つのファイルに入れることもできます。その場合、サーバー側では`rathole --server config.toml`を実行し、クライアント側では`rathole --client config.toml`を実行して、`rathole`に実行モードを明示的に伝えます。

完全な設定仕様に進む前に、設定フォーマットの感覚をつかむために[設定例](./examples)を確認することをお勧めします。

暗号化と`transport`ブロックの詳細については、[Transport](./docs/transport.md)を参照してください。

以下が完全な設定仕様です：

```toml
[client]
remote_addr = "example.com:2333" # 必須。サーバーのアドレス
default_token = "default_token_if_not_specify" # オプション。サービスのデフォルトトークン（独自のトークンを定義していない場合）
heartbeat_timeout = 40 # オプション。0に設定するとアプリケーション層のハートビートテストを無効にします。値は`server.heartbeat_interval`より大きくなければなりません。デフォルト：40秒
retry_interval = 1 # オプション。サーバーへの接続リトライ間隔。デフォルト：1秒

[client.transport] # ブロック全体がオプション。使用するトランスポートを指定
type = "tcp" # オプション。可能な値：["tcp", "tls", "noise"]。デフォルト："tcp"

[client.transport.tcp] # オプション。`noise`と`tls`にも影響します
proxy = "socks5://user:passwd@127.0.0.1:1080" # オプション。サーバーへの接続に使用するプロキシ。`http`と`socks5`がサポートされています。
nodelay = true # オプション。適用可能な場合、TCP_NODELAYを有効にするかどうかを決定します。レイテンシは改善されますが帯域幅は減少します。デフォルト：true
keepalive_secs = 20 # オプション。適用可能な場合、`tcp(7)`の`tcp_keepalive_time`を指定します。デフォルト：20秒
keepalive_interval = 8 # オプション。適用可能な場合、`tcp(7)`の`tcp_keepalive_intvl`を指定します。デフォルト：8秒

[client.transport.tls] # `type`が"tls"の場合は必須
trusted_root = "ca.pem" # 必須。サーバーの証明書に署名したCAの証明書
hostname = "example.com" # オプション。クライアントが証明書を検証するために使用するホスト名。設定されていない場合、`client.remote_addr`にフォールバック

[client.transport.noise] # Noiseプロトコル。詳細な説明は`docs/transport.md`を参照
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # オプション。表示されているデフォルト値
local_private_key = "key_encoded_in_base64" # オプション
remote_public_key = "key_encoded_in_base64" # オプション

[client.transport.websocket] # `type`が"websocket"の場合は必須
tls = true # `true`の場合、`client.transport.tls`の設定を使用します

[client.services.service1] # 転送が必要なサービス。名前`service1`は、サーバーの設定と同一である限り任意に変更可能
type = "tcp" # オプション。転送が必要なプロトコル。可能な値：["tcp", "udp"]。デフォルト："tcp"
token = "whatever" # `client.default_token`が設定されていない場合は必須
local_addr = "127.0.0.1:1081" # 必須。転送する必要があるサービスのアドレス
nodelay = true # オプション。サービスごとに`client.transport.nodelay`を上書き
retry_interval = 1 # オプション。サーバーへの接続リトライ間隔。デフォルト：グローバル設定を継承

[client.services.service2] # 複数のサービスを定義可能
local_addr = "127.0.0.1:1082"

[server]
bind_addr = "0.0.0.0:2333" # 必須。サーバーがクライアントを待ち受けるアドレス。通常はポートのみを変更する必要があります。
default_token = "default_token_if_not_specify" # オプション
heartbeat_interval = 30 # オプション。2つのアプリケーション層ハートビート間の間隔。0に設定するとハートビート送信を無効にします。デフォルト：30秒

[server.transport] # `[client.transport]`と同じ
type = "tcp"

[server.transport.tcp] # クライアントと同じ
nodelay = true
keepalive_secs = 20
keepalive_interval = 8

[server.transport.tls] # `type`が"tls"の場合は必須
pkcs12 = "identify.pfx" # 必須。サーバーの証明書と秘密鍵のpkcs12ファイル
pkcs12_password = "password" # 必須。pkcs12ファイルのパスワード

[server.transport.noise] # `[client.transport.noise]`と同じ
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s"
local_private_key = "key_encoded_in_base64"
remote_public_key = "key_encoded_in_base64"

[server.transport.websocket] # `type`が"websocket"の場合は必須
tls = true # `true`の場合、`server.transport.tls`の設定を使用します

[server.services.service1] # サービス名はクライアント側と同一である必要があります
type = "tcp" # オプション。クライアントの`[client.services.X.type]`と同じ
token = "whatever" # `server.default_token`が設定されていない場合は必須
bind_addr = "0.0.0.0:8081" # 必須。サービスが公開されるアドレス。通常はポートのみを変更する必要があります。
nodelay = true # オプション。クライアントと同じ

[server.services.service2]
bind_addr = "0.0.0.1:8082"
```

### ログ出力

`rathole`は、他の多くのRustプログラムと同様に、環境変数を使用してログレベルを制御します。`info`、`warn`、`error`、`debug`、`trace`が利用可能です。

```shell
RUST_LOG=error ./rathole config.toml
```

は、エラーレベルのログのみで`rathole`を実行します。

`RUST_LOG`が存在しない場合、デフォルトのログレベルは`info`です。

### チューニング

v0.4.7から、ratholeはデフォルトでTCP_NODELAYを有効にしており、これによりレイテンシとRDP、Minecraftサーバーのようなインタラクティブなアプリケーションに恩恵をもたらすはずです。ただし、帯域幅はわずかに減少します。

帯域幅がより重要な場合、`nodelay = false`でTCP_NODELAYを無効にできます。

## ベンチマーク

ratholeは[frp](https://github.com/fatedier/frp)と同様のレイテンシを持ちますが、より多くの接続を処理でき、より大きな帯域幅を提供し、メモリ使用量は少なくなります。

詳細については、別ページ[ベンチマーク](./docs/benchmark.md)を参照してください。

**ただし、`rathole`が魔法のように転送サービスを以前の数倍速くできると思わないでください。** ベンチマークはローカルループバックで行われており、タスクがCPUバウンドの場合のパフォーマンスを示しています。ネットワークがボトルネックでない場合、かなりの改善が得られます。残念ながら、多くのユーザーにとってはそうではありません。その場合、主な利点は低リソース消費であり、帯域幅とレイテンシは大幅に改善されない可能性があります。

![http_throughput](./docs/img/http_throughput.svg)
![tcp_bitrate](./docs/img/tcp_bitrate.svg)
![udp_bitrate](./docs/img/udp_bitrate.svg)
![mem](./docs/img/mem-graph.png)

## 計画

- [ ] 設定用のHTTP API

[対象外の範囲](./docs/out-of-scope.md)には、実装が計画されていない機能とその理由が記載されています。