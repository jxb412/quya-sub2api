#!/usr/bin/env python3
"""纯标准库 Ed25519（RFC 8032 参考实现）：密钥生成 + 签名。

用法:
  python3 tools/ed25519_tool.py keygen <key-file>     # 生成私钥(32字节种子,hex)
  python3 tools/ed25519_tool.py pubkey <key-file>     # 输出 Base64 公钥
  python3 tools/ed25519_tool.py sign <key-file> <msg-file>  # 输出 Base64 签名

私钥文件只保存在发布者本地，绝不放入插件包、仓库或部署环境。
"""

import base64
import hashlib
import os
import secrets
import sys

p = 2**255 - 19
q = 2**252 + 27742317777372353535851937790883648493


def sha512(s: bytes) -> bytes:
    return hashlib.sha512(s).digest()


def sha512_modq(s: bytes) -> int:
    return int.from_bytes(sha512(s), "little") % q


def modp_inv(x: int) -> int:
    return pow(x, p - 2, p)


d = -121665 * modp_inv(121666) % p
modp_sqrt_m1 = pow(2, (p - 1) // 4, p)


def point_add(P, Q):
    A = (P[1] - P[0]) * (Q[1] - Q[0]) % p
    B = (P[1] + P[0]) * (Q[1] + Q[0]) % p
    C = 2 * P[3] * Q[3] * d % p
    D = 2 * P[2] * Q[2] % p
    E, F, G_, H = B - A, D - C, D + C, B + A
    return (E * F % p, G_ * H % p, F * G_ % p, E * H % p)


def point_mul(s: int, P):
    Q = (0, 1, 1, 0)
    while s > 0:
        if s & 1:
            Q = point_add(Q, P)
        P = point_add(P, P)
        s >>= 1
    return Q


def recover_x(y: int, sign_bit: int):
    if y >= p:
        return None
    x2 = (y * y - 1) * modp_inv(d * y * y + 1)
    if x2 == 0:
        return None if sign_bit else 0
    x = pow(x2, (p + 3) // 8, p)
    if (x * x - x2) % p != 0:
        x = x * modp_sqrt_m1 % p
    if (x * x - x2) % p != 0:
        return None
    if (x & 1) != sign_bit:
        x = p - x
    return x


g_y = 4 * modp_inv(5) % p
g_x = recover_x(g_y, 0)
G = (g_x, g_y, 1, g_x * g_y % p)


def point_compress(P) -> bytes:
    zinv = modp_inv(P[2])
    x = P[0] * zinv % p
    y = P[1] * zinv % p
    return int.to_bytes(y | ((x & 1) << 255), 32, "little")


def secret_expand(secret: bytes):
    if len(secret) != 32:
        raise ValueError("private key seed must be 32 bytes")
    h = sha512(secret)
    a = int.from_bytes(h[:32], "little")
    a &= (1 << 254) - 8
    a |= 1 << 254
    return a, h[32:]


def secret_to_public(secret: bytes) -> bytes:
    a, _ = secret_expand(secret)
    return point_compress(point_mul(a, G))


def sign(secret: bytes, msg: bytes) -> bytes:
    a, prefix = secret_expand(secret)
    A = point_compress(point_mul(a, G))
    r = sha512_modq(prefix + msg)
    R = point_mul(r, G)
    Rs = point_compress(R)
    h = sha512_modq(Rs + A + msg)
    s = (r + h * a) % q
    return Rs + int.to_bytes(s, 32, "little")


def load_seed(path: str) -> bytes:
    seed = bytes.fromhex(open(path, encoding="ascii").read().strip())
    if len(seed) != 32:
        raise SystemExit("invalid key file (expect 64 hex chars)")
    return seed


def main() -> None:
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    command, key_path = sys.argv[1], sys.argv[2]
    if command == "keygen":
        if os.path.exists(key_path):
            raise SystemExit(f"refusing to overwrite existing key: {key_path}")
        seed = secrets.token_bytes(32)
        fd = os.open(key_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as handle:
            handle.write(seed.hex() + "\n")
        print(f"private key written to {key_path} (keep it secret)")
        print(f"public key (base64): {base64.b64encode(secret_to_public(seed)).decode()}")
    elif command == "pubkey":
        seed = load_seed(key_path)
        print(base64.b64encode(secret_to_public(seed)).decode())
    elif command == "sign":
        if len(sys.argv) < 4:
            raise SystemExit("usage: sign <key-file> <msg-file>")
        seed = load_seed(key_path)
        message = open(sys.argv[3], "rb").read()
        print(base64.b64encode(sign(seed, message)).decode())
    else:
        raise SystemExit(f"unknown command: {command}")


if __name__ == "__main__":
    main()
