"""Py Notes —— bytehost 的 Python 示例应用(仅标准库)。

行为(与 node-notes 完全一致):
  GET  /         HTML 页(计数 + 运行时版本 + EventSource/WebSocket 小脚本)
  POST /hit      计数 +1,返回 JSON(用于同源检查与持久化)
  GET  /events   SSE,每 200ms 发一条 `data: tick N`,共 5 条后保持连接空闲
  GET  /ws       手写 RFC 6455 握手,文本帧原样回显
  GET  /pid      返回进程号(用于观察重启)
  GET  /hang     此后不再应答任何请求(触发宿主健康检查)
  GET  /crash    立即以退出码 3 退出(触发重启)

端口取环境变量 `PORT`,只绑 `127.0.0.1`;计数存在 `BYTEHOST_DATA_DIR/counter.txt`。
"""

import base64
import hashlib
import json
import os
import socketserver
import struct
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler


class Server(socketserver.ThreadingTCPServer):
    # `ThreadingHTTPServer.server_bind` 会调用 `socket.getfqdn`,在没有反向 DNS 的主机上
    # (如 CI runner)会卡住几十秒才 serve_forever;这里跳过它,其余语义与前者一致。
    allow_reuse_address = True
    daemon_threads = True

DATA_DIR = os.environ["BYTEHOST_DATA_DIR"]
PORT = int(os.environ["PORT"])
RUNNING = sys.version.split()[0]

# /hang 之后置位:所有处理函数(含健康检查的 /)都卡住,于是宿主探测超时。
_hang = threading.Event()
_counter_lock = threading.Lock()


def _counter_path():
    return os.path.join(DATA_DIR, "counter.txt")


def _read_counter():
    try:
        with open(_counter_path(), "r", encoding="utf-8") as f:
            return int(f.read().strip() or "0")
    except (OSError, ValueError):
        return 0


def _write_counter(n):
    os.makedirs(DATA_DIR, exist_ok=True)
    with open(_counter_path(), "w", encoding="utf-8") as f:
        f.write(str(n))


def _index_html(counter):
    return f"""<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Py Notes</title></head>
<body>
<h1>Py Notes</h1>
<p id="count">hits: {counter}</p>
<p>runtime: Python {RUNNING}</p>
<pre id="events"></pre>
<pre id="wslog"></pre>
<script>
  var es = new EventSource('/events');
  es.onmessage = function (e) {{ document.getElementById('events').textContent += e.data + "\\n"; }};
  fetch('/hit', {{ method: 'POST' }}).then(function (r) {{ return r.json(); }})
    .then(function (d) {{ document.getElementById('count').textContent = 'hits: ' + d.hits; }});
  var ws = new WebSocket('ws://' + location.host + '/ws');
  ws.onopen = function () {{ ws.send('hello'); }};
  ws.onmessage = function (e) {{ document.getElementById('wslog').textContent += 'ws: ' + e.data + "\\n"; }};
</script>
</body>
</html>
"""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # 静音默认的 stderr 访问日志
        pass

    def _hang_forever(self):
        # 已经进入 /hang 状态:卡住这条连接直到超时。
        time.sleep(3600)

    def _send(self, code, body, content_type):
        data = body.encode("utf-8") if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if _hang.is_set():
            self._hang_forever()
            return
        if self.path == "/":
            self._send(200, _index_html(_read_counter()), "text/html; charset=utf-8")
        elif self.path == "/pid":
            self._send(200, str(os.getpid()), "text/plain")
        elif self.path == "/events":
            self._events()
        elif self.path == "/ws":
            self._ws()
        elif self.path == "/hang":
            _hang.set()
            self._hang_forever()
        elif self.path == "/crash":
            os._exit(3)
        else:
            self._send(404, "not found", "text/plain")

    def do_POST(self):
        if _hang.is_set():
            self._hang_forever()
            return
        if self.path == "/hit":
            with _counter_lock:
                n = _read_counter() + 1
                _write_counter(n)
            self._send(200, json.dumps({"hits": n}), "application/json")
        else:
            self._send(404, "not found", "text/plain")

    def _events(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "keep-alive")
        self.end_headers()
        for i in range(1, 6):
            self.wfile.write(f"data: tick {i}\n\n".encode("utf-8"))
            self.wfile.flush()
            time.sleep(0.2)
        # 发完 5 条后保持连接空闲(客户端会看到流已开始但未结束)。

    def _ws(self):
        key = self.headers.get("Sec-WebSocket-Key", "")
        accept = base64.b64encode(
            hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("utf-8")).digest()
        ).decode("ascii")
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        # 单帧回显:读一帧文本,原样回一帧文本。
        try:
            payload = self._read_ws_frame()
            if payload is not None:
                self.wfile.write(self._ws_frame(payload))
                self.wfile.flush()
        except OSError:
            pass

    def _read_ws_frame(self):
        hdr = self.rfile.read(2)
        if len(hdr) < 2:
            return None
        _b1, b2 = hdr[0], hdr[1]
        length = b2 & 0x7F
        if length == 126:
            length = struct.unpack(">H", self.rfile.read(2))[0]
        elif length == 127:
            length = struct.unpack(">Q", self.rfile.read(8))[0]
        mask = self.rfile.read(4) if (b2 & 0x80) else b""
        data = self.rfile.read(length)
        if mask:
            data = bytes(c ^ mask[i % 4] for i, c in enumerate(data))
        return data

    @staticmethod
    def _ws_frame(payload):
        n = len(payload)
        if n < 126:
            header = struct.pack(">BB", 0x81, n)
        else:
            header = struct.pack(">BBH", 0x81, 126, n)
        return header + payload


def main():
    print("listening on", PORT, flush=True)
    server = Server(("127.0.0.1", PORT), Handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
