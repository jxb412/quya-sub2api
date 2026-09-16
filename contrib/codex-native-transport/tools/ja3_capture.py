#!/usr/bin/env python3
"""本地 TLS ClientHello 捕获器：解析 JA3 及关键扩展，打印后断开。

用法: python3 tools/ja3_capture.py [port]
对比不同客户端的 TLS 指纹时，让客户端向 https://127.0.0.1:<port> 发起请求即可
（握手不会完成，但 ClientHello 已经送达）。
"""

import hashlib
import socket
import struct
import sys


def is_grease(value: int) -> bool:
    return (value & 0x0F0F) == 0x0A0A and ((value >> 8) & 0xFF) == (value & 0xFF)


def read_exact(conn: socket.socket, count: int) -> bytes:
    data = b""
    while len(data) < count:
        chunk = conn.recv(count - len(data))
        if not chunk:
            raise EOFError("connection closed")
        data += chunk
    return data


def parse_client_hello(conn: socket.socket) -> dict:
    header = read_exact(conn, 5)
    content_type, _major, _minor, length = struct.unpack("!BBBH", header)
    if content_type != 0x16:
        raise ValueError(f"not a TLS handshake record: {content_type}")
    record = read_exact(conn, length)
    if record[0] != 0x01:
        raise ValueError("not a ClientHello")
    body = record[4:]
    offset = 0
    legacy_version = struct.unpack("!H", body[offset : offset + 2])[0]
    offset += 2 + 32  # version + random
    session_len = body[offset]
    offset += 1 + session_len
    (cipher_len,) = struct.unpack("!H", body[offset : offset + 2])
    offset += 2
    ciphers = [
        struct.unpack("!H", body[i : i + 2])[0]
        for i in range(offset, offset + cipher_len, 2)
    ]
    offset += cipher_len
    compression_len = body[offset]
    offset += 1 + compression_len
    (ext_total,) = struct.unpack("!H", body[offset : offset + 2])
    offset += 2

    extensions = []
    curves: list[int] = []
    point_formats: list[int] = []
    alpn: list[str] = []
    sig_algs: list[int] = []
    supported_versions: list[int] = []
    key_share_groups: list[int] = []

    end = offset + ext_total
    while offset + 4 <= end:
        ext_type, ext_len = struct.unpack("!HH", body[offset : offset + 4])
        offset += 4
        data = body[offset : offset + ext_len]
        offset += ext_len
        extensions.append(ext_type)
        if ext_type == 0x0A and len(data) >= 2:  # supported_groups
            (list_len,) = struct.unpack("!H", data[:2])
            curves = [
                struct.unpack("!H", data[i : i + 2])[0]
                for i in range(2, 2 + list_len, 2)
            ]
        elif ext_type == 0x0B and len(data) >= 1:  # ec_point_formats
            point_formats = list(data[1 : 1 + data[0]])
        elif ext_type == 0x10 and len(data) >= 2:  # ALPN
            pos = 2
            while pos < len(data):
                name_len = data[pos]
                alpn.append(data[pos + 1 : pos + 1 + name_len].decode("ascii", "replace"))
                pos += 1 + name_len
        elif ext_type == 0x0D and len(data) >= 2:  # signature_algorithms
            (list_len,) = struct.unpack("!H", data[:2])
            sig_algs = [
                struct.unpack("!H", data[i : i + 2])[0]
                for i in range(2, 2 + list_len, 2)
            ]
        elif ext_type == 0x2B and len(data) >= 1:  # supported_versions
            list_len = data[0]
            supported_versions = [
                struct.unpack("!H", data[i : i + 2])[0]
                for i in range(1, 1 + list_len, 2)
            ]
        elif ext_type == 0x33 and len(data) >= 2:  # key_share
            (list_len,) = struct.unpack("!H", data[:2])
            pos = 2
            while pos + 4 <= 2 + list_len:
                group, share_len = struct.unpack("!HH", data[pos : pos + 4])
                key_share_groups.append(group)
                pos += 4 + share_len

    ja3_parts = [
        str(legacy_version),
        "-".join(str(c) for c in ciphers if not is_grease(c)),
        "-".join(str(e) for e in extensions if not is_grease(e)),
        "-".join(str(c) for c in curves if not is_grease(c)),
        "-".join(str(p) for p in point_formats),
    ]
    ja3_str = ",".join(ja3_parts)
    return {
        "ja3": ja3_str,
        "ja3_md5": hashlib.md5(ja3_str.encode()).hexdigest(),
        "ciphers": [hex(c) for c in ciphers if not is_grease(c)],
        "extensions": [hex(e) for e in extensions if not is_grease(e)],
        "curves": [hex(c) for c in curves if not is_grease(c)],
        "alpn": alpn,
        "sig_algs": [hex(s) for s in sig_algs],
        "supported_versions": [hex(v) for v in supported_versions if not is_grease(v)],
        "key_share_groups": [hex(g) for g in key_share_groups if not is_grease(g)],
    }


def main() -> None:
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8443
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", port))
    server.listen(16)
    print(f"listening on 127.0.0.1:{port}", flush=True)
    index = 0
    while True:
        conn, addr = server.accept()
        index += 1
        try:
            conn.settimeout(5)
            info = parse_client_hello(conn)
            print(f"--- connection #{index} from {addr[0]}:{addr[1]} ---", flush=True)
            for key in (
                "ja3_md5",
                "ja3",
                "alpn",
                "supported_versions",
                "curves",
                "key_share_groups",
                "sig_algs",
                "ciphers",
                "extensions",
            ):
                print(f"{key}: {info[key]}", flush=True)
        except Exception as exc:  # noqa: BLE001
            print(f"--- connection #{index}: parse error: {exc} ---", flush=True)
        finally:
            conn.close()


if __name__ == "__main__":
    main()
