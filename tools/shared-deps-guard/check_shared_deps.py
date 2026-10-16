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
