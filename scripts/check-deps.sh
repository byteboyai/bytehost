#!/usr/bin/env bash
# 门禁:bytehost 各 crate 必须与产品、界面框架、平台 webview 库解耦。
#  1. 任一 crate 的依赖树里都不得出现 dozer*、iced*、wry、tauri*、objc2*(含 build 依赖与所有目标平台);
#  2. bytehost-apps 默认 feature 的依赖闭包只能是 serde 家族(dozer-core 依赖它时 hook/mcp 不多出新 crate);
#  3. bytehost-panel 的依赖只能是 bytehost-apps、bytehost-client 及其传递依赖;
#  4. bytehost-panel 源码里不得出现 `AppSlot` 或小写 `toast` 字样(它只认 `AppKey` 与 `Notice`)。
set -euo pipefail
cd "$(dirname "$0")/.."

# 注意:`cargo tree` 失败必须让门禁失败(不能被管道/`|| true` 吞掉);边类型含 build-dependency,
# 并覆盖所有目标平台(`--target all`),否则会漏掉 build 依赖和 `cfg(target_os = …)` 下的依赖。
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

# 1. 四个 crate 在 --all-features 下都不得出现被禁依赖。
banned='^(dozer.*|iced.*|wry|tauri.*|objc2.*)$'
for pkg in bytehost-apps bytehost-client bytehost-webview bytehost-panel; do
  tree=$(tree_of "$pkg" --all-features)
  hits=$(echo "$tree" | grep -E "$banned" || true)
  if [ -n "$hits" ]; then
    echo "$pkg 的依赖里出现了被禁的 crate:" >&2
    echo "$hits" >&2
    exit 1
  fi
done

# 2. bytehost-apps 默认 feature 的依赖闭包只允许 serde 家族。
apps_default=$(tree_of bytehost-apps)
allowed='^(bytehost-apps|serde|serde_core|serde_derive|serde_json|proc-macro2|quote|syn|unicode-ident|itoa|memchr|zmij)$'
extra=$(echo "$apps_default" | grep -Ev "$allowed" || true)
if [ -n "$extra" ]; then
  echo "bytehost-apps 默认 feature 的依赖闭包里出现了不在白名单里的 crate(新增依赖请放进 feature):" >&2
  echo "$extra" >&2
  exit 1
fi

# 3. bytehost-panel 的依赖只能是 bytehost-apps、bytehost-client 及其传递依赖。
panel=$(tree_of bytehost-panel --all-features)
panel_banned='^(iced.*|wry|tauri.*|objc2.*|dozer.*)$'
hits=$(echo "$panel" | grep -E "$panel_banned" || true)
if [ -n "$hits" ]; then
  echo "bytehost-panel 的依赖里出现了被禁的 crate:" >&2
  echo "$hits" >&2
  exit 1
fi
# panel 的直接依赖(去掉 bytehost-apps/client 的传递闭包)只允许 bytehost-apps/client。
# 用 `cargo tree -p bytehost-panel --depth 1` 取直接依赖名单。
direct=$(cargo tree -p bytehost-panel --depth 1 --prefix none --all-features 2>/dev/null \
  | awk '{print $1}' | sort -u | grep -v '^bytehost-panel$' || true)
unexpected=$(echo "$direct" | grep -Ev '^(bytehost-apps|bytehost-client)$' || true)
if [ -n "$unexpected" ]; then
  echo "bytehost-panel 的直接依赖超出了 bytehost-apps/bytehost-client:" >&2
  echo "$unexpected" >&2
  exit 1
fi

# 4. 源码里不得出现 dozer 词汇。
if grep -rn "AppSlot\|toast" crates/bytehost-panel/src; then
  echo "bytehost-panel 源码里出现了 AppSlot/toast(应改用 AppKey/Notice):" >&2
  exit 1
fi

echo "bytehost deps check: ok"
