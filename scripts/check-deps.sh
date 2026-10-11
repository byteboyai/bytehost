#!/usr/bin/env bash
# 门禁:bytehost 各 crate 必须与产品、界面框架、平台 webview 库解耦。分三组:
#
#   group "apps"(应用平台四 crate:bytehost-apps/-client/-webview/-panel)
#     - 依赖树里都不得出现 dozer*、digger*、iced*、wry、tauri*、objc2*(含 build 依赖与所有目标平台);
#     - bytehost-apps 默认 feature 的依赖闭包只能是 serde 家族;
#     - bytehost-panel 的依赖只能是 bytehost-apps、bytehost-client 及其传递依赖;
#     - bytehost-panel 源码里不得出现 `AppSlot` 或小写 `toast`。
#
#   group "core"(纯契约层 bytehost-core)
#     - 不得出现 dozer*、digger*、iced*、wry、tauri*、objc2*;
#     - 依赖闭包只允许 serde 家族(core 不得拖入任何界面/平台库)。
#
#   group "desktop"(桌面宿主 bytehost-desktop;Task 07 起存在)
#     - 允许 iced*/wry(它就是要实现窗口),但**不得**出现 dozer*、digger*、tauri*、objc2*。
#     - 尚不存在时跳过(新建后自动纳入)。
#
# `--self-test` 追加门禁变异测试:临时注入(1)core→iced 依赖、(2)apps→digger 依赖,
# 断言门禁**必须失败**,随后恢复并断言通过。用于证明门禁不是恒真。
set -euo pipefail
cd "$(dirname "$0")/.."

# cargo tree 失败必须让门禁失败(不能被管道/`|| true` 吞掉);边类型含 build-dependency,
# 并覆盖所有目标平台(`--target all`),否则会漏掉 build 依赖和 cfg(target_os=…) 下的依赖。
tree_of() {
  local pkg="$1"; shift
  local out
  out=$(cargo tree -p "$pkg" -e normal,build --target all --prefix none "$@" 2>&1) || {
    echo "cargo tree 失败:" >&2
    echo "$out" >&2
    exit 1
  }
  echo "$out" | awk '{print $1}' | sort -u
}

product_banned='^(dozer.*|digger.*)$'
framework_banned='^(iced.*|wry|tauri.*|objc2.*)$'

check_forbidden() {
  local pkg="$1" pattern="$2" why="$3"
  local tree hits
  tree=$(tree_of "$pkg" --all-features)
  hits=$(echo "$tree" | grep -E "$pattern" || true)
  if [ -n "$hits" ]; then
    echo "$pkg 的依赖里出现了被禁的 crate($why):" >&2
    echo "$hits" >&2
    exit 1
  fi
}

# ---- group: apps ----
for pkg in bytehost-apps bytehost-client bytehost-webview bytehost-panel; do
  check_forbidden "$pkg" "$product_banned" "产品 crate,宿主层不得依赖"
  check_forbidden "$pkg" "$framework_banned" "界面框架/平台 webview,应用平台应为框架中立"
done

# bytehost-apps 默认 feature 的依赖闭包只允许 serde 家族。
apps_default=$(tree_of bytehost-apps)
allowed='^(bytehost-apps|serde|serde_core|serde_derive|serde_json|proc-macro2|quote|syn|unicode-ident|itoa|memchr|zmij)$'
extra=$(echo "$apps_default" | grep -Ev "$allowed" || true)
if [ -n "$extra" ]; then
  echo "bytehost-apps 默认 feature 的依赖闭包里出现了不在白名单里的 crate(新增依赖请放进 feature):" >&2
  echo "$extra" >&2
  exit 1
fi

# bytehost-panel 的直接依赖只允许 bytehost-apps/client。
direct=$(cargo tree -p bytehost-panel --depth 1 --prefix none --all-features 2>/dev/null \
  | awk '{print $1}' | sort -u | grep -v '^bytehost-panel$' || true)
unexpected=$(echo "$direct" | grep -Ev '^(bytehost-apps|bytehost-client)$' || true)
if [ -n "$unexpected" ]; then
  echo "bytehost-panel 的直接依赖超出了 bytehost-apps/bytehost-client:" >&2
  echo "$unexpected" >&2
  exit 1
fi

# bytehost-panel 源码里不得出现 dozer 词汇。
if grep -rn "AppSlot\|toast" crates/bytehost-panel/src; then
  echo "bytehost-panel 源码里出现了 AppSlot/toast(应改用 AppKey/Notice):" >&2
  exit 1
fi

# ---- group: core ----
core_allowed='^(bytehost-core|serde|serde_core|serde_derive|serde_json|proc-macro2|quote|syn|unicode-ident|itoa|memchr|zmij)$'
check_forbidden bytehost-core "$product_banned" "产品 crate,纯契约层不得依赖"
check_forbidden bytehost-core "$framework_banned" "界面框架/平台 webview,纯契约层不得依赖"
core_default=$(tree_of bytehost-core --all-features)
core_extra=$(echo "$core_default" | grep -Ev "$core_allowed" || true)
if [ -n "$core_extra" ]; then
  echo "bytehost-core 的依赖闭包里出现了不在白名单里的 crate(纯契约层只允许 serde 家族):" >&2
  echo "$core_extra" >&2
  exit 1
fi

# ---- group: desktop(存在才检查)----
if cargo metadata --no-deps --format-version 1 2>/dev/null | grep -q '"name":"bytehost-desktop"'; then
  check_forbidden bytehost-desktop "$product_banned" "产品 crate,共享桌面宿主不得依赖"
  check_forbidden bytehost-desktop '^tauri.*|^objc2.*' "tauri/objc2 不在共享桌面宿主的依赖白名单"
else
  echo "note: bytehost-desktop 尚未创建,跳过 desktop 组(Task 07 起自动纳入)" >&2
fi

echo "bytehost deps check: ok"

# ---- 可选:门禁变异测试 ----
if [ "${1:-}" = "--self-test" ]; then
  echo "运行门禁变异测试…" >&2
  core_bak="$(mktemp)"
  apps_bak="$(mktemp)"
  probe_dir="$(mktemp -d)"
  cleanup() {
    cp "$core_bak" crates/bytehost-core/Cargo.toml
    cp "$apps_bak" crates/bytehost-apps/Cargo.toml
    rm -rf "$probe_dir" "$core_bak" "$apps_bak"
  }
  trap cleanup EXIT
  cp crates/bytehost-core/Cargo.toml "$core_bak"
  cp crates/bytehost-apps/Cargo.toml "$apps_bak"

  # 变异 1:core 临时加入 iced 依赖(注入到 [dependencies] 段)→ 必须失败。
  python3 - "$PWD/crates/bytehost-core/Cargo.toml" <<'PYEOF'
import sys
p = sys.argv[1]
s = open(p).read()
s = s.replace("[dev-dependencies]", "iced = \"0.14\"\n\n[dev-dependencies]", 1)
open(p, "w").write(s)
PYEOF
  if bash "$0" >/dev/null 2>&1; then
    echo "变异测试失败:core 加入 iced 依赖后门禁仍然通过" >&2
    exit 1
  fi
  cp "$core_bak" crates/bytehost-core/Cargo.toml
  echo "  变异 1 通过:core→iced 被拦下" >&2

  # 变异 2:apps 依赖一个包名为 digger-* 的本地 crate → 必须失败。
  mkdir -p "$probe_dir/src"
  cat > "$probe_dir/Cargo.toml" <<'TOMLEOF'
[package]
name = "digger-mutation-probe"
version = "0.0.0"
edition = "2021"
publish = false
[workspace]
TOMLEOF
  echo "" > "$probe_dir/src/lib.rs"
  abs_probe="$(cd "$probe_dir" && pwd)"
  python3 - "$PWD/crates/bytehost-apps/Cargo.toml" "$abs_probe" <<'PYEOF'
import sys
p, probe = sys.argv[1], sys.argv[2]
s = open(p).read()
s = s.replace("[dependencies]", f'[dependencies]\ndigger-mutation-probe = {{ path = "{probe}" }}', 1)
open(p, "w").write(s)
PYEOF
  if bash "$0" >/dev/null 2>&1; then
    echo "变异测试失败:apps 加入 digger-* 依赖后门禁仍然通过" >&2
    exit 1
  fi
  cp "$apps_bak" crates/bytehost-apps/Cargo.toml
  echo "  变异 2 通过:apps→digger-* 被拦下" >&2

  # 恢复后必须通过。
  cleanup
  trap - EXIT
  if ! bash "$0" >/dev/null 2>&1; then
    echo "变异测试失败:恢复后门禁未通过" >&2
    exit 1
  fi
  echo "门禁变异测试:ok(注入即失败、恢复即通过)" >&2
fi
