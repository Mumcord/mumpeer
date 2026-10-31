#!/bin/bash
# Usage: ./down.sh   (stops the test environment; data, secret and relay key are kept)
set -euo pipefail
cd "$(dirname "$0")"
. ./env.sh
export PUID="$(id -u)" PGID="$(id -g)" SUPERUSER_PASSWORD=x MUMPEER_API_SECRET=x
docker compose down --remove-orphans
