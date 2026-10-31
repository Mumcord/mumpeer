# Bot API reference

The allowlist API is how the relay bot (`mumbot peer`) talks to mumpeer. It is small on
purpose: the bot reads the relay's status, creates pairs and removes users. Nothing else can
put a certificate on the allowlist.

Normal operators never call it by hand except for a quick status check. If you write your own
bot, this page is the contract.

## Transport

- HTTP/1.1, one request per connection (`Connection: close`), JSON bodies.
- Listens on `api_listen`, default `127.0.0.1:7480`, plain HTTP.
- On any non-loopback address the API uses **HTTPS** with the relay's own certificate (the same
  one clients pin). Callers must pin its SHA-256 fingerprint instead of checking it against a
  certificate authority; mumbot does that with `api_fp`.
- Limits: request head at most 8 KiB (`431` above), body at most 4 KiB (`413` above), 10 seconds
  per request, at most 32 connections at once.

## Authentication

Every request needs the shared secret:

```http
Authorization: Bearer <secret>
```

The secret is compared in constant time. A missing or wrong secret returns
`401 {"code":"unauthorized"}`, is logged as a warning with the caller's /24 (or /48) network,
and counts as a failed login of the caller's address. After `failed_per_ip_per_min` failures
(default 10) in one minute, the address is blocked for 10 minutes: its API connections are
closed without an answer.

Every authenticated request also tells the relay that the bot is alive. When no authenticated
request has arrived for 90 seconds, the relay treats the bot as gone (see
[`bot_gone`](protocol.md#error-codes)). mumbot polls `GET /v1/status` every 30 seconds for this
reason.

## Errors

Errors carry a JSON body with a machine-readable code:

```json
{"code":"bad_request"}
```

| Status | `code` | Meaning |
| --- | --- | --- |
| 400 | `bad_request` | Malformed request, JSON or certificate hash |
| 401 | `unauthorized` | Missing or wrong secret |
| 404 | `not_found` | Unknown path |
| 405 | `method_not_allowed` | Known path, wrong method |
| 413 | `too_large` | Body over 4 KiB |
| 429 | `rate_limited` | More than `pairs_per_min` pairs this minute |
| 431 | `too_large` | Request head over 8 KiB |
| 503 | `relay_full` | `max_sessions` reached |

## `GET /v1/status`

What the bot announces to clients, plus the current load.

```sh
curl -fsS -H "Authorization: Bearer $SECRET" http://127.0.0.1:7480/v1/status
```

`200 OK`:

```json
{
  "v": 1,
  "boot_id": "72b7cc68a6510b53",
  "host": "relay.example.org",
  "udp": 443,
  "tcp": 443,
  "fp": "0b355b90b32dae6e5fe93b59215ab74449e26a695328b27b861cba4f90c16ad5",
  "sessions": 0,
  "max_sessions": 50
}
```

| Field | Meaning |
| --- | --- |
| `v` | API version, `1` |
| `boot_id` | 16 hex characters, new on every start. A change tells the bot the relay restarted, so it announces again. |
| `host` | `public_host` from the config |
| `udp`, `tcp` | The ports the relay listens on (`udp_listen`, `tcp_listen`) |
| `fp` | SHA-256 of the relay certificate (DER), 64 lowercase hex characters: what clients pin |
| `sessions` | Pairs with at least one connected side |
| `max_sessions` | The configured limit |

## `POST /v1/pairs`

Creates a pair for two Mumble certificate hashes and returns its token. The bot sends the
token to both users; each uses it once to connect.

```sh
curl -fsS -H "Authorization: Bearer $SECRET" -H 'Content-Type: application/json' \
  -d '{"a":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","b":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}' \
  http://127.0.0.1:7480/v1/pairs
```

Body:

| Field | Meaning |
| --- | --- |
| `a`, `b` | Mumble certificate hashes of the two users: SHA-1 of the certificate DER, 40 hex characters (any case; stored lowercase). Must differ. |

`201 Created`:

```json
{"tok":"Xq3…43 characters…","expires_in":120}
```

| Field | Meaning |
| --- | --- |
| `tok` | 32 random bytes, base64url without padding (43 characters). The relay keeps only its SHA-256. |
| `expires_in` | Seconds both sides have to connect (`token_ttl_s`) |

Rules:

- A new pair for the same two hashes **replaces** the old one: the old token stops working and
  a running session of the old pair ends.
- Once both sides are connected, the pair is a session and no longer expires; it ends as
  described in [session end](protocol.md#session-end).
- Errors: `400 bad_request` (malformed or equal hashes), `429 rate_limited`,
  `503 relay_full`.

## `DELETE /v1/peers/{hash}`

Removes a certificate from the allowlist: all its pairs are revoked and their sessions end
(log reason `revoked`). The bot calls it when a user leaves the Mumble server or changes
certificate.

```sh
curl -fsS -X DELETE -H "Authorization: Bearer $SECRET" \
  http://127.0.0.1:7480/v1/peers/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
```

`204 No Content`, also when the hash was in no pair. `400 bad_request` if `{hash}` is not 40
hex characters.
