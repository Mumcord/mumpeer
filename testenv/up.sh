#!/bin/bash
# Usage: [MUMPEER_PROFILE=<profile>] [MUMPEER_IMAGE=<image>] ./up.sh [--no-bot]
# Starts the Mumble test server, mumpeer (built from this checkout) and the relay bot. Generates
# the API secret into the git-ignored .env on first use; data, secret and relay key survive
# restarts. --no-bot leaves the bot stopped (server without a relay announcement).
# MUMPEER_PROFILE picks the relay config (see README.md); MUMPEER_IMAGE runs an existing image,
# for example a previous release, instead of building this checkout.
set -euo pipefail
cd "$(dirname "$0")"
. ./env.sh
start_bot=1
[ "${1:-}" = --no-bot ] && start_bot=0
export PUID="$(id -u)" PGID="$(id -g)"

if [ ! -f .env ]; then
  (umask 077; printf 'SUPERUSER_PASSWORD=%s\nMUMPEER_API_SECRET=%s\n' \
    "$(openssl rand -hex 16)" "$(openssl rand -hex 32)" > .env)
  echo "generated testenv/.env (superuser password and API secret)"
fi
set -a; . ./.env; set +a
./gen-certs.sh
mkdir -p "$DATA_ROOT/mumble" "$DATA_ROOT/mumpeer" "$DATA_ROOT/mumbot"

# Clients inside the netns.sh namespaces reach the host at 10.77.255.1, which is also a local
# address of the host itself, so announce it whenever the namespaces exist.
host=127.0.0.1
ip -br addr show mp-up 2>/dev/null | grep -q '10\.77\.255\.1/' && host=10.77.255.1
host="${MUMPEER_PUBLIC_HOST:-$host}"
uses=""; limits=""
case "$MUMPEER_PROFILE" in
  all) ;;
  no-files) uses="files = false" ;;
  no-screen) uses="screen_share = false" ;;
  no-images) uses="images = false" ;;
  no-broadcast) limits="max_broadcasts = 0" ;;
  old) ;; # a previous release may not know [uses] or the broadcast limits
  *) echo "unknown MUMPEER_PROFILE '$MUMPEER_PROFILE' (all, no-files, no-screen, no-images, no-broadcast, old)" >&2; exit 1 ;;
esac
cat > "$DATA_ROOT/mumpeer.toml" <<TOML
public_host = "$host"
udp_listen = "[::]:$PORT_RELAY"
tcp_listen = "[::]:$PORT_RELAY"
api_listen = "127.0.0.1:$PORT_API"
data_dir = "/var/lib/mumpeer"
TOML
[ -z "$uses" ] || printf '\n[uses]\n%s\n' "$uses" >> "$DATA_ROOT/mumpeer.toml"
[ -z "$limits" ] || printf '\n[limits]\n%s\n' "$limits" >> "$DATA_ROOT/mumpeer.toml"

if [ -n "${MUMPEER_IMAGE:-}" ]; then build=--no-build; else build=--build; fi
docker compose up -d $build --force-recreate mumble mumpeer

# An open port is not enough: right after a (re)start the server resets connections until its
# virtual server is up, so wait for a complete TLS handshake.
for _ in $(seq 60); do
  openssl s_client -connect "127.0.0.1:$PORT_MUMBLE" </dev/null >/dev/null 2>&1 && break
  sleep 1
done
openssl s_client -connect "127.0.0.1:$PORT_MUMBLE" </dev/null >/dev/null 2>&1 \
  || { echo "the test server did not accept TLS within 60s" >&2; docker compose logs --tail 20 mumble >&2; exit 1; }

status=""
for _ in $(seq 30); do
  status=$(curl -fsS -H "Authorization: Bearer $MUMPEER_API_SECRET" \
    "http://127.0.0.1:$PORT_API/v1/status" 2>/dev/null) && break
  sleep 1
done
[ -n "$status" ] || { echo "mumpeer did not answer within 30s" >&2; docker compose logs --tail 20 mumpeer >&2; exit 1; }
fp=$(printf '%s' "$status" | sed -n 's/.*"fp":"\([0-9a-f]*\)".*/\1/p')

server_fp=$(openssl x509 -in certs/cert.pem -outform der | sha256sum | cut -d' ' -f1)
cat > "$DATA_ROOT/bot.toml" <<TOML
[server]
host = "127.0.0.1"
port = $PORT_MUMBLE
name = "RelayBot"
server_cert_sha256 = "$server_fp"

[peer]
api_url = "http://127.0.0.1:$PORT_API"
TOML

# The relay bot must be a registered user, so clients accept its announcement (FR-4.8). It
# registers itself once with the test bot CLI; its certificate stays in data/mumbot.
if [ ! -f "$DATA_ROOT/mumbot/.registered" ]; then
  docker compose run --rm --no-deps --entrypoint mumbot mumbot \
    --host 127.0.0.1 --port "$PORT_MUMBLE" --name RelayBot register
  touch "$DATA_ROOT/mumbot/.registered"
fi

if [ "$start_bot" = 1 ]; then
  docker compose up -d --force-recreate mumbot
else
  docker compose stop mumbot >/dev/null 2>&1 || true
fi

cat <<INFO
test environment up
  Mumble server   $host:$PORT_MUMBLE   (certificate SHA-256 $server_fp)
  profile         $MUMPEER_PROFILE${MUMPEER_IMAGE:+ (image $MUMPEER_IMAGE)}
  mumpeer relay   $host:$PORT_RELAY udp+tcp, fingerprint $fp
  mumpeer API     http://127.0.0.1:$PORT_API (secret in testenv/.env)
  relay bot       $([ "$start_bot" = 1 ] && echo "RelayBot (mumbot peer, $MUMBOT_IMAGE)" || echo "not started (--no-bot)")
INFO
