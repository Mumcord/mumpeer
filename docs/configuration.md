# Configuration reference

mumpeer reads one TOML file. Its path comes from `--config <path>`, else from the environment
variable `MUMPEER_CONFIG`, else `/etc/mumpeer/mumpeer.toml`. The Docker image sets
`MUMPEER_CONFIG=/config/mumpeer.toml`.

Start from [`mumpeer.example.toml`](../mumpeer.example.toml): it lists every key with its
default. Only `public_host` is required.

The file is checked strictly at start. An unknown key, a wrong type or an invalid value stops
mumpeer with an error that names the key, so a typo never goes unnoticed.

## Top-level keys

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `public_host` | string | **required** | The address the bot announces to Mumcord clients: a DNS name such as `relay.example.org` or an IP address. It must be reachable by clients from the internet. A DNS name is better: you can move the relay without clients noticing. |
| `udp_listen` | socket address | `"[::]:443"` | Where the QUIC (UDP) relay listens. `[::]` listens on all IPv6 and, on Linux by default, IPv4 addresses. |
| `tcp_listen` | socket address | `"[::]:443"` | Where the TLS (TCP) relay listens, for clients whose network blocks UDP. |
| `api_listen` | socket address | `"127.0.0.1:7480"` | Where the allowlist API for the bot listens. Keep it on loopback. Any other address switches the API to HTTPS with the relay certificate and logs a warning at start. |
| `data_dir` | path | `"/var/lib/mumpeer"` | Where the generated relay key (`relay-key.pem`, mode 0600) and certificate (`relay-cert.pem`) live. Created with mode 0700 on first start. Keep it on persistent storage. |
| `api_secret_file` | path | none | File that holds the API secret. Used only when `MUMPEER_API_SECRET` is not set. |
| `cert_file` | path | none | Your own certificate chain (PEM) instead of the generated one. Must be set together with `key_file`. |
| `key_file` | path | none | The private key (PEM) for `cert_file`. |

### `public_host` rules

The same rules Mumcord clients apply to the announcement:

- a DNS name of letters, digits and hyphens, labels of 1 to 63 characters, at most 253
  characters in total, no label starting or ending with a hyphen; or
- an IPv4 or IPv6 address that is not unspecified (`0.0.0.0`, `::`), multicast or broadcast.

Private and loopback addresses are allowed, for local tests.

### Ports in the announcement

The bot announces the ports of `udp_listen` and `tcp_listen`, exactly as mumpeer binds them.
If a firewall or Docker maps a different outside port to these, clients are sent to the wrong
port. Publish the ports under the same number (`-p 443:443/udp -p 443:443/tcp`).

### Own certificate

Without `cert_file` and `key_file`, mumpeer creates an ECDSA P-256 key and a self-signed
certificate on first start and keeps them in `data_dir`. Clients do not check the certificate
against a certificate authority; they pin its **SHA-256 fingerprint**, which the bot announces.

Use `cert_file` and `key_file` only if you have a reason, such as a key kept in a central
place. The fingerprint is calculated from the first certificate in `cert_file`, so **every
renewal changes it**, and every user then sees "the relay of this server changed" and has to
trust it again. Certificates that renew automatically every few weeks are therefore a poor fit.
The relay reads the files only at start.

## The API secret

The bot authenticates with a shared secret of **at least 32 characters**. mumpeer takes it
from:

1. the environment variable `MUMPEER_API_SECRET`, if set; else
2. the file named in `api_secret_file`.

A trailing newline (as written by `echo` or `openssl rand -hex 32 > file`) is ignored. The
secret is never accepted from the config file itself and never logged. mumpeer keeps only its
SHA-256 in memory. Create one with:

```sh
openssl rand -hex 32
```

The bot needs the same value (`MUMBOT_PEER_API_SECRET` or its own `api_secret_file`, see
[mumbot's peer documentation](https://github.com/Mumcord/mumbot/blob/main/docs/peer.md)).

## `[limits]`

All limits are optional. Every value must be greater than 0, except `max_session_s`.

| Key | Default | Meaning |
| --- | --- | --- |
| `session_mbit` | `10` | Bandwidth per session and direction, in Mbit/s. Above it packets are dropped, and the end-to-end QUIC connection slows down by itself. Short bursts (100 ms worth, at least 64 KiB) pass. |
| `total_mbit` | `200` | Bandwidth of all sessions together, in Mbit/s. Same dropping rule. |
| `max_sessions` | `50` | Sessions at the same time. Above it, `POST /v1/pairs` answers `503 relay_full` and a new `bind` gets `relay_full`. |
| `max_sessions_per_peer` | `4` | Sessions at the same time per client certificate. Above it a `bind` gets `peer_limit`. |
| `token_ttl_s` | `120` | Seconds both sides have to connect with a new token. At most 65535. |
| `idle_timeout_s` | `60` | A session without packets ends after this many seconds (Mumcord's keep-alive keeps a healthy session busy). |
| `max_session_s` | `0` | Maximum length of a session in seconds; `0` means unlimited. |
| `conn_per_ip_per_min` | `20` | New relay connections per source address per minute. Further ones are refused for the rest of the minute. |
| `failed_per_ip_per_min` | `10` | Failed handshakes, refused binds or failed API logins per source address per minute. Above it the address is blocked for 10 minutes, for the relay and the API. |
| `pairs_per_min` | `120` | Pairs the bot may create per minute (`429 rate_limited` above). |

IPv6 sources are counted per /64 network, because one host can use a whole /64.

### Sizing

The defaults target about 50 parallel screen shares on a small VPS with 2 vCPUs. A rough rule:
the uplink needs `max_sessions × session_mbit × 2` (both directions), capped by `total_mbit`.
A relay with a 100 Mbit/s uplink should set `total_mbit` below 100.

## Fixed values

These are not configurable:

| What | Value |
| --- | --- |
| Bot counted as gone after | 90 s without an authenticated API call |
| Idle timeout while the bot is gone | 60 s (or less, if `idle_timeout_s` is lower) |
| Block after too many failures | 10 minutes |
| Time to send `bind` after connecting | 5 s |
| Time for the TLS or QUIC handshake | 10 s |
| Expired pairs remembered (so late clients get `expired`) | 5 minutes |
| Totals log line | every 5 minutes |
| API: request head / body | at most 8 KiB / 4 KiB, 10 s per request, 32 connections at once |

## Environment variables

| Variable | Meaning |
| --- | --- |
| `MUMPEER_CONFIG` | Path of the config file (the `--config` flag wins). |
| `MUMPEER_API_SECRET` | The API secret; wins over `api_secret_file`. |
| `RUST_LOG` | Log level: `error`, `warn`, `info` (default) or `debug`. See [logs](operator-guide.md#logs). |

## Command line

```text
mumpeer [--config <PATH>]
mumpeer --version
mumpeer --help
```

## Full example

```toml
public_host = "relay.example.org"
udp_listen = "[::]:443"
tcp_listen = "[::]:443"
api_listen = "127.0.0.1:7480"
data_dir = "/var/lib/mumpeer"
api_secret_file = "/run/secrets/mumpeer_api_secret"

[limits]
session_mbit = 10
total_mbit = 200
max_sessions = 50
max_sessions_per_peer = 4
token_ttl_s = 120
idle_timeout_s = 60
max_session_s = 0
conn_per_ip_per_min = 20
failed_per_ip_per_min = 10
pairs_per_min = 120
```
