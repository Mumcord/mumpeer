# Operator guide

This guide is for people who run mumpeer for their Mumble community: install, configure, run,
watch and update it.

- [Before you start](#before-you-start)
- [Install](#install)
- [Configure](#configure)
- [Run with Docker](#run-with-docker)
- [Run as a systemd service](#run-as-a-systemd-service)
- [Connect the bot](#connect-the-bot)
- [Port 443 and reverse proxies](#port-443-and-reverse-proxies)
- [TLS certificate and fingerprint](#tls-certificate-and-fingerprint)
- [Logs](#logs)
- [Monitoring and health](#monitoring-and-health)
- [Backups](#backups)
- [Upgrading](#upgrading)

## Before you start

You need:

- **A Linux host** (x86_64 or aarch64) with a public IP address, or a router that forwards
  UDP and TCP 443 to it. A small VPS is enough: the defaults target about 50 parallel screen
  shares on 2 vCPUs. Bandwidth matters more than CPU (see
  [sizing](configuration.md#sizing)).
- **A DNS name** for it, for example `relay.example.org`. An IP address also works, but a
  name lets you move the relay later.
- **A Mumble server** with the relay bot, **mumbot `peer`**
  ([Mumcord/mumbot](https://github.com/Mumcord/mumbot)). mumpeer and the bot usually run on
  the same host, because the API stays on loopback. The Mumble server itself can run
  anywhere and needs no changes.

mumpeer never connects to the Mumble server; only the bot does.

## Install

### Docker image

```sh
docker pull ghcr.io/mumcord/mumpeer:0.1.0
```

Multi-arch (amd64, arm64), Alpine based, runs as uid 10001. It reads
`/config/mumpeer.toml` and keeps its identity in the volume `/var/lib/mumpeer`.

### Static binary

```sh
VERSION=0.1.0
ARCH=x86_64        # or aarch64
curl -fsSLO "https://github.com/Mumcord/mumpeer/releases/download/v$VERSION/mumpeer-$VERSION-linux-$ARCH.tar.gz"
curl -fsSLO "https://github.com/Mumcord/mumpeer/releases/download/v$VERSION/SHA256SUMS"
sha256sum -c SHA256SUMS --ignore-missing
mkdir mumpeer-$VERSION && tar -xzf "mumpeer-$VERSION-linux-$ARCH.tar.gz" -C mumpeer-$VERSION
sudo install -m 0755 mumpeer-$VERSION/mumpeer /usr/local/bin/mumpeer
mumpeer --version
```

The archive also holds `mumpeer.example.toml`, `README.md`, `CHANGELOG.md` and `LICENSE`. The
binary is statically linked and runs on any distribution.

### From source

```sh
git clone https://github.com/Mumcord/mumpeer.git && cd mumpeer
cargo build --release --locked      # Rust 1.90 or newer
sudo install -m 0755 target/release/mumpeer /usr/local/bin/mumpeer
```

## Configure

1. Create the secret the bot uses (at least 32 characters):

   ```sh
   sudo mkdir -p /etc/mumpeer
   openssl rand -hex 32 | sudo tee /etc/mumpeer/api_secret >/dev/null
   sudo chmod 400 /etc/mumpeer/api_secret
   ```

2. Create `/etc/mumpeer/mumpeer.toml` from
   [`mumpeer.example.toml`](../mumpeer.example.toml). The smallest working config is one line:

   ```toml
   public_host = "relay.example.org"
   ```

   Everything else has a sensible default. Where the secret comes from depends on how you run
   mumpeer; see the two sections below. All keys: [configuration reference](configuration.md).

## Run with Docker

### Docker Compose (recommended)

Run mumpeer and the bot in one Compose project. The bot shares mumpeer's network namespace
(`network_mode: "service:mumpeer"`), so it reaches the API on `127.0.0.1:7480` and the API is
never exposed. A complete, tested setup including a Mumble server is mumbot's
[peer-with-relay example](https://github.com/Mumcord/mumbot/tree/main/examples/peer-with-relay).
The relay part:

```yaml
services:
  mumpeer:
    image: ghcr.io/mumcord/mumpeer:0.1.0
    restart: unless-stopped
    environment:
      MUMPEER_API_SECRET: ${PEER_API_SECRET:?set PEER_API_SECRET in .env}
      RUST_LOG: info
    ports:
      - "443:443/udp"
      - "443:443/tcp"
    volumes:
      - ./mumpeer.toml:/config/mumpeer.toml:ro
      - mumpeer-data:/var/lib/mumpeer
    healthcheck:
      # A bare TCP connection to the API: proves the relay runs without logging in.
      test: ["CMD-SHELL", "nc -w 2 127.0.0.1 7480 </dev/null >/dev/null"]
      interval: 30s
      timeout: 5s
      retries: 3

volumes:
  mumpeer-data:
```

Put `PEER_API_SECRET=<output of openssl rand -hex 32>` into a `.env` file next to
`docker-compose.yml` (mode 600, never committed). The bot reads the same variable.

Docker lets the unprivileged container user bind port 443 inside its own network namespace, so
no extra capability is needed.

### Plain `docker run`

```sh
docker run -d --name mumpeer --restart unless-stopped \
  -p 443:443/udp -p 443:443/tcp \
  -v /etc/mumpeer/mumpeer.toml:/config/mumpeer.toml:ro \
  -v /etc/mumpeer/api_secret:/run/secrets/mumpeer_api_secret:ro \
  -v mumpeer-data:/var/lib/mumpeer \
  ghcr.io/mumcord/mumpeer:0.1.0
```

With `api_secret_file = "/run/secrets/mumpeer_api_secret"` in the config (the example's
default). The container user (uid 10001) must be able to read the secret:
`sudo chown 10001 /etc/mumpeer/api_secret`.

**Ports:** publish each port under the same number it has inside the container. The relay
announces its listening ports, so `-p 8443:443/tcp` would send clients to the wrong port. To
use another port, change `udp_listen`/`tcp_listen` and publish that port.

## Run as a systemd service

This unit runs mumpeer as a throw-away system user with only the right to bind port 443. The
secret is handed over with systemd's credentials, so the service user never sees the file in
`/etc`.

`/etc/mumpeer/mumpeer.toml`:

```toml
public_host = "relay.example.org"
api_secret_file = "/run/credentials/mumpeer.service/api_secret"
```

`/etc/systemd/system/mumpeer.service`:

```ini
[Unit]
Description=mumpeer relay for Mumcord peer-to-peer features
Documentation=https://github.com/Mumcord/mumpeer
Wants=network-online.target
After=network-online.target

[Service]
ExecStart=/usr/local/bin/mumpeer --config /etc/mumpeer/mumpeer.toml
Environment=RUST_LOG=info
DynamicUser=yes
# /var/lib/mumpeer: the relay key and certificate, kept across restarts.
StateDirectory=mumpeer
StateDirectoryMode=0700
LoadCredential=api_secret:/etc/mumpeer/api_secret
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
ProtectHome=yes
PrivateDevices=yes
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now mumpeer
journalctl -u mumpeer | grep fingerprint
```

`LoadCredential` needs systemd 247 or newer. On older systems drop it, set
`api_secret_file = "/etc/mumpeer/api_secret"`, and use a fixed user that owns the secret
instead of `DynamicUser`.

mumpeer stops cleanly on `SIGTERM` (`systemctl stop`, `docker stop`).

## Connect the bot

The bot needs three things from you:

| Bot setting | Value |
| --- | --- |
| API address (`api_url`) | `http://127.0.0.1:7480` when it runs on the same host or in the same network namespace |
| API secret | the same secret as mumpeer (`MUMBOT_PEER_API_SECRET` or a file) |
| `api_fp` | only if the API listens on a non-loopback address (HTTPS): the relay fingerprint |

Everything clients need (address, ports, fingerprint) the bot reads from
[`GET /v1/status`](api.md#get-v1status). You never copy the fingerprint by hand. Details:
[mumbot peer documentation](https://github.com/Mumcord/mumbot/blob/main/docs/peer.md).

New relay connections are refused with `bot_gone` until the bot has made its first API call,
and again whenever it has been silent for 90 seconds.

If the bot must run on another host, set `api_listen` to a private address (for example
`"10.0.0.5:7480"`). mumpeer then serves the API over HTTPS with the relay certificate and logs
a warning; the bot pins the fingerprint with `api_fp`. Allow that port only from the bot's
host in your firewall.

## Port 443 and reverse proxies

The relay uses **UDP 443** (QUIC) and **TCP 443** (TLS), because 443 gets through almost every
network. The TCP side is TLS with a mandatory client certificate and ALPN `mumpeer/1`.
A reverse proxy **cannot terminate** this TLS: the relay needs to see the client certificate
itself. Choose one of these, best first:

1. **Its own IP address.** Give mumpeer a second IPv4/IPv6 address (or its own small VPS) and
   bind it there: `udp_listen = "203.0.113.7:443"`, `tcp_listen = "203.0.113.7:443"`, and let
   `public_host` point to that address. The web server keeps the other address. Remember UDP:
   a web server with HTTP/3 also wants UDP 443.
2. **Another port.** For example `udp_listen = "[::]:8443"` and `tcp_listen = "[::]:8443"`. The
   bot announces the new port automatically. Some strict networks only allow 443, so a few
   users may not reach the relay.
3. **TLS passthrough by ALPN.** A TCP (layer 4) proxy that reads the ALPN without decrypting
   can send `mumpeer/1` connections to mumpeer and everything else to the web server.
   Example for nginx's `stream` module:

   ```nginx
   stream {
       map $ssl_preread_alpn_protocols $tls_backend {
           ~\bmumpeer/1\b  127.0.0.2:443;   # mumpeer: tcp_listen = "127.0.0.2:443"
           default         127.0.0.1:8443;  # the web server's own TLS listener
       }
       server {
           listen 443;
           ssl_preread on;
           proxy_pass $tls_backend;
       }
   }
   ```

   mumpeer must still listen on port **443** (on another local address), because it announces
   the port it listens on. **Caveat:** mumpeer then sees the proxy's address as the source of
   every TCP client, so the per-address limits (`conn_per_ip_per_min`,
   `failed_per_ip_per_min`) apply to all TCP users together. A few bad clients can block TCP
   for everyone for 10 minutes. Raise those limits if you use this, and keep UDP direct.

## TLS certificate and fingerprint

On first start mumpeer generates an ECDSA P-256 key and a self-signed certificate in
`data_dir` (`relay-key.pem` with mode 0600, `relay-cert.pem`) and logs:

```text
INFO mumpeer: relay fingerprint (SHA-256): 3f9c…a9
```

Clients do not use certificate authorities for the relay; they **pin** this fingerprint, which
the bot announces. That is why the `data_dir` must survive restarts and updates. A new key
means a new fingerprint, and every user sees "The relay of this server changed" and must
choose "Trust new relay".

Read the fingerprint later from the certificate file, or from outside:

```sh
openssl x509 -in /var/lib/mumpeer/relay-cert.pem -noout -fingerprint -sha256 \
  | cut -d= -f2 | tr -d ':' | tr 'A-F' 'a-f'

openssl s_client -connect relay.example.org:443 -alpn mumpeer/1 </dev/null 2>/dev/null \
  | openssl x509 -noout -fingerprint -sha256 | cut -d= -f2 | tr -d ':' | tr 'A-F' 'a-f'
```

The second command has no client certificate, so the relay counts it as one failed handshake
from your address; that is harmless once in a while.

**Changing the identity on purpose** (for example after the key leaked): stop mumpeer, delete
`relay-key.pem` and `relay-cert.pem`, start it again. Tell your users that a "relay changed"
message is expected.

**Own certificate:** `cert_file` and `key_file` let you bring your own. Every renewal changes
the fingerprint, so do not use certificates that renew automatically. See
[own certificate](configuration.md#own-certificate).

## Logs

mumpeer logs to standard error; Docker and journald collect it. The level comes from
`RUST_LOG` (default `info`).

At `info`:

| Line | When |
| --- | --- |
| `generated a new relay identity in …` | First start only |
| `mumpeer 0.1.0 relay for relay.example.org: UDP …, TCP …, API …; limits: …` | Start: version, address, listeners, main limits |
| `relay fingerprint (SHA-256): …` | Start |
| `totals sessions=… bytes=…` | Every 5 minutes |
| `session ended session=… transports=udp/tcp duration_s=… bytes_ab=… bytes_ba=… dropped=… reason=…` | One per finished session |

Session end reasons: `closed` (a client closed), `idle`, `revoked` (the bot dropped the pair,
for example because a user left the server), `max_duration`, `bot_gone`.

At `warn`, with only the source network (/24 for IPv4, /48 for IPv6):

- `too many new connections from 198.51.100.0/24; refusing for the rest of the minute`
- `too many failed handshakes from 198.51.100.0/24; blocked for 10 minutes`
- `API request without a valid secret from 127.0.0.0/24`
- `the API listens on non-loopback address …` (start)

`RUST_LOG=debug` adds the first 8 hex characters of certificate hashes for bind attempts and
refusals, which helps when you debug one user's problem. Switch back to `info` afterwards.

**Never logged, at any level:** full IP addresses, full certificate hashes, tokens, the API
secret, packet contents.

## Monitoring and health

- **Is it running?** Open a TCP connection to the API without sending a request:
  `nc -w 2 127.0.0.1 7480 </dev/null` (exit code 0 = up). This does not log in, so it neither
  counts as a failed login nor as bot activity. The Compose example above uses it as the
  health check.
- **What does the bot see?** Ask the API by hand:

  ```sh
  curl -fsS -H "Authorization: Bearer $(sudo cat /etc/mumpeer/api_secret)" \
    http://127.0.0.1:7480/v1/status
  # {"boot_id":"72b7cc68a6510b53","fp":"3f9c…a9","host":"relay.example.org","max_sessions":50,"sessions":0,"tcp":443,"udp":443,"v":1}
  ```

  Do not poll this from an automatic monitor: every authenticated call counts as a sign of
  life from the bot, so the relay would no longer notice when the bot is gone. A wrong secret
  counts as a failed login; ten in a minute block the address, and on loopback that is also
  the bot's address.
- **Is it reachable from outside?** Run the `openssl s_client` command from
  [the fingerprint section](#tls-certificate-and-fingerprint) from another network. For UDP,
  ask a user to share a screen through the relay; Mumcord's connection info shows
  "Via relay (UDP)" or "Via relay (TCP 443)".
- **Load:** the `totals` line every 5 minutes and the `session ended` lines (bytes, `dropped`).
  Many dropped packets mean `session_mbit` or `total_mbit` is the bottleneck.

## Backups

Only one thing is worth backing up: `relay-key.pem` and `relay-cert.pem` in `data_dir` (the
`mumpeer-data` volume in Docker). Restoring them keeps the fingerprint, so users do not get a
"relay changed" message after a move to a new host. Treat the key like a password. Everything
else (pairs, sessions, counters) lives in memory and is rebuilt by the bot.

## Upgrading

1. Read [`CHANGELOG.md`](../CHANGELOG.md). The config rejects unknown keys, so a removed or
   renamed key shows up as a start error.
2. Docker: change the image tag, then `docker compose pull mumpeer && docker compose up -d
   mumpeer`. Binary: replace `/usr/local/bin/mumpeer` and `sudo systemctl restart mumpeer`.
3. Check the start lines: same fingerprint as before.

Running relay sessions end on restart; Mumcord sets up a new one when it needs it. The bot sees
the new `boot_id` within 30 seconds and announces the relay again.
