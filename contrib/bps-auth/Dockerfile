# bps-auth sidecar image: runs the Excel/BPS authorization service next to the
# Sub2API container. The plugin lives inside an Alpine container with no node or
# python, so the official Excel OAuth flow (password + TOTP + Cloudflare solver)
# runs here instead.
#
# Build:  docker build -t bps-auth:0.1.0 .
# Run:    see run-container.sh
FROM node:20-bookworm

ARG TOSUB2_COMMIT=8548397e89bf80e508eda64a87e0d556d43abc84

ENV BPS_AUTH_ROOT=/opt/bps-auth \
    BPS_AUTH_TOSUB2_ROOT=/opt/bps-auth/tosub2 \
    BPS_AUTH_PYTHON=/opt/bps-auth/venv/bin/python \
    BPS_AUTH_LISTEN=0.0.0.0:18770 \
    BPS_AUTH_CONCURRENCY=2 \
    BPS_AUTH_QUEUE=16 \
    BPS_AUTH_TIMEOUT=900 \
    PYTHONUNBUFFERED=1

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      python3 python3-venv python3-pip curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY src/ /opt/bps-auth/src/
RUN find /opt/bps-auth/src -name '__pycache__' -type d -prune -exec rm -rf {} + \
 && python3 -m venv /opt/bps-auth/venv \
 && /opt/bps-auth/venv/bin/pip install --quiet --upgrade pip \
 && /opt/bps-auth/venv/bin/pip install --quiet "curl_cffi==0.15.0"

# The pinned toSub2 tarball ships no node_modules; its Cloudflare solver
# (src/cloudflare-ctf/cf_runtime.cjs) requires jsdom.
RUN mkdir -p /opt/bps-auth/tosub2 \
 && curl -fsSL "https://codeload.github.com/poxiao33/toSub2/tar.gz/${TOSUB2_COMMIT}" \
      | tar -xz --strip-components=1 -C /opt/bps-auth/tosub2 \
 && printf '%s' "${TOSUB2_COMMIT}" > /opt/bps-auth/tosub2.commit \
 && cd /opt/bps-auth/tosub2 \
 && npm install --no-save --no-audit --no-fund jsdom@26.1.0 \
 && npm cache clean --force

WORKDIR /opt/bps-auth
EXPOSE 18770

HEALTHCHECK --interval=60s --timeout=15s --start-period=20s --retries=3 \
  CMD /opt/bps-auth/venv/bin/python -c "import os,urllib.request;urllib.request.urlopen(urllib.request.Request('http://127.0.0.1:18770/health',headers={'Authorization':'Bearer '+os.environ.get('BPS_AUTH_TOKEN','')}),timeout=10).read()" || exit 1

ENTRYPOINT ["/opt/bps-auth/venv/bin/python", "/opt/bps-auth/src/server.py"]
