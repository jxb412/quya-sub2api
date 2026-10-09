"""Run one Excel/BPS password+TOTP authorization through the pinned toSub2 runtime.

Flow (all HTTP is performed by the pinned toSub2 `protocol-login.mjs`, adapted to
the official Excel/BPS OAuth client by `excel_runtime.prepare_excel_runtime`):

    authorize -> authorize/continue -> password/verify -> mfa issue/verify
    -> workspace/select -> callback -> POST /oauth/token?unified=true

Secrets (password, TOTP seed, tokens) only travel through environment variables
and temp files; this module never logs them.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from excel_runtime import CLIENT_ID, prepare_excel_runtime, validate_excel_credentials

# toSub2 prints these markers; they are the only stage information we keep.
_STAGE_MARKERS = (
    ("Password accepted", "password_accepted"),
    ("TOTP 2FA challenge reached", "mfa_challenge"),
    ("2FA verification accepted", "mfa_accepted"),
    ("Start official Excel PKCE", "excel_authorize"),
    ("Password/TOTP authorization completed", "excel_authorized"),
)
_REASON_MARKERS = (
    ("EXCEL_ADDITIONAL_VERIFICATION_REQUIRED", "additional_verification"),
    ("EXCEL_CALLBACK_STATE_MISMATCH", "callback_state_mismatch"),
    ("EXCEL_EXPECTED_WORKSPACE_NOT_LISTED", "workspace_not_listed"),
    ("EXCEL_PASSWORD_PAGE_REQUIRED", "password_page_required"),
    ("EXCEL_UNEXPECTED_INITIAL_PAGE", "unexpected_initial_page"),
    ("EXCEL_AUTH_ORIGIN_INVALID", "auth_origin_invalid"),
    ("Password was rejected", "password_rejected"),
    ("2FA code was rejected", "totp_rejected"),
    ("security-check", "security_check"),
    ("PROXY_RISK_CONTROL", "security_check"),
    ("dynamic_risk", "security_check"),
    ("Sentinel", "sentinel_error"),
    ("sentinel", "sentinel_error"),
)


class LoginError(Exception):
    """A login failure carrying only a non-sensitive code and stage."""

    def __init__(self, code: str, stage: str = "unknown", detail: str = ""):
        self.code = code
        self.stage = stage
        self.detail = detail
        super().__init__(f"{code} (stage={stage})")


def _checks(executable: str, args: list) -> bool:
    try:
        done = subprocess.run(
            [executable, *args],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=60,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return done.returncode == 0


def find_python(configured: str = "") -> str:
    """Return a python interpreter that can `import curl_cffi`."""

    # A dedicated venv is the normal deployment; sys.executable makes the service
    # work without BPS_AUTH_PYTHON while system python3 has no curl_cffi.
    candidates = [configured] if configured else [sys.executable, "python3", "python"]
    for candidate in candidates:
        candidate = (candidate or "").strip()
        if candidate and _checks(candidate, ["-c", "import curl_cffi"]):
            return candidate
    raise LoginError("python_curl_cffi_missing", "preflight")


def find_node(configured: str = "") -> str:
    candidates = [configured] if configured else ["node"]
    for candidate in candidates:
        candidate = (candidate or "").strip()
        if candidate and _checks(candidate, ["--version"]):
            return candidate
    raise LoginError("node_missing", "preflight")


def describe_failure(output: str, exit_code: int) -> LoginError:
    stage = "web_login"
    for marker, label in _STAGE_MARKERS:
        if marker in output:
            stage = label
    reasons = sorted({label for marker, label in _REASON_MARKERS if marker in output})
    code = ",".join(reasons) if reasons else "protocol_error"
    return LoginError(code, stage, f"exit={exit_code}")


def run_excel_login(
    *,
    tosub2_root: Path,
    email: str,
    password: str,
    totp_secret: str = "",
    proxy_url: str = "",
    expected_workspace: str = "",
    python_executable: str = "",
    node_executable: str = "",
    timeout_seconds: int = 900,
) -> dict:
    """Perform one Excel/BPS authorization and return the OAuth credentials."""

    email = (email or "").strip()
    if not email or not password:
        raise LoginError("missing_credentials", "preflight")
    root = Path(tosub2_root)
    if not (root / "src" / "protocol-login.mjs").is_file():
        raise LoginError("tosub2_runtime_missing", "preflight")

    python_bin = find_python(python_executable)
    node_bin = find_node(node_executable)

    with tempfile.TemporaryDirectory(prefix="bps-auth-") as temp_dir:
        temp = Path(temp_dir)
        try:
            script = prepare_excel_runtime(root, temp)
        except ValueError as exc:
            raise LoginError(str(exc), "preflight") from None
        output_path = temp / "oauth.json"
        command = [
            node_bin,
            str(script),
            "--email",
            email,
            "--output-mode",
            "sub2api",
            "--sub2api-out",
            str(output_path),
            "--sub2api-name",
            "bps-excel",
        ]
        if proxy_url:
            command.extend(("--proxy", proxy_url))
        child_env = os.environ.copy()
        # One bounded fixed-exit attempt: never rotate proxies and retry a rejected login.
        child_env["CHATGPT_PROXY_MAX_ATTEMPTS"] = "1"
        child_env["CHATGPT_LOGIN_PASSWORD"] = password
        child_env["TOSUB2_PYTHON"] = python_bin
        if totp_secret:
            child_env["CHATGPT_TOTP_SECRET"] = totp_secret
        else:
            child_env.pop("CHATGPT_TOTP_SECRET", None)
        if expected_workspace:
            child_env["OPENAI_EXCEL_EXPECTED_WORKSPACE"] = expected_workspace
        else:
            child_env.pop("OPENAI_EXCEL_EXPECTED_WORKSPACE", None)
        try:
            result = subprocess.run(
                command,
                cwd=temp_dir,
                env=child_env,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=timeout_seconds,
                check=False,
            )
        except subprocess.TimeoutExpired:
            raise LoginError("login_timeout", "web_login") from None
        if result.returncode != 0:
            raise describe_failure(result.stdout + "\n" + result.stderr, result.returncode)
        try:
            payload = json.loads(output_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, ValueError):
            raise LoginError("no_oauth_output", "web_login") from None

    accounts = payload.get("accounts") if isinstance(payload, dict) else None
    account = accounts[0] if isinstance(accounts, list) and len(accounts) == 1 else None
    credentials = account.get("credentials") if isinstance(account, dict) else None
    extra = account.get("extra") if isinstance(account, dict) else None
    if not isinstance(credentials, dict) or not all(
        str(credentials.get(key) or "").strip()
        for key in ("access_token", "refresh_token", "id_token")
    ):
        raise LoginError("incomplete_credentials", "token_exchange")
    credentials = dict(credentials)
    credentials.setdefault("client_id", CLIENT_ID)
    try:
        validate_excel_credentials(credentials, email, expected_workspace or None)
    except ValueError as exc:
        raise LoginError(str(exc), "token_exchange") from None
    return {
        "credentials": credentials,
        "extra": extra if isinstance(extra, dict) else {},
        "account_name": account.get("name") if isinstance(account, dict) else None,
    }


def runtime_health(tosub2_root: Path, python_executable: str = "", node_executable: str = "") -> dict:
    """Cheap preflight for /health: runtimes present, no network calls."""

    root = Path(tosub2_root)
    info = {
        "tosub2_root": str(root),
        "protocol_login": (root / "src" / "protocol-login.mjs").is_file(),
        "sentinel_runtime": (root / "src" / "cloudflare-ctf" / "sentinel_runtime.cjs").is_file(),
        "cloudflare_solver": (root / "src" / "cloudflare-ctf" / "cf_runtime.cjs").is_file()
        and (root / "node_modules" / "jsdom").is_dir(),
        "python": "",
        "curl_cffi": False,
        "node": "",
    }
    try:
        info["python"] = find_python(python_executable)
        info["curl_cffi"] = True
    except LoginError:
        pass
    try:
        info["node"] = find_node(node_executable)
    except LoginError:
        pass
    return info
