# Troubleshooting

The full table of start errors, symptoms and fixes: [troubleshooting](https://github.com/Mumcord/mumpeer/blob/main/docs/troubleshooting.md).

## Quick checks

1. **Relay running?** `docker compose logs mumpeer` or `journalctl -u mumpeer`: look for the
   fingerprint line and errors.
2. **Bot connected?** The bot log reports the relay as up. A wrong secret shows up in the relay
   log as `API request without a valid secret`.
3. **Reachable from outside?** From another network:
   `openssl s_client -connect relay.example.org:443 -alpn mumpeer/1 </dev/null` prints the
   relay certificate when TCP 443 works.

## Common problems

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| "Mumpeer relay: No" in Mumcord | Bot not on the server, not registered, or cannot reach the API | Register the bot, check `api_url` and the secret, share the network namespace |
| "The relay is not reachable right now" | Bot's status polls fail | Start mumpeer; check secret and `api_fp` |
| "The relay could not be reached over UDP or TCP 443" | Firewall, wrong port mapping, wrong `public_host`, NAT | Open UDP and TCP 443, publish `443:443`, forward ports on the router |
| Always "Via relay (TCP 443)" | UDP 443 blocked or MTU below 1300 on the path | Open UDP 443; check tunnels and VPNs |
| "The relay of this server changed" | New key: data volume lost, own certificate renewed | Restore the key files, or tell users to trust the new relay |
| `expired` | Second user connected after the token's 120 s | Retry; raise `token_ttl_s` if frequent |
| `unknown_token` | Token replaced by a newer request, or relay restarted | Retry |
| `not_allowed` | Certificate not in the pair, or side already bound | User changed certificate or used a second device; retry |
| `bot_gone` | No API call from the bot for 90 s | Restart or fix the bot |
| `relay_full` / `peer_limit` | `max_sessions` / `max_sessions_per_peer` reached | Wait or raise the limit |
| Choppy screen share | Bandwidth limits drop packets (`dropped=` in the log) | Raise `session_mbit`/`total_mbit` if the uplink allows |
| One user cannot connect for 10 minutes | Too many failed handshakes from their network | Wait; check the `blocked for 10 minutes` warning |

For more detail, run with `RUST_LOG=debug` for a short time: bind attempts then show the first
8 characters of the certificate hash.

Still stuck? [Open an issue](https://github.com/Mumcord/mumpeer/issues) with the version, how you
run it and the relevant log lines; never include the secret or the relay key.
