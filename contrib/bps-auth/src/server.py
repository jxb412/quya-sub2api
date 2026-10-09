#!/usr/bin/env python3
"""bps-auth: local Excel/BPS authorization service for the Codex Native Transport plugin.

The plugin lives inside the Sub2API container (Alpine, no node/python), so the
official OpenAI Excel OAuth login cannot run there. This service runs on the
host next to it and exposes two endpoints over loopback:

    GET  /health                -> runtime preflight (no secrets)
    POST /login                 -> password+TOTP Excel authorization
    POST /refresh               -> Excel refresh-token rotation

Both POST endpoints require `Authorization: Bearer $BPS_AUTH_TOKEN`.
Request/response bodies carry OAuth credentials; they are never logged.
"""
from __future__ import annotations

import json
import os
import re
import socket
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from excel_runtime import CLIENT_ID  # noqa: E402
from login_runner import LoginError, run_excel_login, runtime_health  # noqa: E402

VERSION = "0.1.0"
TOKEN_URL = "https://auth.openai.com/oauth/token?unified=true"
MAX_BODY_BYTES = 64 * 1024
_EMAIL = re.compile(r"^[^@\s]{1,128}@[^@\s]{1,128}$")
_PROXY = re.compile(r"^(socks5h?|http|https)://[^\s]{1,300}$", re.IGNORECASE)
_TOTP = re.compile(r"^[A-Za-z2-7 =]{8,128}$")


def _mask_email(value: str) -> str:
    value = (value or "").strip()
    local, _, domain = value.partition("@")
    if not domain:
        return "***"
    head = local[:2]
    return f"{head}***@{domain}"


def _log(event: str, **fields) -> None:
    payload = {"ts": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "event": event}
    payload.update(fields)
    sys.stdout.write(json.dumps(payload, ensure_ascii=False) + "\n")
    sys.stdout.flush()


class Config:
    def __init__(self) -> None:
        self.token = os.environ.get("BPS_AUTH_TOKEN", "").strip()
        listen = os.environ.get("BPS_AUTH_LISTEN", "127.0.0.1:18770").strip()
        host, _, port = listen.rpartition(":")
        self.host = host or "127.0.0.1"
        self.port = int(port or "18770")
        root = os.environ.get("BPS_AUTH_TOSUB2_ROOT", "").strip()
        self.tosub2_root = Path(root) if root else Path(__file__).resolve().parent.parent / "tosub2"
        self.python = os.environ.get("BPS_AUTH_PYTHON", "").strip()
        self.node = os.environ.get("BPS_AUTH_NODE", "").strip()
        self.timeout = max(120, int(os.environ.get("BPS_AUTH_TIMEOUT", "900")))
        self.concurrency = max(1, int(os.environ.get("BPS_AUTH_CONCURRENCY", "2")))
        self.queue = max(1, int(os.environ.get("BPS_AUTH_QUEUE", "16")))
        self.trusted_proxies_only = os.environ.get("BPS_AUTH_ALLOW_PROXY", "1") != "0"


if len(sys.argv) > 1 and sys.argv[1] == "--check":
    _cfg = Config()
    print(json.dumps(runtime_health(_cfg.tosub2_root, _cfg.python, _cfg.node), ensure_ascii=False, indent=2))
    raise SystemExit(0)

CONFIG = Config()
_SLOTS = threading.BoundedSemaphore(CONFIG.concurrency)


def _refresh(refresh_token: str, proxy_url: str = "") -> dict:
    """Rotate an Excel refresh token. Nothing here needs the Sentinel runtime."""

    form = (
        "grant_type=refresh_token"
        f"&refresh_token={urllib.parse.quote(refresh_token, safe='')}"
        f"&client_id={urllib.parse.quote(CLIENT_ID, safe='')}"
    ).encode()
    request = urllib.request.Request(
        TOKEN_URL,
        data=form,
        method="POST",
        headers={
            "content-type": "application/x-www-form-urlencoded",
            "accept": "application/json",
        },
    )
    opener = urllib.request.build_opener()
    if proxy_url:
        opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({"http": proxy_url, "https": proxy_url})
        )
    with opener.open(request, timeout=60) as response:
        return json.loads(response.read().decode("utf-8"))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = f"bps-auth/{VERSION}"

    def log_message(self, fmt: str, *args) -> None:  # keep the default noisy log quiet
        return

    def _send(self, status: int, payload: dict) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json; charset=utf-8")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _authorized(self) -> bool:
        header = self.headers.get("authorization", "")
        prefix = "Bearer "
        if not header.startswith(prefix) or not CONFIG.token:
            return False
        return _constant_eq(header[len(prefix):].strip(), CONFIG.token)

    def _read_json(self) -> dict:
        length = int(self.headers.get("content-length") or 0)
        if length <= 0 or length > MAX_BODY_BYTES:
            raise ValueError("bad_length")
        raw = self.rfile.read(length)
        value = json.loads(raw.decode("utf-8"))
        if not isinstance(value, dict):
            raise ValueError("bad_body")
        return value

    def do_GET(self) -> None:  # noqa: N802 - http.server API
        if self.path.split("?")[0] not in ("/health", "/"):
            self._send(404, {"ok": False, "error": "not_found"})
            return
        if not self._authorized():
            self._send(401, {"ok": False, "error": "unauthorized"})
            return
        health = runtime_health(CONFIG.tosub2_root, CONFIG.python, CONFIG.node)
        ready = bool(health.get("protocol_login") and health.get("python") and health.get("node"))
        self._send(
            200,
            {
                "ok": ready,
                "version": VERSION,
                "concurrency": CONFIG.concurrency,
                "timeout": CONFIG.timeout,
                "runtime": health,
            },
        )

    def do_POST(self) -> None:  # noqa: N802 - http.server API
        path = self.path.split("?")[0]
        if path not in ("/login", "/refresh"):
            self._send(404, {"ok": False, "error": "not_found"})
            return
        if not self._authorized():
            self._send(401, {"ok": False, "error": "unauthorized"})
            return
        try:
            body = self._read_json()
        except (ValueError, json.JSONDecodeError):
            self._send(400, {"ok": False, "error": "bad_request"})
            return
        if path == "/refresh":
            self._handle_refresh(body)
        else:
            self._handle_login(body)

    def _handle_refresh(self, body: dict) -> None:
        refresh_token = str(body.get("refresh_token") or "").strip()
        proxy_url = str(body.get("proxy_url") or "").strip()
        if not refresh_token:
            self._send(400, {"ok": False, "error": "missing_refresh_token"})
            return
        if proxy_url and not _PROXY.match(proxy_url):
            self._send(400, {"ok": False, "error": "bad_proxy"})
            return
        try:
            payload = _refresh(refresh_token, proxy_url)
        except urllib.error.HTTPError as exc:
            _log("refresh_failed", status=exc.code)
            self._send(200, {"ok": False, "error": f"refresh_http_{exc.code}"})
            return
        except (urllib.error.URLError, TimeoutError, OSError, ValueError):
            _log("refresh_failed", status=0)
            self._send(200, {"ok": False, "error": "refresh_transport_error"})
            return
        if not payload.get("access_token"):
            self._send(200, {"ok": False, "error": "refresh_incomplete"})
            return
        self._send(200, {"ok": True, "credentials": payload})

    def _handle_login(self, body: dict) -> None:
        email = str(body.get("email") or "").strip()
        password = str(body.get("password") or "")
        totp_secret = str(body.get("totp_secret") or "").strip()
        proxy_url = str(body.get("proxy_url") or "").strip()
        workspace = str(body.get("expected_workspace") or "").strip()
        request_id = str(body.get("request_id") or "").strip()[:64]
        if not _EMAIL.match(email):
            self._send(400, {"ok": False, "error": "bad_email"})
            return
        if not password:
            self._send(400, {"ok": False, "error": "missing_password"})
            return
        if totp_secret and not _TOTP.match(totp_secret):
            self._send(400, {"ok": False, "error": "bad_totp_secret"})
            return
        if proxy_url and not _PROXY.match(proxy_url):
            self._send(400, {"ok": False, "error": "bad_proxy"})
            return
        if not _SLOTS.acquire(timeout=CONFIG.queue):
            _log("login_busy", email=_mask_email(email), request_id=request_id)
            self._send(200, {"ok": False, "error": "service_busy"})
            return
        started = time.monotonic()
        try:
            result = run_excel_login(
                tosub2_root=CONFIG.tosub2_root,
                email=email,
                password=password,
                totp_secret=totp_secret,
                proxy_url=proxy_url,
                expected_workspace=workspace,
                python_executable=CONFIG.python,
                node_executable=CONFIG.node,
                timeout_seconds=CONFIG.timeout,
            )
        except LoginError as exc:
            _log(
                "login_failed",
                email=_mask_email(email),
                request_id=request_id,
                error=exc.code,
                stage=exc.stage,
                elapsed_ms=int((time.monotonic() - started) * 1000),
            )
            self._send(200, {"ok": False, "error": exc.code, "stage": exc.stage, "detail": exc.detail})
            return
        except Exception:  # never leak an internal traceback to the caller
            _log("login_failed", email=_mask_email(email), request_id=request_id, error="internal_error")
            self._send(200, {"ok": False, "error": "internal_error", "stage": "service"})
            return
        finally:
            _SLOTS.release()
        _log(
            "login_ok",
            email=_mask_email(email),
            request_id=request_id,
            elapsed_ms=int((time.monotonic() - started) * 1000),
        )
        self._send(200, {"ok": True, **result})


def _constant_eq(left: str, right: str) -> bool:
    if len(left) != len(right):
        return False
    diff = 0
    for a, b in zip(left, right):
        diff |= ord(a) ^ ord(b)
    return diff == 0


def main() -> int:
    if not CONFIG.token:
        sys.stderr.write("BPS_AUTH_TOKEN is required\n")
        return 2
    server = ThreadingHTTPServer((CONFIG.host, CONFIG.port), Handler)
    server.daemon_threads = True
    server.socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    _log(
        "listening",
        addr=f"{CONFIG.host}:{CONFIG.port}",
        version=VERSION,
        concurrency=CONFIG.concurrency,
        tosub2_root=str(CONFIG.tosub2_root),
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
