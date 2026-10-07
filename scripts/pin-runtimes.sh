#!/usr/bin/env bash
#
# 从官方源生成 bytehost 的**固定版本表**(`pins.rs` 的数据段)。
#
# 用法:`scripts/bytehost/pin-runtimes.sh <node-version> <uv-version>`
#   node-version: 形如 `24.21.0`(不带前导 `v`)
#   uv-version:   形如 `0.12.23`(不带前导 `v`)
#
# 每个可装物只有一个 Pin { name, version, target, url, sha256, strip_components, bin_rel }。
# **校验和只能由本脚本直接 curl 官方源取得**,不许手写、不许模型转述。Node 的校验和取自
# https://nodejs.org/dist/v<V>/SHASUMS256.txt;uv 的取自 GitHub release 的同名 `.sha256` 资产。
# Node 若本机有 `gpg` 且能取到 `SHASUMS256.txt.sig`,则验签;验不了就打印醒目的 UNVERIFIED-SIGNATURE。
#
# 输出(Rust 字面量)打印到 stdout;任何一步失败整体失败,不产出部分结果。

set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "用法: $0 <node-version> <uv-version>" >&2
  echo "例:   $0 24.21.0 0.12.23" >&2
  exit 2
fi

NODE_VERSION="$1"
UV_VERSION="$2"

# 只在 macOS 上发;fixed 的两个 target 与 Rust 里的 Target 枚举一一对应。
# target 标签用 Rust 里的 `Target::{Aarch64Apple,X86_64Apple}`,arch 段用官方文件名里的那段。
NODE_ARCHES=("arm64:Aarch64Apple" "x64:X86_64Apple")
UV_TRIPLES=("aarch64-apple-darwin:Aarch64Apple" "x86_64-apple-darwin:X86_64Apple")

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() { echo "ERROR: $*" >&2; exit 1; }

# ---------- Node ----------
NODE_SHASUMS_URL="https://nodejs.org/dist/v${NODE_VERSION}/SHASUMS256.txt"
echo "取 Node 校验和: $NODE_SHASUMS_URL" >&2
curl -fsSL --max-time 60 "$NODE_SHASUMS_URL" -o "$WORK/node-shasums.txt" \
  || fail "下载 Node SHASUMS256.txt 失败"

# 尽量验签:有 gpg、且官方给出了 SHASUMS256.txt.sig 就验;否则醒目提示。
NODE_SIG_URL="${NODE_SHASUMS_URL}.sig"
if command -v gpg >/dev/null 2>&1; then
  if curl -fsSL --max-time 60 "$NODE_SIG_URL" -o "$WORK/node-shasums.txt.sig" 2>/dev/null; then
    # Node 发布用的 GPG key 官方页:https://github.com/nodejs/node#release-keys
    # 这里不内置公钥:导入公钥是人工步骤。没有公钥时 gpg 无法验,退化为 UNVERIFIED。
    if gpg --verify "$WORK/node-shasums.txt.sig" "$WORK/node-shasums.txt" >/dev/null 2>&1; then
      echo "Node SHASUMS256.txt GPG 验签:OK" >&2
    else
      echo "UNVERIFIED-SIGNATURE: 取到了 Node SHASUMS256.txt.sig,但本机 gpg 无法验证(公钥未导入?);校验和未验签。" >&2
    fi
  else
    echo "UNVERIFIED-SIGNATURE: 取不到 Node SHASUMS256.txt.sig;校验和未验签,请人工比对官方页面。" >&2
  fi
else
  echo "UNVERIFIED-SIGNATURE: 本机没有 gpg;Node 校验和未验签,请人工比对官方页面。" >&2
fi

# ---------- uv ----------
declare -A UV_SHA
for entry in "${UV_TRIPLES[@]}"; do
  triple="${entry%%:*}"
  echo "取 uv 校验和: uv-${triple}.tar.gz.sha256" >&2
  sha_file="$WORK/uv-${triple}.sha256"
  curl -fsSL --max-time 60 \
    "https://github.com/astral-sh/uv/releases/download/${UV_VERSION}/uv-${triple}.tar.gz.sha256" \
    -o "$sha_file" || fail "下载 uv ${triple} 的 .sha256 失败"
  # .sha256 文件形如 "<hex>  uv-<triple>.tar.gz"
  UV_SHA["$triple"]="$(awk '{print $1}' "$sha_file")"
done

# ---------- 输出 Rust 字面量 ----------
emit_pin() {
  local name="$1" version="$2" target="$3" url="$4" sha="$5" strip="$6" bin_rel="$7"
  echo "    Pin {"
  echo "        name: \"${name}\","
  echo "        version: \"${version}\","
  echo "        target: Target::${target},"
  echo "        url: \"${url}\","
  echo "        sha256: \"${sha}\","
  echo "        strip_components: ${strip},"
  echo "        bin_rel: \"${bin_rel}\","
  echo "    },"
}

echo "// 本数据段由 scripts/bytehost/pin-runtimes.sh 从官方源生成,请勿手写。"
echo "// 生成命令: scripts/bytehost/pin-runtimes.sh ${NODE_VERSION} ${UV_VERSION}"
echo "// Node ${NODE_VERSION} LTS, uv ${UV_VERSION}。"
echo "pub const PINS: &[Pin] = &["

for entry in "${NODE_ARCHES[@]}"; do
  arch="${entry%%:*}"
  target="${entry##*:}"
  file="node-v${NODE_VERSION}-darwin-${arch}.tar.gz"
  sha="$(awk -v f="$file" '$2 == f {print $1}' "$WORK/node-shasums.txt")"
  [ -n "$sha" ] || fail "SHASUMS256.txt 里找不到 $file"
  url="https://nodejs.org/dist/v${NODE_VERSION}/${file}"
  # node 包解压后是 `node-v<ver>-darwin-<arch>/bin/node`,strip 掉顶层目录
  emit_pin "node" "$NODE_VERSION" "$target" "$url" "$sha" 1 "bin/node"
done

for entry in "${UV_TRIPLES[@]}"; do
  triple="${entry%%:*}"
  target="${entry##*:}"
  file="uv-${triple}.tar.gz"
  sha="${UV_SHA[$triple]}"
  [ -n "$sha" ] || fail "uv ${triple} 校验和为空"
  url="https://github.com/astral-sh/uv/releases/download/${UV_VERSION}/${file}"
  # uv 包解压后根下就是 `uv`/`uvx`(无顶层目录)
  emit_pin "uv" "$UV_VERSION" "$target" "$url" "$sha" 0 "uv"
done

echo "];"
