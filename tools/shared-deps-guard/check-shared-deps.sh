#!/usr/bin/env bash
# 共享库依赖守卫入口。规格:digger 仓库 docs/superpowers/specs/2026-10-16-shared-deps-guard-design.md
#
# 用法:scripts/check-shared-deps.sh [--offline] [--rev <提交>] [--downgrade-blocks]
#   无参数        检查工作区,输出完整报告(含落后提示),有阻断时退出码为 1
#   --rev <提交>  检查该提交里的内容(pre-push 用它检查将被推送的提交)
#   --offline     跳过联网检查
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
exec python3 -I "$here/check_shared_deps.py" --root "$root" "$@"
