#!/usr/bin/env python3
"""shared-deps-guard:byteboyai 共享库依赖守卫(仅标准库)。

正本在 github.com/byteboyai/bytehost 的 tools/shared-deps-guard/,
消费者(Digger、Dozer)把本文件 vendor 到自己的 scripts/。
规格:digger 仓库 docs/superpowers/specs/2026-10-16-shared-deps-guard-design.md
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime
import io
import os
import re
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path
from typing import Protocol

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    sys.exit("check_shared_deps.py 需要 Python 3.11+(tomllib)")

# 正本所在仓库与 vendor 来源 tag。vendor 时把 SOURCE_TAG 改成来源 tag。
CANONICAL_REPO = "bytehost"
SOURCE_TAG = "unreleased"

SKIP_DIRS = {"target", ".git", "node_modules", ".dozer", ".digger", ".cargo"}
DEP_SECTIONS = ("dependencies", "dev-dependencies", "build-dependencies")

BLOCK, WARN, INFO = "block", "warn", "info"


@dataclasses.dataclass(frozen=True)
class Issue:
    level: str
    rule: str
    message: str


@dataclasses.dataclass(frozen=True)
class Dep:
    crate: str  # 实际的 package 名(尊重 `package = "..."`)
    repo: str  # byteboyai 下的仓库名,如 "bytehost"
    ref_kind: str  # "tag" | "rev" | "branch" | "none"
    ref: str
    manifest: str  # 声明它的 Cargo.toml,相对 root
    graph: str = "."  # 所属工作区根目录(相对 root);同一工作区共用一份 Cargo.lock 和依赖图


@dataclasses.dataclass(frozen=True)
class Pin:
    reason: str
    review_by: datetime.date | None


_REPO_RE = re.compile(
    r"^(?:git\+)?https?://github\.com/byteboyai/([^/?#]+?)(?:\.git)?/?(?:[?#].*)?$"
)


def repo_name(url: str) -> str | None:
    """`https://github.com/byteboyai/bytehost(.git)` 之类 -> "bytehost";其他 -> None。"""
    m = _REPO_RE.match(url.strip())
    return m.group(1) if m else None


def find_manifests(root: Path) -> list[Path]:
    found: list[Path] = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        if "Cargo.toml" in filenames:
            found.append(Path(dirpath) / "Cargo.toml")
    return found


def _dep_tables(doc: dict):
    for name in DEP_SECTIONS:
        if isinstance(doc.get(name), dict):
            yield doc[name]
    ws = doc.get("workspace", {})
    if isinstance(ws.get("dependencies"), dict):
        yield ws["dependencies"]
    for cfg in doc.get("target", {}).values():
        for name in DEP_SECTIONS:
            if isinstance(cfg.get(name), dict):
                yield cfg[name]


def _load_all(root: Path) -> list[tuple[Path, dict]]:
    return [(m, tomllib.loads(m.read_text())) for m in find_manifests(root)]


def _graph_of(manifest: Path, root: Path, workspace_dirs: set[Path]) -> str:
    """最近的、声明了 [workspace] 的祖先目录;没有就是自己所在目录(独立包)。"""
    d = manifest.parent
    while True:
        if d in workspace_dirs:
            return str(d.relative_to(root))
        if d == root or d.parent == d:
            return str(manifest.parent.relative_to(root))
        d = d.parent


def collect_deps(root: Path) -> list[Dep]:
    docs = _load_all(root)
    workspace_dirs = {m.parent for m, doc in docs if "workspace" in doc}
    deps: list[Dep] = []
    for manifest, doc in docs:
        rel = str(manifest.relative_to(root))
        graph = _graph_of(manifest, root, workspace_dirs)
        for table in _dep_tables(doc):
            for key, spec in table.items():
                if not isinstance(spec, dict) or "git" not in spec:
                    continue
                repo = repo_name(spec["git"])
                if repo is None:
                    continue
                kind, ref = "none", ""
                for k in ("tag", "rev", "branch"):
                    if k in spec:
                        kind, ref = k, str(spec[k])
                        break
                deps.append(Dep(spec.get("package", key), repo, kind, ref, rel, graph))
    return deps


def find_local_patches(root: Path) -> list[tuple[str, str, str]]:
    """入库 Cargo.toml 里指向本地路径的 byteboyai `[patch]`:(manifest, repo, crate)。"""
    out: list[tuple[str, str, str]] = []
    for manifest, doc in _load_all(root):
        rel = str(manifest.relative_to(root))
        for source, crates in doc.get("patch", {}).items():
            repo = repo_name(source)
            if repo is None or not isinstance(crates, dict):
                continue
            for crate, spec in crates.items():
                if isinstance(spec, dict) and "path" in spec:
                    out.append((rel, repo, crate))
    return out


def load_lock(root: Path) -> list[dict]:
    lock = root / "Cargo.lock"
    if not lock.exists():
        return []
    return tomllib.loads(lock.read_text()).get("package", [])


def load_pins(root: Path) -> dict[str, Pin]:
    path = root / "shared-deps.toml"
    if not path.exists():
        return {}
    pins: dict[str, Pin] = {}
    for name, body in tomllib.loads(path.read_text()).get("pin", {}).items():
        review_by = body.get("review_by")
        if isinstance(review_by, str):
            review_by = datetime.date.fromisoformat(review_by)
        pins[name] = Pin(body.get("reason", ""), review_by)
    return pins


# ---------------------------------------------------------------- 离线规则


def check_local_patch(patches: list[tuple[str, str, str]]) -> list[Issue]:
    grouped: dict[tuple[str, str], list[str]] = {}
    for manifest, repo, crate in patches:
        grouped.setdefault((manifest, repo), []).append(crate)
    return [
        Issue(BLOCK, "local-patch",
              f"{manifest} 的 [patch] 把 {repo} 的 {', '.join(sorted(crates))} 指向本地路径;"
              "联调请改用不入库的 .cargo/config.toml(见 scripts/dev-patch.example.toml)")
        for (manifest, repo), crates in sorted(grouped.items())
    ]


def check_lock(deps: list[Dep], packages: list[dict], lock_path: str = "Cargo.lock") -> list[Issue]:
    shared = {d.crate for d in deps}
    local = sorted({pkg["name"] for pkg in packages
                    if pkg.get("name") in shared and not str(pkg.get("source", "")).startswith("git+")})
    if not local:
        return []
    return [Issue(BLOCK, "lock-not-git",
                  f"{lock_path} 里这些共享库不是来自远端 git(多半被本地 [patch] 覆盖):{', '.join(local)};"
                  f"移除覆盖后执行 cargo update -p {local[0]}(其余同理)还原")]


def check_lock_duplicates(packages: list[dict], lock_path: str = "Cargo.lock") -> list[Issue]:
    """同一个共享 crate 在锁文件里有多个 git 来源:依赖图里有两份,同名类型互不兼容。

    典型成因是传递依赖:本仓库用 bytehost v0.4.1,而依赖的 dozer-panel-* 自己用 v0.4.0。
    只看 Cargo.toml 发现不了,只有 Cargo.lock 里看得到。
    """
    sources: dict[str, set[str]] = {}
    for pkg in packages:
        source = str(pkg.get("source", ""))
        if repo_name(source) is not None:
            sources.setdefault(pkg["name"], set()).add(source)
    issues = []
    for name, found in sorted(sources.items()):
        if len(found) > 1:
            detail = ", ".join(sorted(_short_source(s) for s in found))
            issues.append(Issue(BLOCK, "lock-duplicate",
                                f"{name} 在 {lock_path} 里有 {len(found)} 份:{detail};"
                                "让传递依赖它的库和本仓库对齐到同一个 tag"))
    return issues


def _short_source(source: str) -> str:
    """git+https://github.com/byteboyai/bytehost?tag=v0.4.1#79212c08... -> bytehost?tag=v0.4.1#79212c0"""
    body = source.split("github.com/byteboyai/", 1)[-1]
    head, _, sha = body.partition("#")
    return f"{head}#{sha[:7]}" if sha else head


def check_mismatch(deps: list[Dep]) -> list[Issue]:
    # 只有同一个工作区(同一份 Cargo.lock)里的版本冲突才会拉进两份同名 crate
    by_repo: dict[tuple[str, str], dict[tuple[str, str], set[str]]] = {}
    for d in deps:
        by_repo.setdefault((d.graph, d.repo), {}).setdefault((d.ref_kind, d.ref), set()).add(d.crate)
    issues = []
    for (graph, repo), refs in sorted(by_repo.items()):
        if len(refs) > 1:
            where = "" if graph == "." else f"(工作区 {graph})"
            detail = "; ".join(
                f"{kind}={ref or '(无)'} <- {', '.join(sorted(crates))}"
                for (kind, ref), crates in sorted(refs.items()))
            issues.append(Issue(BLOCK, "ref-mismatch",
                                f"{repo}{where} 在同一依赖图里出现多个版本,会拉进两份同名 crate:{detail}"))
    return issues


def check_pins(deps: list[Dep], pins: dict[str, Pin], today: datetime.date) -> list[Issue]:
    repos = {d.repo for d in deps}
    issues = []
    for name, pin in sorted(pins.items()):
        if name not in repos:
            issues.append(Issue(WARN, "manifest-stale",
                                f"shared-deps.toml 里的 pin.{name} 在任何 Cargo.toml 中都已不存在"))
        elif pin.review_by is not None and today > pin.review_by:
            issues.append(Issue(WARN, "pin-expired",
                                f"{name} 的 pin 已过 review_by({pin.review_by}):{pin.reason}"))
    return issues


# ---------------------------------------------------------------- 远端规则


class Remote(Protocol):
    def refs(self, repo: str) -> dict[str, str] | None:
        """{"refs/tags/v1": sha, "refs/heads/main": sha, ...};无法联网时 None。"""

    def has_commit(self, repo: str, rev: str) -> bool | None:
        """远端是否有该提交;无法确定时 None。"""


_SEMVER_RE = re.compile(r"^v?(\d+)\.(\d+)\.(\d+)$")


def parse_semver(tag: str) -> tuple[int, int, int] | None:
    m = _SEMVER_RE.match(tag)
    return (int(m[1]), int(m[2]), int(m[3])) if m else None


def latest_semver_tag(refs: dict[str, str]) -> str | None:
    best: tuple[tuple[int, int, int], str] | None = None
    for ref in refs:
        if not ref.startswith("refs/tags/") or ref.endswith("^{}"):
            continue
        tag = ref[len("refs/tags/"):]
        ver = parse_semver(tag)
        if ver is not None and (best is None or ver > best[0]):
            best = (ver, tag)
    return best[1] if best else None


def describe_gap(cur: tuple[int, int, int], latest: tuple[int, int, int]) -> str:
    if latest[0] != cur[0]:
        return f"{latest[0] - cur[0]} 个 major"
    if latest[1] != cur[1]:
        return f"{latest[1] - cur[1]} 个 minor"
    return f"{latest[2] - cur[2]} 个 patch"


def check_remote(deps: list[Dep], pins: dict[str, Pin], remote: Remote) -> list[Issue]:
    issues: list[Issue] = []
    by_repo: dict[str, set[tuple[str, str]]] = {}
    for d in deps:
        by_repo.setdefault(d.repo, set()).add((d.ref_kind, d.ref))

    for repo, wanted in sorted(by_repo.items()):
        refs = remote.refs(repo)
        if refs is None:
            issues.append(Issue(WARN, "ref-unverified",
                                f"无法联网查询 {repo} 的远端引用,本次未验证其 tag/rev 是否存在"))
            continue
        shas = set(refs.values())
        for kind, ref in sorted(wanted):
            if kind == "tag" and f"refs/tags/{ref}" not in refs:
                issues.append(Issue(BLOCK, "ref-missing", f"{repo} 的 tag {ref} 在远端不存在,先推送再依赖"))
            elif kind == "branch" and f"refs/heads/{ref}" not in refs:
                issues.append(Issue(BLOCK, "ref-missing", f"{repo} 的分支 {ref} 在远端不存在,先推送再依赖"))
            elif kind == "rev" and not (len(ref) >= 7 and any(s.startswith(ref) for s in shas)):
                found = remote.has_commit(repo, ref)
                if found is False:
                    issues.append(Issue(BLOCK, "ref-missing",
                                        f"{repo} 的 rev {ref[:12]} 在远端不存在,先推送再依赖"))
                elif found is None:
                    issues.append(Issue(WARN, "ref-unverified",
                                        f"无法确认 {repo} 的 rev {ref[:12]} 是否已在远端"))

        # 落后:只比较语义化 tag;rev 与 `libs-2026-10-16` 这类非语义化 tag 只查存在性
        if repo in pins:
            continue
        current = [(parse_semver(ref), ref) for kind, ref in wanted
                   if kind == "tag" and parse_semver(ref) is not None]
        latest_tag = latest_semver_tag(refs)
        if current and latest_tag:
            cur_ver, cur_tag = max(current)
            latest_ver = parse_semver(latest_tag)
            if latest_ver > cur_ver:
                issues.append(Issue(WARN, "behind",
                                    f"{repo} {cur_tag} -> 最新 {latest_tag}"
                                    f"(落后 {describe_gap(cur_ver, latest_ver)})"))
    return issues


def check_guard_source(remote: Remote, source_tag: str = SOURCE_TAG) -> list[Issue]:
    """提示 vendor 的本脚本可能不是正本最新版(只能提示,不会自动更新)。"""
    current = parse_semver(source_tag)
    refs = remote.refs(CANONICAL_REPO)
    if current is None or refs is None:
        return []
    latest = latest_semver_tag(refs)
    if latest and parse_semver(latest) > current:
        return [Issue(INFO, "guard-source",
                      f"本脚本 vendor 自 {CANONICAL_REPO} {source_tag},现在有 {latest};"
                      "如果 tools/shared-deps-guard 有更新,请重新 vendor")]
    return []


# ---------------------------------------------------------------- 联网实现


def parse_ls_remote(output: str) -> dict[str, str]:
    refs: dict[str, str] = {}
    for line in output.splitlines():
        sha, _, ref = line.partition("\t")
        if ref:
            refs[ref] = sha
    return refs


_MISSING_MARKERS = ("not our ref", "couldn't find remote ref", "no such remote ref")


def classify_fetch_failure(stderr: str) -> bool | None:
    """git fetch 失败的原因:明确"远端没有该提交" -> False;其他(网络、鉴权) -> None。"""
    low = stderr.lower()
    return False if any(m in low for m in _MISSING_MARKERS) else None


class GitRemote:
    """用 `git ls-remote` / `git fetch` 查询 github.com/byteboyai/<repo>。"""

    def __init__(self, timeout: int = 30):
        self.timeout = timeout
        self._cache: dict[str, dict[str, str] | None] = {}
        self._env = {**os.environ, "GIT_TERMINAL_PROMPT": "0"}

    @staticmethod
    def url(repo: str) -> str:
        return f"https://github.com/byteboyai/{repo}.git"

    def _git(self, args: list[str], cwd: str | None = None) -> subprocess.CompletedProcess | None:
        try:
            return subprocess.run(["git", *args], cwd=cwd, env=self._env, capture_output=True,
                                  text=True, timeout=self.timeout)
        except (OSError, subprocess.TimeoutExpired):
            return None

    def refs(self, repo: str) -> dict[str, str] | None:
        if repo not in self._cache:
            proc = self._git(["ls-remote", "--tags", "--heads", self.url(repo)])
            ok = proc is not None and proc.returncode == 0
            self._cache[repo] = parse_ls_remote(proc.stdout) if ok else None
        return self._cache[repo]

    def has_commit(self, repo: str, rev: str) -> bool | None:
        if len(rev) != 40:  # 远端只按完整 sha 取提交;短 rev 无法确认
            return None
        with tempfile.TemporaryDirectory() as tmp:
            if self._git(["init", "--bare", "-q", tmp]) is None:
                return None
            # tree:0 只取提交对象本身:Dozer 这样的大仓库从 15 秒降到 1.5 秒
            proc = self._git(["fetch", "--depth=1", "--filter=tree:0", "-q", self.url(repo), rev], cwd=tmp)
        if proc is None:
            return None
        if proc.returncode == 0:
            return True
        return classify_fetch_failure(proc.stderr)


def ahead_of_remote(root: Path, branch: str = "main") -> Issue | None:
    """本地 branch 领先 origin/branch 的提交数(只用本地引用,不联网)。"""
    try:
        proc = subprocess.run(
            ["git", "-C", str(root), "rev-list", "--count", f"origin/{branch}..{branch}"],
            capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if proc.returncode != 0 or not proc.stdout.strip().isdigit() or int(proc.stdout) == 0:
        return None
    return Issue(INFO, "ahead-of-remote",
                 f"本地 {branch} 领先 origin/{branch} {int(proc.stdout)} 个提交;"
                 "被其他仓库依赖的提交要先推送,否则对方会引用到远端不存在的 rev")


# ---------------------------------------------------------------- 入口

_ORDER = {BLOCK: 0, WARN: 1, INFO: 2}


def downgrade_blocks(issues: list[Issue]) -> list[Issue]:
    return [Issue(WARN, i.rule, i.message + "(推送目标不是 main/tag,阻断降级为警告)")
            if i.level == BLOCK else i for i in issues]


def render(issues: list[Issue]) -> str:
    lines = [f"{i.level.upper():5}  {i.rule:16}  {i.message}"
             for i in sorted(issues, key=lambda i: _ORDER[i.level])]
    blocks = sum(i.level == BLOCK for i in issues)
    warns = sum(i.level == WARN for i in issues)
    lines.append(f"shared-deps: {blocks} 个阻断, {warns} 个警告" if issues else "shared-deps: 没有发现问题")
    return "\n".join(lines)


WANTED_FILES = {"Cargo.toml", "Cargo.lock", "shared-deps.toml"}


def export_rev(repo: Path, rev: str, dest: Path) -> bool:
    """把某个提交里的 Cargo.toml / Cargo.lock / shared-deps.toml 取到 dest,用来检查"将被推送的内容"。"""
    ls = subprocess.run(["git", "-C", str(repo), "ls-tree", "-r", "--name-only", "-z", rev],
                        capture_output=True, text=True)
    if ls.returncode != 0:
        return False
    paths = [p for p in ls.stdout.split("\0") if p and p.rsplit("/", 1)[-1] in WANTED_FILES]
    if not paths:
        return True
    arc = subprocess.run(["git", "-C", str(repo), "archive", rev, "--", *paths], capture_output=True)
    if arc.returncode != 0:
        return False
    with tarfile.open(fileobj=io.BytesIO(arc.stdout)) as tf:
        try:
            tf.extractall(dest, filter="data")
        except TypeError:  # Python < 3.11.4 没有 filter 参数
            tf.extractall(dest)
    return True


def run(root: Path, remote: Remote | None, today: datetime.date,
        git_root: Path | None = None) -> list[Issue]:
    deps = collect_deps(root)
    pins = load_pins(root)
    issues = [
        *check_local_patch(find_local_patches(root)),
        *check_mismatch(deps),
        *check_pins(deps, pins, today),
    ]
    for graph in sorted({d.graph for d in deps}):
        lock_path = "Cargo.lock" if graph == "." else f"{graph}/Cargo.lock"
        packages = load_lock(root / graph)
        issues += check_lock([d for d in deps if d.graph == graph], packages, lock_path)
        issues += check_lock_duplicates(packages, lock_path)
    if remote is not None:
        issues += check_remote(deps, pins, remote)
        issues += check_guard_source(remote)
    ahead = ahead_of_remote(git_root or root)
    if ahead is not None:
        issues.append(ahead)
    return issues


def main(argv: list[str] | None = None, remote: Remote | None = None,
         today: datetime.date | None = None) -> int:
    parser = argparse.ArgumentParser(description="byteboyai 共享库依赖守卫")
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--rev", help="检查该提交里的内容,而不是工作区(pre-push 用它检查将被推送的提交)")
    parser.add_argument("--offline", action="store_true", help="跳过所有联网检查")
    parser.add_argument("--downgrade-blocks", action="store_true",
                        help="把阻断降级为警告(推送非 main/tag 时由 pre-push 使用)")
    args = parser.parse_args(argv)

    if remote is None and not args.offline:
        remote = GitRemote()
    root = args.root.resolve()
    remote = None if args.offline else remote
    today = today or datetime.date.today()
    if args.rev:
        with tempfile.TemporaryDirectory() as tmp:
            if not export_rev(root, args.rev, Path(tmp)):
                print(f"shared-deps: 无法读取提交 {args.rev}", file=sys.stderr)
                return 2
            issues = run(Path(tmp), remote, today, git_root=root)
    else:
        issues = run(root, remote, today)
    if args.downgrade_blocks:
        issues = downgrade_blocks(issues)
    print(render(issues))
    return 1 if any(i.level == BLOCK for i in issues) else 0


if __name__ == "__main__":
    sys.exit(main())
