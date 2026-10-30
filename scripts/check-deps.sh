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
#     - 允许 iced*/wry(它就是要实现窗口),但**不得**出现 dozer*、digger*、tauri*。
#     - **不**禁 objc2*:它是 wry/winit 在 macOS 上的**传递**平台依赖,禁它等于禁 wry;
#       规格 §67 的 desktop 白名单是 iced/wry/byteui/core,并未禁平台绑定。
#       (tauri* 仍禁:防止重新引入 Tauri 作为可选框架。)
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
  check_forbidden bytehost-desktop '^tauri.*' "tauri 不在共享桌面宿主的依赖白名单(防止重新引入 Tauri)"
else
  echo "note: bytehost-desktop 尚未创建,跳过 desktop 组(Task 07 起自动纳入)" >&2
fi

# 直接依赖白名单:只看 depth 1(传递依赖另用 check_forbidden 控制)。
# 用于表达「这个 crate 只该直接依赖这些东西」的边界。
check_direct_allowed() {
  local pkg="$1" pattern="$2" why="$3"
  local out direct unexpected
  out=$(cargo tree -p "$pkg" --depth 1 -e normal,build --target all --prefix none --all-features 2>&1) || {
    echo "cargo tree 失败:" >&2
    echo "$out" >&2
    exit 1
  }
  direct=$(echo "$out" | awk '{print $1}' | sort -u | grep -v "^${pkg}$" || true)
  unexpected=$(echo "$direct" | grep -Ev "$pattern" || true)
  if [ -n "$unexpected" ]; then
    echo "$pkg 的直接依赖超出了允许范围($why):" >&2
    echo "$unexpected" >&2
    exit 1
  fi
}

# ---- group: chrome ----
# 共享窗口骨架(bytehost-chrome):只理解 PanelId,不理解具体面板与产品。
# 允许 byteui/iced 与平台绑定(objc2/winit 经 iced/byteui 传递引入),
# 但不得依赖产品 crate,也不得依赖 bytehost 应用平台层(apps/client/webview/panel/settings*)。
# panes/hover/layout 已下沉到 core(Tier3 Task3),chrome 不再经 desktop 拖进 webview 执行层。
# objc2 允许:窗口骨架要操作 NSWindow 的交通灯按钮,那是直接使用的平台绑定,不是 wry/winit 的传递依赖。
chrome_direct='^(bytehost-core|byteui|iced_widget|iced_renderer|serde|serde_json|objc2|objc2-app-kit|objc2-foundation|block2|raw-window-handle)$'
check_forbidden bytehost-chrome "$product_banned" "产品 crate,共享窗口骨架不得依赖"
check_forbidden bytehost-chrome '^(tauri.*|wry|winit)$' "webview/窗口循环/tauri,窗口骨架不得依赖"
check_direct_allowed bytehost-chrome "$chrome_direct" "骨架只依赖 bytehost-core、byteui/iced 与平台绑定"

# ---- group: settings ----
# 设置页身份 / 注册表 / 导航状态(bytehost-settings):与 core 同级的纯数据层,
# 依赖闭包只允许 serde 家族,不得出现界面框架或平台库。
settings_allowed='^(bytehost-settings|serde|serde_core|serde_derive|serde_json|proc-macro2|quote|syn|unicode-ident|itoa|memchr|zmij)$'
check_forbidden bytehost-settings "$product_banned" "产品 crate,纯设置状态层不得依赖"
check_forbidden bytehost-settings "$framework_banned" "界面框架/平台 webview,纯设置状态层不得依赖"
settings_tree=$(tree_of bytehost-settings --all-features)
settings_extra=$(echo "$settings_tree" | grep -Ev "$settings_allowed" || true)
if [ -n "$settings_extra" ]; then
  echo "bytehost-settings 的依赖闭包里出现了不在白名单里的 crate(纯设置状态层只允许 serde 家族):" >&2
  echo "$settings_extra" >&2
  exit 1
fi

# ---- group: settings-ui ----
# 共享设置窗口的 iced 视图(bytehost-settings-ui):允许 byteui/iced 与 bytehost 应用类型,
# 但不得依赖产品 crate、wry(它不创建 webview)或 tauri。
settings_ui_banned='^(bytehost-(desktop|chrome|webview)|wry|tauri.*)$'
settings_ui_direct='^(bytehost-apps|bytehost-panel|bytehost-settings|byteui|iced)$'
check_forbidden bytehost-settings-ui "$product_banned" "产品 crate,共享设置视图不得依赖"
check_forbidden bytehost-settings-ui "$settings_ui_banned" "webview 宿主/tauri,共享设置视图不得依赖"
check_direct_allowed bytehost-settings-ui "$settings_ui_direct" "设置视图只依赖应用类型、byteui 与 iced"

# ---- group: pins(共享库钉版)----
# 1) 同一个外部共享库(byteui/bytegit)在所有 crate 里必须钉同一个 tag,Cargo.lock 里只能有一份。
# 2) `--check-latest`(联网,发版前用):钉的 tag 必须是该库远程的最新 `vX.Y.Z`。
#    消费方(dozer/digger)总是跟着最新 byteui;bytehost 里任何一个 crate 落后,消费方依赖图里就会有两份 byteui、
#    类型不兼容、无法编译(v0.4.7 的 `bytehost-chrome` 曾钉在 byteui v0.4.4 上,dozer 不依赖 chrome 所以看不出来)。
check_latest=0
for arg in "$@"; do [ "$arg" = "--check-latest" ] && check_latest=1; done
for lib in byteui bytegit; do
  lines=$(grep -rhE "github.com/byteboyai/${lib}\"" Cargo.toml crates/*/Cargo.toml 2>/dev/null || true)
  [ -z "$lines" ] && continue
  tags=$(echo "$lines" | sed -E 's/.*tag = "([^"]+)".*/\1/' | sort -u)
  if [ "$(echo "$tags" | grep -c .)" -gt 1 ]; then
    echo "各 crate 对 $lib 钉了不同的 tag(必须一致,发版前一起升):" >&2
    grep -rnE "github.com/byteboyai/${lib}\"" Cargo.toml crates/*/Cargo.toml >&2
    exit 1
  fi
  copies=$(grep -c "^name = \"${lib}\"\$" Cargo.lock || true)
  if [ "$copies" -gt 1 ]; then
    echo "Cargo.lock 里有 $copies 份 $lib" >&2
    exit 1
  fi
  if [ "$check_latest" = 1 ]; then
    remote=$(echo "$lines" | head -1 | sed -E 's/.*git = "([^"]+)".*/\1/')
    if ! listing=$(git ls-remote --tags "$remote" 2>/dev/null); then
      echo "note: 联不上 $remote,跳过 $lib 的最新版检查" >&2
      continue
    fi
    latest=$(echo "$listing" | awk '{print $2}' | grep -v '\^{}' | sed 's|refs/tags/||' \
      | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -1)
    if [ -n "$latest" ] && [ "$tags" != "$latest" ]; then
      echo "bytehost 钉的 $lib 是 $tags,但远程最新是 $latest:发版前把所有 crate 的 $lib 钉到 $latest" >&2
      exit 1
    fi
  fi
done

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

  # 注入一个依赖后门禁必须失败;恢复后由最后的整体检查确认。
  # 用法:expect_reject <标签> <目标 Cargo.toml> <探针包名> [依赖行]
  #   - 给了探针包名时,依赖行自动生成为指向探针目录的 path 依赖;
  #   - 不给探针包名时,用第三个参数给出的依赖行(用于直接注入真实 crate)。
  expect_reject() {
    local label="$1" file="$2" pkg_name="$3" dep_line="${4:-}"
    local bak
    if [ -n "$pkg_name" ]; then
      cat > "$probe_dir/Cargo.toml" <<TOMLEOF
[package]
name = "$pkg_name"
version = "0.0.0"
edition = "2021"
publish = false
[workspace]
TOMLEOF
      echo "" > "$probe_dir/src/lib.rs"
      dep_line="$pkg_name = { path = \"$abs_probe\" }"
    fi
    bak="$(mktemp)"
    cp "$file" "$bak"
    python3 - "$file" "$dep_line" <<'PYEOF'
import sys
p, dep = sys.argv[1], sys.argv[2]
s = open(p).read()
s = s.replace("[dependencies]", "[dependencies]\n" + dep, 1)
open(p, "w").write(s)
PYEOF
    if bash "$0" >/dev/null 2>&1; then
      cp "$bak" "$file"
      rm -f "$bak"
      echo "变异测试失败:$label 后门禁仍然通过" >&2
      exit 1
    fi
    cp "$bak" "$file"
    rm -f "$bak"
    echo "  $label 被拦下" >&2
  }

  # 变异 4:chrome 依赖产品 crate(dozer-*)→ 必须失败。
  expect_reject "变异 4:chrome→dozer-*" crates/bytehost-chrome/Cargo.toml dozer-mutation-probe
  # 变异 5:chrome 依赖 tauri → 必须失败。
  expect_reject "变异 5:chrome→tauri-*" crates/bytehost-chrome/Cargo.toml tauri-mutation-probe
  # 变异 6:settings 纯数据层拖入 iced → 必须失败。
  expect_reject "变异 6:settings→iced" crates/bytehost-settings/Cargo.toml "" 'iced = "0.14"'
  # 变异 7:settings-ui 依赖产品 crate(digger-*)→ 必须失败。
  expect_reject "变异 7:settings-ui→digger-*" crates/bytehost-settings-ui/Cargo.toml digger-mutation-probe

  # 变异 3:chrome 的 byteui 钉回旧 tag → `--check-latest` 必须失败(联不上远程时跳过这一条,并说明)。
  # 必须用**真实存在**的旧 tag:不存在的 tag 会让 cargo 拉取失败,那样"失败"并不是这条检查拦下的。
  if git ls-remote --tags https://github.com/byteboyai/byteui >/dev/null 2>&1; then
    ui_bak_dir="$(mktemp -d)"
    lock_bak="$(mktemp)"
    cp Cargo.lock "$lock_bak"
    # 所有钉 byteui 的 Cargo.toml 都要一起改:只改一份会先被「各 crate tag 不一致」拦下,
    # 那样测到的就不是 `--check-latest` 这条规则。
    ui_files=$(grep -rlE 'github.com/byteboyai/byteui"' Cargo.toml crates/*/Cargo.toml)
    for f in $ui_files; do
      mkdir -p "$ui_bak_dir/$(dirname "$f")"
      cp "$f" "$ui_bak_dir/$f"
    done
    trap 'for f in $ui_files; do cp "$ui_bak_dir/$f" "$f"; done; cp "$lock_bak" Cargo.lock; rm -rf "$ui_bak_dir" "$lock_bak"' EXIT
    for f in $ui_files; do
      sed -i.bak -E '/byteboyai\/byteui"/ s/tag = "[^"]+"/tag = "v0.4.4"/' "$f"
      rm -f "$f.bak"
    done
    out=$(bash "$0" --check-latest 2>&1) && {
      echo "变异测试失败:chrome 钉着旧 byteui 后 --check-latest 仍然通过" >&2
      exit 1
    }
    echo "$out" | grep -q "远程最新是" || {
      echo "变异测试失败:失败了,但不是最新版检查拦下的:$out" >&2
      exit 1
    }
    for f in $ui_files; do cp "$ui_bak_dir/$f" "$f"; done
    cp "$lock_bak" Cargo.lock
    trap - EXIT
    rm -rf "$ui_bak_dir" "$lock_bak"
    echo "  变异 3 通过:byteui 钉版落后于远程最新被拦下" >&2
  else
    echo "  变异 3 跳过:联不上 byteui 远程" >&2
  fi

  # 恢复后必须通过。
  cleanup
  trap - EXIT
  if ! bash "$0" >/dev/null 2>&1; then
    echo "变异测试失败:恢复后门禁未通过" >&2
    exit 1
  fi
  echo "门禁变异测试:ok(注入即失败、恢复即通过)" >&2
fi
