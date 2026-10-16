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
