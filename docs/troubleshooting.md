# Troubleshooting

Start with the relay's log (`docker compose logs mumpeer` or `journalctl -u mumpeer`) and the
reason Mumcord shows. Mumcord prints relay refusals as "The relay refused the connection:
{reason}" and shows the relay state in the server info panel.

## mumpeer does not start

| Error | Fix |
| --- | --- |
| `no API secret: set MUMPEER_API_SECRET or api_secret_file` | Provide the secret in one of the two ways. |
| `the API secret must be at least 32 characters long` | Create a longer one: `openssl rand -hex 32`. |
| `cannot read api_secret_file …` | Wrong path, or the service user cannot read it. In Docker the user is uid 10001. |
| `public_host must be a DNS name or an IP address clients can reach` | Set `public_host`; no scheme, no port, no `0.0.0.0`. |
| `unknown field …` | A typo or a key from another version. Compare with `mumpeer.example.toml`. |
| `limits.… must be greater than 0` | Every limit except `max_session_s` must be at least 1. |
| `cert_file and key_file must be set together` | Set both or neither. |
| `cannot create /var/lib/mumpeer` / `cannot write …relay-key.pem` | The data directory is not writable by the service user. In Docker, use a named volume or `chown 10001` a bind-mounted folder. |
| `Permission denied` / `Address already in use` on port 443 | Another program uses 443 (see [port 443](operator-guide.md#port-443-and-reverse-proxies)), or a non-Docker install lacks `CAP_NET_BIND_SERVICE`. |

## The server info panel says "Mumpeer relay: No"

The client never got an announcement.

- Is the bot on the server, and is it a **registered** user? Mumcord ignores announcements from
  unregistered users.
- Does the bot reach the API? Its log shows the relay as down when two status polls in a row
  fail. Check `api_url`, the secret on both sides, and that the bot shares mumpeer's network
  (`network_mode: "service:mumpeer"`).
- Is the bot's address blocked? Ten wrong secrets within a minute block the caller for 10
  minutes. Look for `API request without a valid secret` in the relay log. On loopback, a
  monitoring script with an old secret blocks the bot too.

## "The relay is not reachable right now (reported by the relay bot)"

The bot cannot reach `GET /v1/status`. mumpeer is stopped, crashed, or the API address, secret
or `api_fp` in the bot config is wrong.

## "The relay could not be reached over UDP or TCP 443"

The client could not complete the outer handshake on either transport.

- **Firewall:** open UDP **and** TCP on the relay ports at the host firewall and at the cloud
  provider's security group.
- **Ports:** the relay announces the ports it listens on. With Docker the host port must equal
  the container port (`443:443`, not `8443:443`).
- **public_host:** must resolve to the relay from the internet. A private address only works
  inside the LAN.
- **Home hosting behind NAT:** forward UDP 443 and TCP 443 on the router to the relay host.
  Behind CGNAT you cannot accept incoming connections at all; use a VPS.
- **Blocked user:** after 10 failed handshakes in a minute an address is blocked for 10
  minutes (`too many failed handshakes from …`). With IPv6 this covers the user's whole /64.

Test from another network:

```sh
openssl s_client -connect relay.example.org:443 -alpn mumpeer/1 </dev/null
```

A certificate in the output means TCP works. There is no simple command-line test for the QUIC
side; if TCP works and users still connect only "Via relay (TCP 443)", UDP 443 is filtered on
the way.

## UDP is blocked

Nothing to fix on the relay: clients fall back to TLS over TCP after 2 seconds, and the
connection info shows "Via relay (TCP 443)". TCP adds some latency under packet loss. If
**every** session is TCP, check that UDP 443 is open on the relay host and its provider.

Also check the path MTU: the relay needs 1300-byte UDP payloads. Tunnels with a small MTU
(some VPNs, PPPoE plus tunnels) make the QUIC attempt fail, and TCP takes over.

## Wrong fingerprint / "The relay of this server changed"

Mumcord pins the relay's certificate fingerprint the first time. It changes when:

- the `data_dir` (Docker volume `mumpeer-data`) was lost or recreated, so mumpeer generated a
  new key;
- you configured `cert_file`/`key_file`, or renewed that certificate;
- someone else's bot started announcing a different relay on your server.

Compare the fingerprint in the relay log (`relay fingerprint (SHA-256): …`) with what the
client shows. If the change is yours, tell users to choose "Trust new relay". If it is not, keep
it blocked and check which bot announces it. To avoid surprises, keep the volume and back up
`relay-key.pem` and `relay-cert.pem`.

If the bot itself fails to connect to an HTTPS API with a fingerprint error, its `api_fp` no
longer matches: update it to the new fingerprint.

## Refusal codes

| Mumcord shows | Cause | Fix |
| --- | --- | --- |
| `expired` | The token's 120 s ran out before both sides connected | Usually one user's network was slow or the second client never tried; just retry. If it happens often, raise `token_ttl_s`. |
| `unknown_token` | The relay does not know the token: it was replaced by a newer request, or the relay restarted in between | Retry. |
| `not_allowed` | The client's certificate is not in the pair, or this side already connected | The user changed certificate after the token was issued, or a second device of the same user tried; retry. |
| `bot_gone` | The relay has not heard from the bot for 90 s | Check that the bot runs and reaches the API. |
| `relay_full` | `max_sessions` reached | Wait, or raise `max_sessions` if bandwidth allows. |
| `peer_limit` | This user already has `max_sessions_per_peer` relayed sessions | Close another share, or raise the limit. |
| `version` | Client and relay speak different protocol versions | Update mumpeer or Mumcord. |
| `bad_request` | Malformed `bind`, wrong ALPN, or no client certificate | Usually not a Mumcord client. |

## Screen share is choppy over the relay

- `session ended … dropped=…` with high numbers: the bandwidth limits drop packets. Raise
  `session_mbit` (per direction) or `total_mbit`, if the uplink allows.
- The relay host's uplink is full: compare `totals` bytes over 5 minutes with its bandwidth.
- The session uses TCP: TCP suffers more from packet loss. See [UDP is blocked](#udp-is-blocked).

## Sessions end after a minute

- `reason=idle`: no packets for `idle_timeout_s`. Normal when a client vanished.
- `reason=bot_gone`: the bot stopped calling the API and the session was quiet for 60 s.
- `reason=revoked`: one user left the server or changed certificate, or the pair was replaced.

## Getting more detail

Set `RUST_LOG=debug` for a short time. Bind attempts and refusals then show the first 8 hex
characters of the certificate hash, which you can compare with the user's certificate hash in
Mumble or Mumcord. Switch back to `info` afterwards.
