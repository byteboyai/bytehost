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
# 原理:用 `cargo --config` 把 9 个 bytehost crate 临时 patch 到**本工作树**
# (与人工联调同一手法),不写任何 `.cargo/config.toml`、不覆盖用户已有的本地 patch。
# 被这次解析改动的消费方 `Cargo.lock` 会在结束时还原。
#
# 为什么不是 GitHub Actions:dozer 与 digger 目前都没有任何 workflow;跨仓联合 CI
# 需要它们先有可复用 workflow 与一个跨仓 token。等这两件事具备后,本脚本的检查项
# 可以直接搬进 bytehost 的发版 workflow(见 docs/superpowers/plans/2026-10-30-bytehost-opt-tier5-cross-repo-governance.md)。

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

build_one() {
  local name="$1" dir="$2" pkg="$3"
  if [ ! -f "$dir/Cargo.toml" ]; then
    echo "note: 没有 $name($dir),跳过联合构建" >&2
    return 0
  fi
  local lock_bak rc=0
  lock_bak="$(mktemp)"
  cp "$dir/Cargo.lock" "$lock_bak"
  echo "联合构建:$name($pkg)…" >&2
  (cd "$dir" && cargo check -q -p "$pkg" "${patch_args[@]}") || rc=$?
  # 这次解析会把消费方的 Cargo.lock 改成本地 path;还原成它原来的内容。
  cp "$lock_bak" "$dir/Cargo.lock"
  rm -f "$lock_bak"
  if [ "$rc" -ne 0 ]; then
    echo "联合构建失败:$name 在 bytehost 当前工作树下编译不过" >&2
    return 1
  fi
  echo "联合构建通过:$name" >&2
}

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
