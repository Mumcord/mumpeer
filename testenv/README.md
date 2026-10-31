# Local relay test environment

Everything the acceptance criteria of spec 04 need on one Linux machine, with nothing public
involved: an unmodified Mumble server, the relay bot (`mumbot peer`), mumpeer built from this
checkout, and network namespaces for two Mumcord clients. Mumcord's E2E tests reuse it.

Requirements: Docker with Compose, `openssl`, `curl`; for the namespaces `iproute2`, `nftables`,
`setpriv` (util-linux) and sudo.

## Start and stop

    testenv/up.sh            # server, relay and bot
    testenv/up.sh --no-bot   # server and relay only: no relay announcement on the server
    testenv/down.sh          # stop; data, secret and relay key are kept

`up.sh` generates `testenv/.env` on first use (SuperUser password and the relay API secret,
git-ignored, never committed), the server certificate (`testenv/certs/`), the relay config and
the bot config (`testenv/data/`), registers the bot user `RelayBot` once, and prints the
addresses and the relay fingerprint.

| Service | Address | Notes |
|---|---|---|
| Mumble server | port 44758 (TCP and UDP) | `mumblevoip/mumble-server:v1.5.735` |
| mumpeer relay | port 44759 (UDP and TCP) | built from this checkout |
| mumpeer API | `http://127.0.0.1:44760` | secret in `testenv/.env` (`MUMPEER_API_SECRET`) |
| relay bot | user `RelayBot` | `MUMBOT_IMAGE` (default `ghcr.io/mumcord/mumbot:latest`) |

Overrides: `MUMPEER_PORT_BASE` (server port; relay +1, API +2), `MUMPEER_PROJECT` (compose
project and data dir, for parallel instances), `MUMBOT_IMAGE`, `MUMPEER_PUBLIC_HOST`,
`MUMPEER_LOG` / `MUMBOT_LOG` (`RUST_LOG` of each service).

All services use the host network and listen on all interfaces of this machine; this is a
local test setup only.

Useful during tests:

    docker compose -p mumpeer-testenv logs -f mumpeer   # session lines (transports, bytes, end reason)
    docker compose -p mumpeer-testenv stop mumbot        # bot leaves (AC-4.11)
    docker compose -p mumpeer-testenv stop mumpeer       # relay down (AC-4.12)
    rm -rf testenv/data/mumpeer && testenv/up.sh         # new relay key (AC-4.13)

## Network namespaces for two clients

    sudo testenv/netns.sh up nodirect     # create A and B (profile: direct, nodirect or noudp)
    testenv/up.sh                         # (re)start so the relay announces 10.77.255.1
    sudo -E testenv/netns.sh run a mumcord
    sudo -E testenv/netns.sh run b mumcord
    sudo testenv/netns.sh profile noudp   # switch the profile at any time
    sudo testenv/netns.sh down

A (10.77.0.2) and B (10.77.0.3) share a LAN, so direct connections between them work in the
`direct` profile. Both reach this machine, and with it the test server and the relay, at
10.77.255.1 and nothing else. Connect the clients to `10.77.255.1:44758`.

| Profile | Effect | Used by |
|---|---|---|
| `direct` | no restrictions | AC-4.4, AC-4.7 |
| `nodirect` | traffic between A and B is dropped, so direct peer-to-peer fails | AC-4.5, AC-4.11, AC-4.17 |
| `noudp` | `nodirect`, plus all UDP leaving A and B is dropped (Mumble voice tunnels over TCP, the relay must use TCP) | AC-4.6 |

`run` starts the command as you with your environment (display, audio and D-Bus sockets work
inside the namespace). IPv6 is off in the namespaces, so no link-local path bypasses the
profiles. A host firewall must allow input from 10.77.255.2 to the ports above.

To capture traffic in B for AC-4.7: `sudo ip netns exec mumpeer-b tcpdump -ni eth0 host 10.77.0.2`.
