#!/usr/bin/env bash
# 联合构建检查(bytehost 优化 Tier5 Task4 的**本地**形态)。
#
# bytehost 改动后,用 sibling 的 dozer / digger 各跑一次 `cargo check`,确认两消费方
# 没被破坏:
#
#   scripts/joint-build.sh                 # 两个都跑(sibling 默认 ..)
#   scripts/joint-build.sh dozer
#   scripts/joint-build.sh digger
#   CONSUMERS_ROOT=/path/to/byteboy scripts/joint-build.sh
#
# 原理:用 `cargo --config` 把 10 个 bytehost crate 临时 patch 到**本工作树**
# (与人工联调同一手法),不写任何 `.cargo/config.toml`、不覆盖用户已有的本地 patch。
# 被这次解析改动的消费方 `Cargo.lock` 会在结束时还原。
#
# PR/tag/手动联合检查由 .github/workflows/joint-build.yml 调用。
# 此脚本仅做候选组合；消费方发行检查必须另用不带 patch 的锁文件。

set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
sibling="${CONSUMERS_ROOT:-$(dirname "$root")}"
which="${1:-all}"

dozer_dir="${DOZER_DIR:-$sibling/dozer}"
digger_dir="${DIGGER_DIR:-$sibling/digger}"

# 每个消费方需要 patch 的 crate:名字与目录一一对应。
crates=(
  bytehost-apps
  bytehost-chrome
  bytehost-client
  bytehost-core
  bytehost-desktop
  bytehost-git-accounts
  bytehost-panel
  bytehost-settings
  bytehost-settings-ui
  bytehost-webview
)

patch_args=()
for crate in "${crates[@]}"; do
  patch_args+=(
    --config
    "patch.\"https://github.com/byteboyai/bytehost\".${crate}.path=\"${root}/crates/${crate}\""
  )
done

build_one() (
  local name="$1" dir="$2" pkg="$3"
  if [ ! -f "$dir/Cargo.toml" ]; then
    echo "error: 缺少 $name($dir),联合构建失败" >&2
    return 1
  fi
  local lock_bak
  lock_bak="$(mktemp)"
  cp "$dir/Cargo.lock" "$lock_bak"
  cleanup() {
    local rc=$?
    trap - EXIT
    cp "$lock_bak" "$dir/Cargo.lock" || rc=1
    rm -f "$lock_bak"
    exit "$rc"
  }
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  echo "联合构建:$name($pkg 与 workspace 全目标)…" >&2
  if ! (cd "$dir" && cargo check --workspace --all-targets "${patch_args[@]}"); then
    echo "联合构建失败:$name 在 bytehost 当前工作树下编译不过" >&2
    return 1
  fi
  echo "联合构建通过:$name" >&2
)

status=0
case "$which" in
  dozer) build_one dozer "$dozer_dir" dozer-app || status=1 ;;
  digger) build_one digger "$digger_dir" digger-desktop || status=1 ;;
  all)
    build_one dozer "$dozer_dir" dozer-app || status=1
    build_one digger "$digger_dir" digger-desktop || status=1
    ;;
  *)
    echo "用法:$0 [dozer|digger|all]" >&2
    exit 2
    ;;
esac

if [ "$status" -ne 0 ]; then
  echo "联合构建未全部通过" >&2
fi
exit "$status"
