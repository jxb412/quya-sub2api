#!/usr/bin/env bash
# Install bps-auth on a Sub2API host (Ubuntu/Debian) as a systemd service.
#
#   sudo ./install.sh --token <shared-secret> [--port 18770] [--listen 127.0.0.1]
#
# The service needs: node >= 20, python3 (venv), curl_cffi. Everything lives in
# /opt/bps-auth; no change is made to the Sub2API container.
set -euo pipefail

TOSUB2_COMMIT="8548397e89bf80e508eda64a87e0d556d43abc84"
ROOT_DIR="/opt/bps-auth"
TOKEN=""
PORT="18770"
LISTEN="127.0.0.1"
CONCURRENCY="2"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --token) TOKEN="${2:-}"; shift 2 ;;
    --port) PORT="${2:-}"; shift 2 ;;
    --listen) LISTEN="${2:-}"; shift 2 ;;
    --concurrency) CONCURRENCY="${2:-}"; shift 2 ;;
    --dir) ROOT_DIR="${2:-}"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$TOKEN" ]]; then
  TOKEN="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  echo "[bps-auth] generated token: $TOKEN"
fi

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq python3 python3-venv python3-pip curl ca-certificates

if ! command -v node >/dev/null 2>&1; then
  curl -fsSL https://deb.nodesource.com/setup_20.x | bash -
  apt-get install -y -qq nodejs
fi

install -d -m 0755 "$ROOT_DIR"
install -d -m 0755 "$ROOT_DIR/src"
install -m 0644 "$(dirname "$0")/src/excel_runtime.py" "$ROOT_DIR/src/excel_runtime.py"
install -m 0644 "$(dirname "$0")/src/openai_excel_password_flow.mjs" "$ROOT_DIR/src/openai_excel_password_flow.mjs"
install -m 0644 "$(dirname "$0")/src/login_runner.py" "$ROOT_DIR/src/login_runner.py"
install -m 0644 "$(dirname "$0")/src/server.py" "$ROOT_DIR/src/server.py"

if [[ ! -x "$ROOT_DIR/venv/bin/python" ]]; then
  python3 -m venv "$ROOT_DIR/venv"
fi
"$ROOT_DIR/venv/bin/pip" install --quiet --upgrade pip
"$ROOT_DIR/venv/bin/pip" install --quiet "curl_cffi==0.15.0"

if [[ ! -f "$ROOT_DIR/tosub2/src/protocol-login.mjs" ]]; then
  rm -rf "$ROOT_DIR/tosub2"
  mkdir -p "$ROOT_DIR/tosub2"
  curl -fsSL "https://codeload.github.com/poxiao33/toSub2/tar.gz/$TOSUB2_COMMIT" \
    | tar -xz --strip-components=1 -C "$ROOT_DIR/tosub2"
fi
echo "$TOSUB2_COMMIT" > "$ROOT_DIR/tosub2.commit"

# The pinned toSub2 tarball ships no node_modules. Its Cloudflare solver
# (src/cloudflare-ctf/cf_runtime.cjs) requires jsdom; without it the solver dies at
# require() and every login stalls until "did not issue cf_clearance before timeout".
# Only jsdom is needed, so it is installed on its own instead of the full dev tree.
if [[ ! -d "$ROOT_DIR/tosub2/node_modules/jsdom" ]]; then
  jsdom_stage="$(mktemp -d)"
  printf '{"name":"bps-auth-jsdom","private":true}\n' > "$jsdom_stage/package.json"
  ( cd "$jsdom_stage" && npm install --no-save --no-audit --no-fund jsdom@26.1.0 )
  cp -a "$jsdom_stage/node_modules" "$ROOT_DIR/tosub2/node_modules"
  rm -rf "$jsdom_stage"
fi

cat > /etc/bps-auth.env <<EOF
BPS_AUTH_TOKEN=$TOKEN
BPS_AUTH_LISTEN=$LISTEN:$PORT
BPS_AUTH_TOSUB2_ROOT=$ROOT_DIR/tosub2
BPS_AUTH_PYTHON=$ROOT_DIR/venv/bin/python
BPS_AUTH_CONCURRENCY=$CONCURRENCY
BPS_AUTH_TIMEOUT=900
EOF
chmod 0600 /etc/bps-auth.env

cat > /etc/systemd/system/bps-auth.service <<EOF
[Unit]
Description=BPS Excel authorization service (Sub2API codex-native-transport)
After=network-online.target

[Service]
Type=simple
EnvironmentFile=/etc/bps-auth.env
ExecStart=$ROOT_DIR/venv/bin/python $ROOT_DIR/src/server.py
Restart=always
RestartSec=5
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now bps-auth.service
sleep 2
systemctl --no-pager --lines=5 status bps-auth.service || true
echo "[bps-auth] token: $TOKEN"
echo "[bps-auth] health: curl -s -H 'Authorization: Bearer $TOKEN' http://$LISTEN:$PORT/health"
