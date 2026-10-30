#!/usr/bin/env bash
# gateway 静态资源热路径基准(bytehost 优化 Tier4 Task0)。
#
# 用法:scripts/bench-gateway.sh [并发] [请求数]
#   默认 64 并发、5000 请求;结果含 p50/p99/max、吞吐与每请求分配次数。
#   改前/改后在同机器上跑同样参数对比(建议各跑 3 次取中位数)。
set -euo pipefail
cd "$(dirname "$0")/.."

exec cargo run --release -q -p bytehost-apps --example gateway_bench --features server -- \
  "${1:-64}" "${2:-5000}"
