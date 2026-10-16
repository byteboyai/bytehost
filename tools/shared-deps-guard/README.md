# shared-deps-guard

byteboyai 共享库(bytehost、byteui、bytegit、dozer 的共享 crate)的消费者依赖守卫。
规格:digger 仓库 `docs/superpowers/specs/2026-10-16-shared-deps-guard-design.md`。

**这里是正本。** 消费者(Digger、Dozer)把下面四个文件 vendor 到自己的仓库,不要在消费者里直接改它们。

| 正本文件 | 消费者里的位置 |
|---|---|
| `check_shared_deps.py` | `scripts/check_shared_deps.py` |
| `check-shared-deps.sh` | `scripts/check-shared-deps.sh` |
| `dev-patch.example.toml` | `scripts/dev-patch.example.toml` |
| `pre-push` | `.githooks/pre-push` |

## 它检查什么

| 规则 | 级别 | 含义 |
|---|---|---|
| `local-patch` | block | 入库的 `Cargo.toml` 里有指向本地路径的 `[patch]` |
| `lock-not-git` | block | `Cargo.lock` 里共享库不是来自远端 git(被本地 patch 覆盖) |
| `lock-duplicate` | block | 同一共享 crate 在 `Cargo.lock` 里有多个 git 来源(传递依赖版本不一致) |
| `ref-mismatch` | block | 同一工作区内同一共享库的 tag/rev 不一致 |
| `ref-missing` | block | 固定的 tag / 分支 / rev 在远端明确不存在 |
| `behind` | warn | 没有 pin 的共享库落后于远端最新语义化 tag |
| `pin-expired` / `manifest-stale` | warn | `shared-deps.toml` 的 pin 已过期 / 已无对应依赖 |
| `ref-unverified` | warn | 联网失败或短 rev,无法确认(放行) |
| `ahead-of-remote` / `guard-source` | info | 本地 main 领先远端 / 正本可能有新版 |

版本号只在各 `Cargo.toml` 里写一份;`shared-deps.toml` 只记录刻意固定的理由:

```toml
[pin.dozer]
reason = "等一次性同步所有共享面板"
review_by = 2026-11-15
```

## 用法

```bash
scripts/check-shared-deps.sh                      # 检查工作区,完整报告
scripts/check-shared-deps.sh --rev HEAD           # 检查已提交内容
scripts/check-shared-deps.sh --offline            # 跳过联网检查
scripts/check-shared-deps.sh --downgrade-blocks   # 阻断降级为警告
git config core.hooksPath .githooks               # 启用 pre-push(每个克隆一次)
```

退出码:`0` 没有阻断;`1` 有阻断;`3` 脚本自己跑不起来(没有 Python 3.11+、读不到提交、脚本出错)。
钩子对 `3` 的处理:推 main/tag 时仍然拦下(fail-closed),推其他分支只警告;提示里会说明这是环境问题而不是依赖问题。
包装脚本会按 `python3.13`、`python3.12`、`python3.11`、`python3` 的顺序找 >= 3.11 的解释器(GUI git 客户端的 PATH 往往只有 macOS 自带的 3.9)。

pre-push 对每个被推送的引用检查它的提交:推 `main` 或 tag 时阻断,推其他分支只警告。
联网失败只警告并放行。确认要绕过时:`git push --no-verify`。

本地联调共享库:把 `dev-patch.example.toml` 复制成 `.cargo/config.toml`(应被 `.gitignore`),
不要把 `[patch]` 写进入库的 `Cargo.toml`。

## vendor 到消费者

在消费者仓库根目录,`TAG` 是发布了本目录的 bytehost tag,`SRC` 指向本地 bytehost 克隆:

```bash
TAG=v0.4.2; SRC=../bytehost
for f in check_shared_deps.py check-shared-deps.sh dev-patch.example.toml; do
  git -C $SRC show "${TAG}:tools/shared-deps-guard/$f" > "scripts/$f"
done
mkdir -p .githooks && git -C $SRC show "${TAG}:tools/shared-deps-guard/pre-push" > .githooks/pre-push
perl -pi -e "s/^SOURCE_TAG = \"unreleased\"/SOURCE_TAG = \"$TAG\"/" scripts/check_shared_deps.py
chmod +x scripts/check-shared-deps.sh .githooks/pre-push
```

## 测试

```bash
python3 -m unittest discover -s tools/shared-deps-guard -p 'test_*.py' -v   # 单元测试,不联网
tools/shared-deps-guard/test_hook.sh                                        # 钩子端到端,真实 git push 到本地裸仓库
```
