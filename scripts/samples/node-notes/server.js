"use strict";
// Node Notes —— bytehost 的 Node 示例应用(仅标准库)。
//
// 行为(与 py-notes 完全一致):
//   GET  /         HTML 页(计数 + 运行时版本 + EventSource/WebSocket 小脚本)
//   POST /hit      计数 +1,返回 JSON(用于同源检查与持久化)
//   GET  /events   SSE,每 200ms 发一条 `data: tick N`,共 5 条后保持连接空闲
//   GET  /ws       手写 RFC 6455 握手,文本帧原样回显
//   GET  /pid      返回进程号(用于观察重启)
//   GET  /hang     此后不再应答任何请求(触发宿主健康检查)
//   GET  /crash    立即以退出码 3 退出(触发重启)
//
// 端口取环境变量 `PORT`,只绑 `127.0.0.1`;计数存在 `BYTEHOST_DATA_DIR/counter.txt`。

const http = require("http");
const crypto = require("crypto");
const fs = require("fs");
const path = require("path");

const DATA_DIR = process.env.BYTEHOST_DATA_DIR;
const PORT = parseInt(process.env.PORT, 10);
const RUNNING = process.version.replace(/^v/, "");

let hung = false; // /hang 之后置位:所有处理(含健康检查的 /)都卡住。

function counterPath() {
  return path.join(DATA_DIR, "counter.txt");
}

function readCounter() {
  try {
    const n = parseInt(fs.readFileSync(counterPath(), "utf8").trim(), 10);
    return Number.isFinite(n) ? n : 0;
  } catch (_e) {
    return 0;
  }
}

function writeCounter(n) {
  fs.mkdirSync(DATA_DIR, { recursive: true });
  fs.writeFileSync(counterPath(), String(n));
}

function indexHtml(counter) {
  return `<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Node Notes</title></head>
<body>
<h1>Node Notes</h1>
<p id="count">hits: ${counter}</p>
<p>runtime: Node.js ${RUNNING}</p>
<pre id="events"></pre>
<pre id="wslog"></pre>
<script>
  var es = new EventSource('/events');
  es.onmessage = function (e) { document.getElementById('events').textContent += e.data + "\\n"; };
  fetch('/hit', { method: 'POST' }).then(function (r) { return r.json(); })
    .then(function (d) { document.getElementById('count').textContent = 'hits: ' + d.hits; });
  var ws = new WebSocket('ws://' + location.host + '/ws');
  ws.onopen = function () { ws.send('hello'); };
  ws.onmessage = function (e) { document.getElementById('wslog').textContent += 'ws: ' + e.data + "\\n"; };
</script>
</body>
</html>
`;
}

function hangForever(_req, _res) {
  // 保持连接不返回,直到客户端超时。
}

function send(res, code, body, contentType) {
  const data = Buffer.isBuffer(body) ? body : Buffer.from(String(body), "utf8");
  res.writeHead(code, {
    "Content-Type": contentType,
    "Content-Length": data.length,
  });
  res.end(data);
}

function events(req, res) {
  res.writeHead(200, {
    "Content-Type": "text/event-stream",
    "Cache-Control": "no-cache",
    Connection: "keep-alive",
  });
  let i = 1;
  const timer = setInterval(() => {
    res.write(`data: tick ${i}\n\n`);
    i += 1;
    if (i > 5) {
      clearInterval(timer);
      // 发完 5 条后保持连接空闲。
    }
  }, 200);
}

function ws(req, socket) {
  const key = req.headers["sec-websocket-key"] || "";
  const accept = crypto
    .createHash("sha1")
    .update(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
    .digest("base64");
  socket.write(
    "HTTP/1.1 101 Switching Protocols\r\n" +
      "Upgrade: websocket\r\n" +
      "Connection: Upgrade\r\n" +
      `Sec-WebSocket-Accept: ${accept}\r\n\r\n`
  );
  const readFrame = (buf) => {
    if (buf.length < 2) return null;
    const b2 = buf[1];
    let len = b2 & 0x7f;
    let off = 2;
    if (len === 126) {
      len = buf.readUInt16BE(2);
      off = 4;
    } else if (len === 127) {
      len = Number(buf.readBigUInt64BE(2));
      off = 10;
    }
    let mask = null;
    if (b2 & 0x80) {
      mask = buf.subarray(off, off + 4);
      off += 4;
    }
    const data = Buffer.from(buf.subarray(off, off + len));
    if (mask) {
      for (let i = 0; i < data.length; i++) data[i] ^= mask[i % 4];
    }
    return data;
  };
  const writeFrame = (payload) => {
    const n = payload.length;
    let header;
    if (n < 126) header = Buffer.from([0x81, n]);
    else {
      header = Buffer.alloc(4);
      header[0] = 0x81;
      header[1] = 126;
      header.writeUInt16BE(n, 2);
    }
    return Buffer.concat([header, payload]);
  };
  socket.on("data", (buf) => {
    const payload = readFrame(buf);
    if (payload !== null) socket.write(writeFrame(payload));
  });
  socket.on("error", () => {});
}

const server = http.createServer((req, res) => {
  const url = req.url.split("?")[0];
  if (hung) {
    hangForever(req, res);
    return;
  }
  if (req.method === "GET" && url === "/") {
    send(res, 200, indexHtml(readCounter()), "text/html; charset=utf-8");
  } else if (req.method === "GET" && url === "/pid") {
    send(res, 200, String(process.pid), "text/plain");
  } else if (req.method === "GET" && url === "/events") {
    events(req, res);
  } else if (req.method === "GET" && url === "/hang") {
    hung = true;
    hangForever(req, res);
  } else if (req.method === "GET" && url === "/crash") {
    process.exit(3);
  } else if (req.method === "POST" && url === "/hit") {
    const n = readCounter() + 1;
    writeCounter(n);
    send(res, 200, JSON.stringify({ hits: n }), "application/json");
  } else {
    send(res, 404, "not found", "text/plain");
  }
});

server.on("upgrade", (req, socket) => {
  if (hung) return;
  if (req.url.split("?")[0] === "/ws") ws(req, socket);
});

server.listen(PORT, "127.0.0.1", () => {
  console.log("listening on", PORT);
});
