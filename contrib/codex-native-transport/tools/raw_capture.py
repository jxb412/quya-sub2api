#!/usr/bin/env python3
"""明文 HTTP 原始请求捕获器：逐字节打印请求行 + 头(保序) + body 摘要。

用法: python3 tools/raw_capture.py [port]
返回一个最小 SSE 响应,让客户端认为请求成功。
"""

import socket
import sys


def handle(conn: socket.socket, index: int) -> None:
    conn.settimeout(8)
    data = b""
    # 读到头结束
    while b"\r\n\r\n" not in data:
        chunk = conn.recv(65536)
        if not chunk:
            break
        data += chunk
    head, _, rest = data.partition(b"\r\n\r\n")
    lines = head.split(b"\r\n")
    print(f"===== request #{index} =====", flush=True)
    for line in lines:
        print(f"|{line.decode('utf-8', 'replace')}", flush=True)
    # 读 body(按 content-length)
    content_length = 0
    for line in lines[1:]:
        if line.lower().startswith(b"content-length:"):
            content_length = int(line.split(b":", 1)[1].strip())
    body = rest
    while len(body) < content_length:
        chunk = conn.recv(65536)
        if not chunk:
            break
        body += chunk
    if body:
        text = body[:2000].decode("utf-8", "replace")
        print(f"BODY({len(body)}B): {text}", flush=True)
    print("===== end =====", flush=True)
    response = (
        b"HTTP/1.1 200 OK\r\n"
        b"content-type: text/event-stream\r\n"
        b"connection: close\r\n"
        b"\r\n"
        b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_x\",\"output\":[]}}\n\n"
    )
    try:
        conn.sendall(response)
    except OSError:
        pass


def main() -> None:
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8902
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", port))
    server.listen(16)
    print(f"listening on 127.0.0.1:{port}", flush=True)
    index = 0
    while True:
        conn, _ = server.accept()
        index += 1
        try:
            handle(conn, index)
        except Exception as exc:  # noqa: BLE001
            print(f"request #{index} error: {exc}", flush=True)
        finally:
            conn.close()


if __name__ == "__main__":
    main()
