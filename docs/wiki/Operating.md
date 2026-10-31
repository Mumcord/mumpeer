# Operating

The full guide with all commands: [operator guide](https://github.com/Mumcord/mumpeer/blob/main/docs/operator-guide.md).

## Run it

- **Docker Compose** with the bot in mumpeer's network namespace
  (`network_mode: "service:mumpeer"`), so the API stays on loopback. See [[Getting started]].
- **systemd** with the static binary: a ready unit with `DynamicUser`, `StateDirectory`,
  `LoadCredential` for the secret and `CAP_NET_BIND_SERVICE` for port 443 is in the
  [operator guide](https://github.com/Mumcord/mumpeer/blob/main/docs/operator-guide.md#run-as-a-systemd-service).

mumpeer stops cleanly on `SIGTERM`.

## Ports

| Port | Protocol | Open to the internet? |
| --- | --- | --- |
| 443 | UDP (QUIC) | yes |
| 443 | TCP (TLS) | yes |
| 7480 | TCP (bot API) | no, loopback only |

A web server already on 443? A reverse proxy cannot terminate the relay's TLS (it needs the
client certificate). Give the relay its own IP address, use another port, or pass connections
through by ALPN `mumpeer/1`; details and caveats in
[port 443 and reverse proxies](https://github.com/Mumcord/mumpeer/blob/main/docs/operator-guide.md#port-443-and-reverse-proxies).

## The relay identity

On first start mumpeer creates `relay-key.pem` and `relay-cert.pem` in `data_dir` and logs
`relay fingerprint (SHA-256): …`. Clients pin that fingerprint. **Keep and back up these two
files**; losing them means every user sees "The relay of this server changed".

## Logs

At `info`: start summary, fingerprint, a `totals` line every 5 minutes and one
`session ended` line per session (random id, transports, duration, bytes each way, dropped
packets, reason). Warnings show only the /24 or /48 network of an address. Never logged: full
IP addresses, full certificate hashes, tokens, the secret, packet contents.

## Health and monitoring

- Liveness: `nc -w 2 127.0.0.1 7480 </dev/null` (a bare TCP connection to the API; no login).
- Manual status: `curl -H "Authorization: Bearer $SECRET" http://127.0.0.1:7480/v1/status`.
  Do not poll it automatically: every authenticated call counts as bot activity, and wrong
  secrets get the caller's address blocked.
- Load: `totals` and `dropped` in the log.

## Upgrading

Read the [changelog](https://github.com/Mumcord/mumpeer/blob/main/CHANGELOG.md), switch the
image tag or binary, restart. Keep the data volume. Running sessions end and are set up again
when needed; the bot re-announces after it sees the new `boot_id`.

Releases: [GitHub releases](https://github.com/Mumcord/mumpeer/releases) (static Linux
binaries with `SHA256SUMS`) and `ghcr.io/mumcord/mumpeer:<version>`.
