#!/usr/bin/env python3
"""端到端验证 UDP 回包源地址：发往 127.0.0.2 的查询，回包必须来自 127.0.0.2。

这是 VIP 补丁（IP_PKTINFO）的唯一硬门禁 —— 单测只覆盖 socket 层，这里跑的是
真正编出来的二进制：起一个 mock 上游 + 用最小配置起 atomdns，再从 127.0.0.2
发一条查询，检查回包源地址。

用法：
    python3 .github/ci/verify_reply_source.py --binary ./stage/atomdns

退出码 0 = 补丁生效；1 = 补丁失效或程序没起来。
"""

import argparse
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

QNAME = "probe.example.com"


def free_port(kind=socket.SOCK_DGRAM):
    s = socket.socket(socket.AF_INET, kind)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def dns_query(ident=0x1234):
    qname = b"".join(bytes([len(p)]) + p.encode() for p in QNAME.split(".")) + b"\x00"
    return struct.pack(">HHHHHH", ident, 0x0100, 1, 0, 0, 0) + qname + struct.pack(">HH", 1, 1)


class MockUpstream:
    """收到什么回什么（置 QR 位），够 atomdns 当成一次正常应答。"""

    def __init__(self, port):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", port))
        self.sock.settimeout(0.5)
        self.port = port
        self._stop = False

    def serve(self):
        while not self._stop:
            try:
                data, peer = self.sock.recvfrom(4096)
            except socket.timeout:
                continue
            except OSError:
                return
            if len(data) < 12:
                continue
            resp = bytearray(data)
            resp[2] |= 0x80
            try:
                self.sock.sendto(bytes(resp), peer)
            except OSError:
                return

    def close(self):
        self._stop = True
        try:
            self.sock.close()
        except OSError:
            pass


def write_config(path, listen_port, api_port, upstream_port):
    path.write_text(
        "log:\n"
        "  level: error\n"
        "api:\n"
        f"  http: '127.0.0.1:{api_port}'\n"
        "plugins:\n"
        "  - tag: mock_upstream\n"
        "    type: forward\n"
        "    args:\n"
        "      upstreams:\n"
        "        - tag: mock\n"
        f"          addr: 'udp://127.0.0.1:{upstream_port}'\n"
        "  - tag: main_entry\n"
        "    type: sequence\n"
        "    args:\n"
        "      - exec: $mock_upstream\n"
        "  - tag: main_udp\n"
        "    type: udp_server\n"
        "    args:\n"
        "      entry: main_entry\n"
        f"      listen: ':{listen_port}'\n"
        "      enable_audit: false\n",
        encoding="utf-8",
    )


def probe(host, port, timeout=2.0):
    """返回 (peer_ip, ok)；超时返回 (None, False)。"""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(timeout)
    try:
        s.sendto(dns_query(), (host, port))
        data, peer = s.recvfrom(4096)
        return peer[0], len(data) >= 12
    except (socket.timeout, OSError):
        return None, False
    finally:
        s.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True)
    ap.add_argument("--wait", type=float, default=25.0)
    args = ap.parse_args()

    binary = os.path.abspath(args.binary)
    if not os.access(binary, os.X_OK):
        print(f"FAIL 二进制不存在或不可执行: {binary}")
        return 1

    work = tempfile.mkdtemp(prefix="verify-reply-src-")
    listen_port = free_port()
    api_port = free_port(socket.SOCK_STREAM)
    upstream_port = free_port()
    config = os.path.join(work, "config.yaml")
    log_path = os.path.join(work, "run.log")

    write_config(Path(config), listen_port, api_port, upstream_port)

    mock = MockUpstream(upstream_port)
    threading.Thread(target=mock.serve, daemon=True).start()

    log = open(log_path, "wb")
    proc = subprocess.Popen(
        [binary, "start", "-d", work, "-c", config],
        stdout=log,
        stderr=subprocess.STDOUT,
    )

    failures = []
    try:
        # 等监听就绪：先探 127.0.0.1（对照组），最多等 --wait 秒
        deadline = time.time() + args.wait
        ready = False
        while time.time() < deadline:
            if proc.poll() is not None:
                break
            peer, ok = probe("127.0.0.1", listen_port, timeout=0.5)
            if ok:
                ready = True
                if peer != "127.0.0.1":
                    failures.append(f"对照组 127.0.0.1 回包源是 {peer}")
                break
            time.sleep(0.3)

        if not ready:
            print("FAIL atomdns 没能起起来或不回包，日志：")
            print(open(log_path, "r", errors="replace").read()[-3000:])
            return 1

        # 真正的门禁：发往 127.0.0.2，回包源必须是 127.0.0.2
        peer, ok = probe("127.0.0.2", listen_port, timeout=3.0)
        if not ok:
            failures.append("发往 127.0.0.2 没有回包（补丁没生效，客户端会超时）")
        elif peer != "127.0.0.2":
            failures.append(f"发往 127.0.0.2 的回包源是 {peer}（应为 127.0.0.2，VIP 上客户端会丢弃）")
        else:
            print(f"OK   发往 127.0.0.2 的回包源 = {peer}")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
        mock.close()
        log.close()

    if failures:
        for f in failures:
            print(f"FAIL {f}")
        print("--- atomdns 日志 ---")
        print(open(log_path, "r", errors="replace").read()[-3000:])
        return 1

    print(f"PASS 回包源地址取自收包目的地址（端口 {listen_port}）")
    shutil.rmtree(work, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
