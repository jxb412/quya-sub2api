#!/usr/bin/env python3
"""把已构建的插件二进制 + UI 打包成 Sub2API 可安装的 .s2plugin。

用法：
  python3 tools/package.py \
      --runtime darwin-arm64=target/release/plugin \
      --runtime linux-amd64=dist/linux-amd64/plugin \
      [--sign-key publisher_id=ed25519_private_key.pem]

未签名包需要宿主配置 plugins.allow_unsigned: true（仅限本地调试）。
签名使用 Ed25519 对 manifest.json 原始字节签名（需要 python3.13+ 或 cryptography 库时
改用 --sign-key；默认产出未签名开发包）。
"""

import argparse
import base64
import hashlib
import json
import sys
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ed25519_tool  # noqa: E402

PLUGIN_ID = "io.sub2api.codex-native-transport"
PLUGIN_NAME = "Codex Native Transport"
DESCRIPTION = (
    "OpenAI OAuth outbound transport with the exact network stack of the official "
    "Codex CLI (reqwest + native-tls + hyper/h2, versions pinned to codex-rs). "
    "TLS and HTTP/2 fingerprints match the real client by construction."
)
AUTHOR = "sub2api-community"
CAPABILITY = {
    "id": "openai.oauth.outbound_transport.v1",
    "platform": "openai",
    "account_type": "oauth",
}
REQUIRES = {
    "sub2api": ">=0.1.0",
    "recommended_sub2api_version": "0.2.3",
    "tested_sub2api_versions": ["0.2.3", "0.0.0-dev"],
    "plugin_protocol": 1,
    "transport_api": 1,
    "ui_bridge": 1,
}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_cargo_version(root: Path) -> str:
    for line in (root / "Cargo.toml").read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line.startswith("version"):
            return line.split("=", 1)[1].strip().strip('"')
    raise SystemExit("cannot find version in Cargo.toml")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--runtime",
        action="append",
        required=True,
        metavar="OS-ARCH=PATH",
        help="e.g. darwin-arm64=target/release/plugin (repeatable)",
    )
    parser.add_argument("--output-dir", default="dist")
    parser.add_argument(
        "--sign-key",
        metavar="KEY_FILE",
        help="Ed25519 私钥文件(tools/ed25519_tool.py keygen 生成)。提供后产出签名包。",
    )
    parser.add_argument(
        "--key-id",
        default="codex-native-transport-publisher-v1",
        help="signature.json 的 key_id,需与宿主 plugins.trusted_publishers 的键一致",
    )
    args = parser.parse_args()

    root = Path(__file__).resolve().parent.parent
    version = read_cargo_version(root)

    runtimes: dict[str, Path] = {}
    for item in args.runtime:
        key, _, raw_path = item.partition("=")
        key = key.strip()
        binary = (root / raw_path).resolve() if not Path(raw_path).is_absolute() else Path(raw_path)
        if not key or not binary.is_file():
            raise SystemExit(f"invalid --runtime {item!r} (binary missing?)")
        runtimes[key] = binary

    ui_dir = root / "ui"
    ui_files = sorted(p for p in ui_dir.rglob("*") if p.is_file())
    if not (ui_dir / "index.html").is_file():
        raise SystemExit("ui/index.html is required")

    # 组装包内文件表：包内路径 -> 磁盘路径
    entries: dict[str, Path] = {}
    runtime_manifest: dict[str, dict[str, str]] = {}
    for key, binary in runtimes.items():
        suffix = ".exe" if key.startswith("windows-") else ""
        inner = f"runtimes/{key}/plugin{suffix}"
        entries[inner] = binary
        runtime_manifest[key] = {"path": inner}
    for path in ui_files:
        entries[f"ui/{path.relative_to(ui_dir).as_posix()}"] = path

    manifest = {
        "schema_version": 1,
        "id": PLUGIN_ID,
        "name": PLUGIN_NAME,
        "version": version,
        "description": DESCRIPTION,
        "author": AUTHOR,
        "requires": REQUIRES,
        "capabilities": [CAPABILITY],
        "runtimes": runtime_manifest,
        "ui": {"entrypoint": "ui/index.html"},
        "files": {inner: sha256_file(path) for inner, path in sorted(entries.items())},
    }
    manifest_bytes = json.dumps(manifest, ensure_ascii=False, indent=2).encode()

    signature_bytes = None
    public_key_b64 = None
    if args.sign_key:
        seed = ed25519_tool.load_seed(args.sign_key)
        public_key_b64 = base64.b64encode(ed25519_tool.secret_to_public(seed)).decode()
        signature = {
            "algorithm": "ed25519",
            "key_id": args.key_id,
            "signature": base64.b64encode(ed25519_tool.sign(seed, manifest_bytes)).decode(),
        }
        signature_bytes = json.dumps(signature, indent=2).encode()

    out_dir = root / args.output_dir
    out_dir.mkdir(parents=True, exist_ok=True)
    package_path = out_dir / f"codex-native-transport-{version}.s2plugin"
    with zipfile.ZipFile(package_path, "w", zipfile.ZIP_DEFLATED) as bundle:
        bundle.writestr("manifest.json", manifest_bytes)
        if signature_bytes is not None:
            bundle.writestr("signature.json", signature_bytes)
        for inner, path in sorted(entries.items()):
            bundle.write(path, inner)

    print(f"wrote {package_path}")
    print(f"  id={PLUGIN_ID} version={version}")
    for key in sorted(runtime_manifest):
        print(f"  runtime {key}: {runtimes[key]}")
    if signature_bytes is not None:
        print(f"  signature: ed25519 key_id={args.key_id}")
        print("  在宿主配置登记发布者公钥:")
        print("    plugins:")
        print("      trusted_publishers:")
        print(f'        {args.key_id}: "{public_key_b64}"')
    else:
        print("  signature: none (requires plugins.allow_unsigned: true)")


if __name__ == "__main__":
    sys.exit(main())
