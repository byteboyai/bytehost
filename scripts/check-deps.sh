#!/usr/bin/env bash
# 门禁:bytehost-apps 是要让 Digger 也能用的、与产品无关的 crate。
#  1. 任何 feature 组合下,依赖树里都不得出现 dozer* crate(不依赖 Dozer 的任何东西);
#  2. 默认 feature(只有类型与纯逻辑)的依赖闭包只能是 serde/serde_json 家族——`dozer-core` 依赖它时,
#     `dozer-hook`/`dozer-mcp` 才不会因此多出新 crate。
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

tree() { tree_of bytehost-apps "$@"; }
all=$(tree --all-features)
if echo "$all" | grep -qE '^dozer'; then
  echo "bytehost-apps 不得依赖 dozer* crate,发现:" >&2
  echo "$all" | grep -E '^dozer' >&2
  exit 1
fi

allowed='^(bytehost-apps|serde|serde_core|serde_derive|serde_json|proc-macro2|quote|syn|unicode-ident|itoa|memchr|zmij)$'
extra=$(tree | grep -Ev "$allowed" || true)
if [ -n "$extra" ]; then
  echo "bytehost-apps 默认 feature 的依赖闭包里出现了不在白名单里的 crate(新增依赖请放进 feature):" >&2
  echo "$extra" >&2
  exit 1
fi
# 注意:这里检查的是 `cargo tree -p dozer-hook` **单独**解析出的依赖闭包。发布脚本把 hook 与 dozerd 在同一次
# cargo 调用里构建,feature 会合并(hook 实际是对着带 `server` feature 的 bytehost-apps 编译的),只是链接器会裁掉
# 用不到的代码。所以本检查保证的是"源码层面 hook 不依赖这些 crate",不是"hook 二进制里没有它们的代码"。
# 3. `dozer-core` 依赖 bytehost-apps(默认 feature),`dozer-hook` 又依赖 `dozer-core`:hook 的依赖闭包里除了
#    bytehost-apps 本身不能出现 tokio/hyper/sha2/toml/uuid 这些 server/digest/manifest-toml feature 的依赖。
hook=$(tree_of dozer-hook)
if echo "$hook" | grep -qE '^(tokio|hyper|hyper-util|http-body-util|bytes|sha2|toml|uuid)$'; then
  echo "dozer-hook 的依赖闭包被 bytehost-apps 的可选依赖污染了:" >&2
  echo "$hook" | grep -E '^(tokio|hyper|hyper-util|http-body-util|bytes|sha2|toml|uuid)$' >&2
  exit 1
fi
echo "bytehost-apps deps check: ok"
