#!/usr/bin/env bash
# Start the bps-auth sidecar next to an existing Sub2API container.
#
#   sudo ./run-container.sh --network sb_sub2api-network [--token <secret>]
#
# The container joins the network the Sub2API container already uses, so the
# plugin reaches it as http://bps-auth:18770 with no host networking, no port
# publish and no change to the Sub2API compose file.
set -euo pipefail

NAME="bps-auth"
IMAGE="bps-auth:0.1.0"
NETWORK=""
TOKEN=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --network) NETWORK="${2:-}"; shift 2 ;;
    --token) TOKEN="${2:-}"; shift 2 ;;
    --image) IMAGE="${2:-}"; shift 2 ;;
    --name) NAME="${2:-}"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$NETWORK" ]]; then
  echo "--network <docker network of the Sub2API container> is required" >&2
  exit 2
fi
docker network inspect "$NETWORK" >/dev/null 2>&1 || { echo "no such docker network: $NETWORK" >&2; exit 2; }

if [[ -z "$TOKEN" ]]; then
  TOKEN="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  echo "[bps-auth] generated token: $TOKEN"
fi

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" --restart unless-stopped \
  --network "$NETWORK" \
  -e BPS_AUTH_TOKEN="$TOKEN" \
  -e BPS_AUTH_LISTEN=0.0.0.0:18770 \
  "$IMAGE" >/dev/null

sleep 3
docker exec "$NAME" /opt/bps-auth/venv/bin/python -c \
  "import urllib.request as u;print(u.urlopen(u.Request('http://127.0.0.1:18770/health',headers={'Authorization':'Bearer $TOKEN'}),timeout=15).read().decode())"

echo "[bps-auth] token:        $TOKEN"
echo "[bps-auth] plugin config: bps_auth_service_url=http://$NAME:18770"
