#!/bin/bash
# Usage: ./up.sh [--no-bot]
# Starts the Mumble test server, mumpeer (built from this checkout) and the relay bot. Generates
# the API secret into the git-ignored .env on first use; data, secret and relay key survive
# restarts. --no-bot leaves the bot stopped (server without a relay announcement).
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
cat > "$DATA_ROOT/mumpeer.toml" <<TOML
public_host = "$host"
udp_listen = "[::]:$PORT_RELAY"
tcp_listen = "[::]:$PORT_RELAY"
api_listen = "127.0.0.1:$PORT_API"
data_dir = "/var/lib/mumpeer"
TOML

docker compose up -d --build --force-recreate mumble mumpeer

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
  mumpeer relay   $host:$PORT_RELAY udp+tcp, fingerprint $fp
  mumpeer API     http://127.0.0.1:$PORT_API (secret in testenv/.env)
  relay bot       $([ "$start_bot" = 1 ] && echo "RelayBot (mumbot peer, $MUMBOT_IMAGE)" || echo "not started (--no-bot)")
INFO
