# mumpeer documentation

| Page | For whom | What |
| --- | --- | --- |
| [Operator guide](operator-guide.md) | People who run a relay | Install, configure, Docker, systemd, port 443, certificate, logs, monitoring, upgrades |
| [Configuration reference](configuration.md) | Operators | Every config key with default and meaning, environment variables |
| [Troubleshooting](troubleshooting.md) | Operators | Start errors, unreachable relay, blocked UDP, fingerprint changes, refusal codes |
| [FAQ](faq.md) | Everyone | Short answers about privacy, ports, certificates, bandwidth |
| [Bot API reference](api.md) | Bot authors | `/v1/status`, `/v1/pairs`, `/v1/peers/{hash}`, authentication, errors |
| [Wire and handshake overview](protocol.md) | Implementers | Discovery, outer transports, control messages, error codes, session end |

The pages under [`wiki/`](wiki/) are the source of the
[GitHub wiki](https://github.com/Mumcord/mumpeer/wiki).

Related: the relay bot is documented in
[mumbot's peer documentation](https://github.com/Mumcord/mumbot/blob/main/docs/peer.md), and the
local test environment in [`testenv/README.md`](../testenv/README.md).
