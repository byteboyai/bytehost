#!/usr/bin/env bash
# pre-push 钩子的端到端测试:真实 git push 到本地裸仓库。不联网(用例里没有 byteboyai 的 git 依赖)。
# 运行:tools/shared-deps-guard/test_hook.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

git init -q --bare "$tmp/remote.git"
git init -q -b main "$tmp/work"
cd "$tmp/work"
git config user.name t && git config user.email t@t
mkdir scripts .githooks
cp "$here/check_shared_deps.py" "$here/check-shared-deps.sh" scripts/
cp "$here/pre-push" .githooks/pre-push
chmod +x scripts/check-shared-deps.sh .githooks/pre-push
git config core.hooksPath .githooks
git remote add origin "$tmp/remote.git"

printf '[workspace]\nmembers = []\n' > Cargo.toml
git add -A && git commit -q -m "clean"

fail() { echo "FAIL: $1" >&2; exit 1; }
pushes() { git push -q "$@" >/dev/null 2>&1; }

pushes origin main || fail "干净的 main 应该能推送"

printf '[workspace]\nmembers = []\n\n[patch."https://github.com/byteboyai/bytehost"]\nbytehost-core = { path = "../x" }\n' > Cargo.toml
git commit -qam "dirty: local patch"
pushes origin main && fail "带本地 [patch] 的 main 应该被拦下"
pushes origin HEAD:refs/heads/feature/x || fail "非 main 分支只警告,不应被拦"
git tag v9.9.9
pushes origin v9.9.9 && fail "带本地 [patch] 的 tag 应该被拦下"
pushes origin --delete feature/x || fail "删除远端分支不应触发检查"

git reset -q --hard HEAD~1
git commit -q --allow-empty -m "clean again"
pushes origin main || fail "清理后的 main 应该能推送"

echo "OK: pre-push 钩子行为符合预期"
