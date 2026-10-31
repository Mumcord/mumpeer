# FAQ

## Do I need mumpeer?

No. Mumcord works fully without it. Without a relay, screen sharing and full-size images work
only when two users can connect directly, and users who hide their IP address cannot use them.
mumpeer closes that gap for your community.

## Does it change my Mumble server?

No. The Mumble server stays completely unmodified: no plugin, no config change. mumpeer never
connects to it. The relay bot joins the server as an ordinary registered user.

## Does voice go through the relay?

No. Voice and text chat always go through the Mumble server as before. The relay only carries
Mumcord's peer-to-peer features: screen sharing and full-size images today.

## Can the operator see or record screen shares?

No. The relay forwards an inner connection that is encrypted end to end between the two users
and pinned to their Mumble certificates. The relay has no key for it, and if it forwarded to
anyone else, the inner handshake would fail.

The operator **can** see the IP addresses of both users in a session, which two certificate
hashes talk to each other, and how much data flows and when. Mumcord tells users this before
they use a relay.

## Does mumpeer store anything about users?

No. Pairs, sessions and counters are in memory only and vanish on restart. The only files it
writes are its own key and certificate. Logs contain no full IP addresses, no full certificate
hashes, no tokens and no content.

## Can someone use my relay as a free proxy or VPN?

No. The relay only forwards between the two sides of a pair that your bot created, for two
users who are on your Mumble server at that moment. It never opens connections to anywhere
else.

## How is this different from a TURN server?

The idea is the same: forward packets when a direct path fails. mumpeer is smaller and stricter:
only pairs your bot created, authenticated with the users' Mumble certificates, with short-lived
tokens. It needs no user accounts or long-term credentials, and it speaks only the Mumcord relay
protocol.

## Why port 443?

Almost every network lets port 443 through, because the web uses it. UDP 443 gives the best
performance (QUIC); TCP 443 gets through networks that block UDP.

## I already run a website on port 443. What now?

Give the relay its own IP address, use another port, or pass connections through by ALPN.
See [port 443 and reverse proxies](https://github.com/Mumcord/mumpeer/blob/main/docs/operator-guide.md#port-443-and-reverse-proxies).

## Can I use a Let's Encrypt certificate?

You can, but you should not. Clients pin the certificate's fingerprint, not a certificate
authority, so each renewal looks like "the relay changed" to every user. The generated
self-signed identity is the better choice and lasts until you delete it.

## What happens when I restart or update mumpeer?

Running relayed sessions end, and Mumcord sets up a new one when needed. As long as the
`data_dir` (Docker volume) is kept, the fingerprint stays the same and users notice nothing
else.

## What if the bot goes offline?

The relay notices after 90 seconds without an API call. It then refuses new connections with
`bot_gone` and ends quiet sessions. Mumcord clients mark the relay as unavailable 15 seconds
after the bot leaves the server.

## How much bandwidth do I need?

Each session may use up to `session_mbit` (default 10 Mbit/s) in each direction; all sessions
together up to `total_mbit` (default 200). Set `total_mbit` below your uplink. A typical screen
share uses a few Mbit/s.

## Can one relay serve several Mumble servers?

Not in this version. A relay has exactly one API secret, so the supported setup is one relay
with one bot for one Mumble server. Several bots or servers with separate secrets on one relay
are out of scope for now.

## Which platforms are supported?

Linux on x86_64 and aarch64 (static binaries and a multi-arch Docker image). Mumcord clients on
every platform can use it.

## Can I test it without a public server?

Yes. mumbot's
[peer-with-relay example](https://github.com/Mumcord/mumbot/tree/main/examples/peer-with-relay)
runs everything on one machine, and this repository's [`testenv/`](https://github.com/Mumcord/mumpeer/blob/main/testenv/README.md) adds
network namespaces that simulate blocked direct connections and blocked UDP.

## Where do I report bugs?

In the [issue tracker](https://github.com/Mumcord/mumpeer/issues). Please include the mumpeer
version, how you run it, and the relevant log lines. Never post your API secret or relay key.
