#!/bin/sh
# 用官方 Docker 镜像里的 Excalidraw 静态构建打包出 bytehost 应用目录。
# 用法: package.sh <输出应用目录> [镜像,默认 excalidraw/excalidraw:latest]
# 需要 docker(只用来取文件,不运行容器)与 python3;打包时会联网下载 3 个 Assistant 字重。
set -eu
OUT="${1:?用法: package.sh <输出应用目录> [镜像]}"
IMAGE="${2:-excalidraw/excalidraw:latest}"
HERE="$(cd "$(dirname "$0")" && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
docker pull "$IMAGE" >/dev/null
C="$(docker create "$IMAGE")"
docker cp "$C:/usr/share/nginx/html" "$TMP/html"
docker rm "$C" >/dev/null
echo "镜像摘要: $(docker image inspect --format '{{index .RepoDigests 0}}' "$IMAGE")"
python3 "$HERE/package.py" "$TMP/html" "$OUT"
