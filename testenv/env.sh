# Sourced by up.sh and down.sh. Derives compose variables from two optional overrides:
#   MUMPEER_PORT_BASE  Mumble server port; the relay uses base+1 (UDP and TCP), the API base+2 on
#                      loopback (default 44758, away from a real server's 64738 and from the
#                      Mumcord and Mumbot test servers)
#   MUMPEER_PROJECT    compose project name (default mumpeer-testenv); a non-default name also
#                      gets its own data dir (data/_<name>, git-ignored)
MUMPEER_PORT_BASE="${MUMPEER_PORT_BASE:-44758}"
MUMPEER_PROJECT="${MUMPEER_PROJECT:-mumpeer-testenv}"
export COMPOSE_PROJECT_NAME="$MUMPEER_PROJECT"
export PORT_MUMBLE=$((MUMPEER_PORT_BASE))
export PORT_RELAY=$((MUMPEER_PORT_BASE + 1))
export PORT_API=$((MUMPEER_PORT_BASE + 2))
export MUMBOT_IMAGE="${MUMBOT_IMAGE:-ghcr.io/mumcord/mumbot:latest}"
if [ "$MUMPEER_PROJECT" = mumpeer-testenv ]; then DATA_ROOT=data; else DATA_ROOT="data/_$MUMPEER_PROJECT"; fi
export DATA_ROOT
