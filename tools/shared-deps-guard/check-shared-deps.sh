#!/usr/bin/env bash
# 共享库依赖守卫入口。规格:digger 仓库 docs/superpowers/specs/2026-10-16-shared-deps-guard-design.md
#
# 用法:scripts/check-shared-deps.sh [--offline] [--rev <提交>] [--downgrade-blocks]
#   无参数        检查工作区,输出完整报告(含落后提示),有阻断时退出码为 1
#   --rev <提交>  检查该提交里的内容(pre-push 用它检查将被推送的提交)
#   --offline     跳过联网检查
# 退出码:0 没有阻断;1 有阻断;3 脚本自己跑不起来(没有 >= 3.11 的 python、读不到提交、脚本出错)。
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"

# GUI git 客户端/IDE 跑钩子时 PATH 往往很短(macOS 的 /usr/bin/python3 是 3.9),
# 所以按版本从新到旧找一个 >= 3.11 的解释器。
py=""
for candidate in python3.13 python3.12 python3.11 python3; do
  if command -v "$candidate" >/dev/null 2>&1 &&
    "$candidate" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)' >/dev/null 2>&1; then
    py="$candidate"
    break
  fi
done
if [ -z "$py" ]; then
  echo "shared-deps: 找不到 Python 3.11+(试过 python3.13/3.12/3.11/python3);请安装或把它加进 PATH" >&2
  exit 3
fi
exec "$py" -I "$here/check_shared_deps.py" --root "$root" "$@"
